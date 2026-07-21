//! Tolerant NDJSON parsers that normalize each provider's native event stream
//! into [`AgentEvent`]s.
//!
//! The Claude `control_request`/`control_response` wire schema is only
//! partially documented, so these parsers are deliberately defensive: unknown
//! event types are ignored, and missing fields fall back to sensible defaults
//! rather than erroring. Each line maps to zero or more events (one assistant
//! message can carry both text and a tool call).

use serde_json::Value;

use crate::agent::events::AgentEvent;
use crate::domain::model::Usage;
use crate::error::Result;

/// Parse one line of `claude --output-format stream-json`.
pub fn parse_claude_line(line: &str) -> Result<Vec<AgentEvent>> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(vec![]);
    }
    let v: Value = serde_json::from_str(line)?;
    let mut out = Vec::new();

    match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
            if let Some(sid) = v.get("session_id").and_then(Value::as_str) {
                out.push(AgentEvent::Started {
                    session_id: sid.to_string(),
                });
            }
        }
        Some("assistant") => {
            if let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                if !text.trim().is_empty() {
                                    out.push(AgentEvent::Progress {
                                        text: text.to_string(),
                                    });
                                }
                            }
                        }
                        Some("tool_use") => {
                            let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                            match name {
                                "ExitPlanMode" => {
                                    let plan = block
                                        .pointer("/input/plan")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string();
                                    out.push(AgentEvent::PlanReady { plan });
                                }
                                "AskUserQuestion" => {
                                    let request_id = block
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or("ask")
                                        .to_string();
                                    let (question, options) = extract_question(block);
                                    out.push(AgentEvent::NeedsInput {
                                        request_id,
                                        question,
                                        options,
                                    });
                                }
                                other => out.push(AgentEvent::Progress {
                                    text: format!("→ {other}"),
                                }),
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Some("result") => {
            let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let result = v
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if is_error {
                out.push(AgentEvent::Error {
                    message: if result.is_empty() {
                        "run failed".into()
                    } else {
                        result
                    },
                });
            } else {
                out.push(AgentEvent::Done {
                    result,
                    cost_usd: v
                        .get("total_cost_usd")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                    usage: parse_usage(v.get("usage")),
                });
            }
        }
        Some("control_request") => {
            // Permission / plan-exit request — needs the user. Schema is
            // under-documented; extract a best-effort id and a generic prompt.
            let request_id = v
                .get("request_id")
                .and_then(Value::as_str)
                .or_else(|| v.get("id").and_then(Value::as_str))
                .unwrap_or("control")
                .to_string();
            out.push(AgentEvent::NeedsInput {
                request_id,
                question: "The agent is requesting permission to proceed.".into(),
                options: vec!["Allow".into(), "Deny".into()],
            });
        }
        _ => {}
    }

    Ok(out)
}

/// If `line` is an assistant message whose content includes an `ExitPlanMode`
/// tool call, return that call's `tool_use` id (empty string if the id field is
/// absent). The id correlates the call with the later `tool_result` that reports
/// whether the plan was approved or rejected — see [`errored_tool_result_ids`].
/// `None` for any line that isn't an assistant message calling `ExitPlanMode`.
///
/// A headless plan run (`--permission-mode plan`) has no interactive approver,
/// so the CLI *rejects* `ExitPlanMode` and the agent re-presents its real plan —
/// including any `usine-questions` block — as the run's *final result*. The
/// Claude pump uses this to avoid trusting the pre-empted tool draft (which can
/// be an earlier, question-less version of the plan) over that final result.
pub(crate) fn exit_plan_mode_tool_id(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    v.pointer("/message/content")
        .and_then(Value::as_array)?
        .iter()
        .find(|b| {
            b.get("type").and_then(Value::as_str) == Some("tool_use")
                && b.get("name").and_then(Value::as_str) == Some("ExitPlanMode")
        })
        .map(|b| {
            b.get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
}

/// Return the `tool_use_id`s of any **errored** `tool_result` blocks carried by
/// `line` (a `user` message). An errored `ExitPlanMode` result is how a headless
/// plan run signals "plan rejected"; see [`exit_plan_mode_tool_id`]. Empty for
/// any line that isn't a user message carrying at least one errored result.
pub(crate) fn errored_tool_result_ids(line: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    if v.get("type").and_then(Value::as_str) != Some("user") {
        return Vec::new();
    }
    v.pointer("/message/content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| {
                    b.get("type").and_then(Value::as_str) == Some("tool_result")
                        && b.get("is_error").and_then(Value::as_bool) == Some(true)
                })
                .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Parse one line of `codex exec --json`. Best-effort: the Codex JSON surface is
/// newer and less stable, so unknown events are ignored.
pub fn parse_codex_line(line: &str) -> Result<Vec<AgentEvent>> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(vec![]);
    }
    let v: Value = serde_json::from_str(line)?;
    let mut out = Vec::new();

    match v.get("type").and_then(Value::as_str) {
        Some("thread.started") => {
            if let Some(id) = v.get("thread_id").and_then(Value::as_str) {
                out.push(AgentEvent::Started {
                    session_id: id.to_string(),
                });
            }
        }
        // Only the *completed* item carries the final text; `item.started` and
        // `item.updated` repeat the same item and would duplicate transcript
        // lines, so they're ignored for text items.
        Some("item.completed") => {
            if let Some(text) = v
                .pointer("/item/text")
                .and_then(Value::as_str)
                .or_else(|| v.pointer("/item/message").and_then(Value::as_str))
            {
                if !text.trim().is_empty() {
                    out.push(AgentEvent::Progress {
                        text: text.to_string(),
                    });
                }
            }
        }
        // A command *starting* is the only signal during long tool work —
        // command items carry no `text`, so without this the transcript stays
        // blank and nothing feeds the executor's idle watchdog while codex
        // grinds through builds/tests. Surfaced at `item.started` (not
        // completed) so a long command resets the watchdog when it begins.
        Some("item.started")
            if v.pointer("/item/type").and_then(Value::as_str) == Some("command_execution") =>
        {
            if let Some(cmd) = v.pointer("/item/command").and_then(Value::as_str) {
                let cmd = cmd.trim();
                if !cmd.is_empty() {
                    // Keep one transcript line per command, however long.
                    let short: String = cmd.chars().take(160).collect();
                    let ellipsis = if short.len() < cmd.len() { "…" } else { "" };
                    out.push(AgentEvent::Progress {
                        text: format!("→ {short}{ellipsis}"),
                    });
                }
            }
        }
        Some("turn.completed") => out.push(AgentEvent::Done {
            result: String::new(),
            cost_usd: 0.0,
            usage: parse_usage(v.get("usage")),
        }),
        Some("turn.failed") | Some("error") => out.push(AgentEvent::Error {
            message: v
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| v.get("message").and_then(Value::as_str))
                .unwrap_or("codex run failed")
                .to_string(),
        }),
        _ => {}
    }

    Ok(out)
}

/// If `line` is a completed Codex `agent_message` item, return its text.
///
/// The codex pump tracks the run's final message with this — NOT via the
/// `Progress` stream, which reasoning summaries and command lines also feed —
/// because Codex's terminal `turn.completed` event carries no text: the last
/// agent message substitutes for it, and everything parsed out of a run's
/// result (plan fallback, review/triage verdicts, commit-message and handoff
/// blocks) depends on it being the *message*, not a trailing reasoning blob.
pub(crate) fn codex_agent_message(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("item.completed")
        || v.pointer("/item/type").and_then(Value::as_str) != Some("agent_message")
    {
        return None;
    }
    v.pointer("/item/text")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/item/message").and_then(Value::as_str))
        .map(str::to_string)
}

fn extract_question(block: &Value) -> (String, Vec<String>) {
    let input = block.get("input");
    let question = input
        .and_then(|i| i.get("question"))
        .and_then(Value::as_str)
        .or_else(|| {
            input
                .and_then(|i| i.pointer("/questions/0/question"))
                .and_then(Value::as_str)
        })
        .unwrap_or("The agent has a question.")
        .to_string();
    let options = input
        .and_then(|i| i.pointer("/questions/0/options"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|o| {
                    o.get("label")
                        .and_then(Value::as_str)
                        .or_else(|| o.as_str())
                })
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    (question, options)
}

fn parse_usage(v: Option<&Value>) -> Usage {
    let Some(v) = v else {
        return Usage::default();
    };
    Usage {
        input_tokens: v.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
        output_tokens: v.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_system_init_yields_started() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc-123","model":"opus"}"#;
        let evts = parse_claude_line(line).unwrap();
        assert!(matches!(&evts[0], AgentEvent::Started { session_id } if session_id == "abc-123"));
    }

    #[test]
    fn claude_assistant_text_and_exit_plan() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"Here is the plan"},
            {"type":"tool_use","name":"ExitPlanMode","input":{"plan":"step 1"}}
        ]}}"#;
        let evts = parse_claude_line(line).unwrap();
        assert!(matches!(&evts[0], AgentEvent::Progress { text } if text == "Here is the plan"));
        assert!(matches!(&evts[1], AgentEvent::PlanReady { plan } if plan == "step 1"));
    }

    #[test]
    fn claude_result_success_reports_cost() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.5,"usage":{"input_tokens":100,"output_tokens":50}}"#;
        let evts = parse_claude_line(line).unwrap();
        match &evts[0] {
            AgentEvent::Done {
                result,
                cost_usd,
                usage,
            } => {
                assert_eq!(result, "done");
                assert_eq!(*cost_usd, 0.5);
                assert_eq!(usage.input_tokens, 100);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn codex_item_message_yields_progress() {
        let line =
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"working on it"}}"#;
        let evts = parse_codex_line(line).unwrap();
        assert!(matches!(&evts[0], AgentEvent::Progress { text } if text == "working on it"));
    }

    #[test]
    fn codex_command_start_yields_arrow_progress_once() {
        let started = r#"{"type":"item.started","item":{"type":"command_execution","command":"cargo test -p usine-core"}}"#;
        let evts = parse_codex_line(started).unwrap();
        assert!(
            matches!(&evts[0], AgentEvent::Progress { text } if text == "→ cargo test -p usine-core")
        );
        // The completed command item is silent — no duplicate transcript line.
        let completed = r#"{"type":"item.completed","item":{"type":"command_execution","command":"cargo test -p usine-core","exit_code":0,"aggregated_output":"ok"}}"#;
        assert!(parse_codex_line(completed).unwrap().is_empty());
        // Non-command started items stay silent (their completed form speaks).
        let msg_started =
            r#"{"type":"item.started","item":{"type":"agent_message","text":"thinking"}}"#;
        assert!(parse_codex_line(msg_started).unwrap().is_empty());
    }

    #[test]
    fn codex_agent_message_ignores_reasoning_and_commands() {
        assert_eq!(
            codex_agent_message(
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"the plan"}}"#
            )
            .as_deref(),
            Some("the plan")
        );
        // Reasoning summaries also carry `text` but must not become the result.
        assert_eq!(
            codex_agent_message(
                r#"{"type":"item.completed","item":{"type":"reasoning","text":"I should…"}}"#
            ),
            None
        );
        // A started (not completed) message item doesn't count yet.
        assert_eq!(
            codex_agent_message(
                r#"{"type":"item.started","item":{"type":"agent_message","text":"draft"}}"#
            ),
            None
        );
        assert_eq!(codex_agent_message("not json"), None);
    }

    #[test]
    fn unknown_lines_are_ignored() {
        assert!(parse_claude_line(r#"{"type":"user"}"#).unwrap().is_empty());
        assert!(parse_claude_line("").unwrap().is_empty());
    }

    #[test]
    fn exit_plan_mode_tool_id_extracts_call_id() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"here's the plan"},
            {"type":"tool_use","id":"toolu_ABC","name":"ExitPlanMode","input":{"plan":"do it"}}
        ]}}"#;
        assert_eq!(exit_plan_mode_tool_id(line).as_deref(), Some("toolu_ABC"));
        // A different tool, a plain text turn, and a user line all yield nothing.
        assert_eq!(
            exit_plan_mode_tool_id(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Bash","input":{}}]}}"#
            ),
            None
        );
        assert_eq!(
            exit_plan_mode_tool_id(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#
            ),
            None
        );
        assert_eq!(exit_plan_mode_tool_id(r#"{"type":"user"}"#), None);
    }

    #[test]
    fn errored_tool_result_ids_only_flags_errors() {
        let rejected = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"toolu_ABC","is_error":true,"content":"rejected"}
        ]}}"#;
        assert_eq!(
            errored_tool_result_ids(rejected),
            vec!["toolu_ABC".to_string()]
        );
        // An approved result (is_error absent) is not flagged.
        let approved = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"toolu_ABC","content":"approved"}
        ]}}"#;
        assert!(errored_tool_result_ids(approved).is_empty());
        assert!(errored_tool_result_ids(r#"{"type":"assistant"}"#).is_empty());
    }
}
