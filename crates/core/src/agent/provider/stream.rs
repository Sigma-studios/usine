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
use crate::domain::session::{BgTaskKind, BgTaskStatus, TaskEvent};
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
        Some("system") => {
            if let Some(task) = parse_task(&v) {
                out.push(AgentEvent::Task(task));
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
                        Some("thinking") => {
                            if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                                if !text.trim().is_empty() {
                                    out.push(AgentEvent::Progress {
                                        text: format!("{THINKING_PROGRESS_PREFIX}{}", text.trim()),
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
                                "TodoWrite" => out.push(AgentEvent::Progress {
                                    text: match current_todo(block) {
                                        Some(item) => format!("{TODO_PROGRESS_PREFIX}{item}"),
                                        None => "→ TodoWrite".into(),
                                    },
                                }),
                                other => out.push(AgentEvent::Progress {
                                    text: match block
                                        .get("input")
                                        .and_then(|i| tool_brief(other, i))
                                    {
                                        Some(brief) => format!("→ {other} {brief}"),
                                        None => format!("→ {other}"),
                                    },
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

/// A `system/task_*` line: the CLI's account of a background task (subagent
/// or command). Shapes verified on CLI 2.1.307 (see the fixtures under
/// `tests/fixtures/claude_task_*.ndjson`).
fn parse_task(v: &Value) -> Option<TaskEvent> {
    let str_of = |key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let id = str_of("task_id")?;
    let tool_uses = v.pointer("/usage/tool_uses").and_then(Value::as_u64);
    match v.get("subtype").and_then(Value::as_str)? {
        "task_started" => Some(TaskEvent::Started {
            id,
            parent: str_of("parent_task_id"),
            kind: match v.get("task_type").and_then(Value::as_str) {
                Some("local_agent") | Some("remote_agent") => BgTaskKind::Agent,
                Some("local_bash") => BgTaskKind::Command,
                _ => BgTaskKind::Other,
            },
            description: str_of("description").unwrap_or_else(|| "background task".into()),
            subagent_type: str_of("subagent_type"),
            backgrounded: v
                .get("is_backgrounded")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        }),
        "task_progress" => Some(TaskEvent::Progress {
            id,
            last_tool: str_of("last_tool_name"),
            // `description` is the running activity ("Running cargo test");
            // `summary`, when present, the agent's own recap.
            summary: str_of("summary").or_else(|| str_of("description")),
            tool_uses,
        }),
        "task_updated" => Some(TaskEvent::Status {
            id,
            status: task_status(v.pointer("/patch/status").and_then(Value::as_str)?)?,
            reason: v
                .pointer("/patch/reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            summary: None,
            tool_uses: None,
        }),
        "task_notification" => Some(TaskEvent::Status {
            id,
            status: task_status(v.get("status").and_then(Value::as_str)?)?,
            reason: str_of("reason"),
            summary: str_of("summary"),
            tool_uses,
        }),
        _ => None,
    }
}

/// A task status word as the CLI writes it; `None` for a still-live one.
fn task_status(s: &str) -> Option<BgTaskStatus> {
    match s {
        "completed" => Some(BgTaskStatus::Completed),
        "failed" | "error" => Some(BgTaskStatus::Failed),
        "stopped" | "killed" | "cancelled" => Some(BgTaskStatus::Stopped),
        _ => None,
    }
}

/// The one input field that says what a tool call does — the command, the
/// file, the pattern — first line only, cut to 120 chars. `None` for tools
/// with no such field, which then log as their bare name.
fn tool_brief(name: &str, input: &Value) -> Option<String> {
    let field = match name {
        "Bash" => "command",
        "Read" | "Edit" | "MultiEdit" | "Write" => "file_path",
        "NotebookEdit" => "notebook_path",
        "Grep" | "Glob" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Task" | "Agent" => "description",
        _ => return None,
    };
    let text = input.get(field).and_then(Value::as_str)?.trim();
    let first = text.lines().next().unwrap_or("").trim();
    (!first.is_empty()).then(|| crate::domain::session::truncate_chars(first, 120))
}

/// How a `TodoWrite` call's progress line starts when it names the item in
/// progress; sessions fall back to that item as their status note.
pub const TODO_PROGRESS_PREFIX: &str = "→ TodoWrite: ";

/// How a progress line carrying Claude's visible thinking starts; it is kept
/// in transcripts but never becomes a status note.
pub const THINKING_PROGRESS_PREFIX: &str = "∴ ";

/// The in-progress item of a `TodoWrite` call (its `activeForm`, else its
/// `content`).
fn current_todo(block: &Value) -> Option<String> {
    let todos = block.pointer("/input/todos")?.as_array()?;
    let item = todos
        .iter()
        .find(|t| t.get("status").and_then(Value::as_str) == Some("in_progress"))?;
    ["activeForm", "content"]
        .iter()
        .filter_map(|k| item.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

/// The text of a user message the CLI echoed back from stdin
/// (`--replay-user-messages`): it marks the moment the agent took that message.
/// `None` for anything else, tool results included.
pub(crate) fn replayed_user_text(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("user")
        || v.get("isReplay").and_then(Value::as_bool) != Some(true)
    {
        return None;
    }
    let content = v.pointer("/message/content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let text: Vec<&str> = content
        .as_array()?
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    Some(text.join("\n"))
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

    fn tool_line(name: &str, input: &str) -> String {
        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"{name}","input":{input}}}]}}}}"#
        );
        match &parse_claude_line(&line).unwrap()[0] {
            AgentEvent::Progress { text } => text.clone(),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn tool_calls_name_what_they_act_on() {
        assert_eq!(
            tool_line("Bash", r#"{"command":"cargo test\necho done"}"#),
            "→ Bash cargo test"
        );
        assert_eq!(
            tool_line("Read", r#"{"file_path":"/repo/src/main.rs"}"#),
            "→ Read /repo/src/main.rs"
        );
        assert_eq!(tool_line("Frobnicate", "{}"), "→ Frobnicate");
        assert_eq!(tool_line("Bash", "{}"), "→ Bash");
    }

    #[test]
    fn todo_write_names_the_item_in_progress() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"TodoWrite","input":{"todos":[{"content":"Read","status":"completed","activeForm":"Reading"},{"content":"Fix the parser","status":"in_progress","activeForm":"Fixing the parser"}]}}]}}"#;
        let evts = parse_claude_line(line).unwrap();
        assert!(
            matches!(&evts[0], AgentEvent::Progress { text } if text == "→ TodoWrite: Fixing the parser")
        );
        let none = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"TodoWrite","input":{"todos":[]}}]}}"#;
        assert!(
            matches!(&parse_claude_line(none).unwrap()[0], AgentEvent::Progress { text } if text == "→ TodoWrite")
        );
    }

    #[test]
    fn only_echoed_user_messages_count_as_replays() {
        let echo = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"say hi"}]},"isReplay":true}"#;
        assert_eq!(replayed_user_text(echo).as_deref(), Some("say hi"));
        let tool = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"a"}]}}"#;
        assert_eq!(replayed_user_text(tool), None);
        assert!(parse_claude_line(echo).unwrap().is_empty());
    }

    #[test]
    fn claude_thinking_with_text_is_a_prefixed_progress_line() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"thinking","thinking":"  The parser drops it.\n","signature":"x"},
            {"type":"thinking","thinking":"","signature":"y"},
            {"type":"redacted_thinking","data":"z"}
        ]}}"#;
        let evts = parse_claude_line(line).unwrap();
        assert_eq!(evts.len(), 1);
        assert!(
            matches!(&evts[0], AgentEvent::Progress { text } if text == "∴ The parser drops it.")
        );
    }

    fn fixture(name: &str) -> Vec<TaskEvent> {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .flat_map(|l| parse_claude_line(l).unwrap())
            .filter_map(|e| match e {
                AgentEvent::Task(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn claude_task_lines_yield_task_events() {
        let evts = fixture("claude_task_events.ndjson");
        assert!(matches!(
            &evts[0],
            TaskEvent::Started { id, kind: BgTaskKind::Agent, description, subagent_type, parent: None, backgrounded: true }
                if id == "a0a0e9d731c800ce5" && description == "Run sleep and echo"
                    && subagent_type.as_deref() == Some("general-purpose")
        ));
        assert!(matches!(
            &evts[1],
            TaskEvent::Progress { last_tool, tool_uses: Some(1), .. } if last_tool.as_deref() == Some("Bash")
        ));
        // The subagent's own (foreground) command names its owner.
        assert!(matches!(
            &evts[2],
            TaskEvent::Started { kind: BgTaskKind::Command, parent, backgrounded: false, .. }
                if parent.as_deref() == Some("a0a0e9d731c800ce5")
        ));
        assert!(matches!(
            evts.last().unwrap(),
            TaskEvent::Status { status: BgTaskStatus::Completed, summary, tool_uses: Some(1), .. }
                if summary.as_deref() == Some("hi")
        ));
    }

    #[test]
    fn claude_killed_and_stopped_tasks_are_stopped() {
        let evts = fixture("claude_task_cutoff.ndjson");
        let stops: Vec<&TaskEvent> = evts
            .iter()
            .filter(|e| matches!(e, TaskEvent::Status { id, .. } if id == "b64qntfst"))
            .collect();
        assert_eq!(stops.len(), 2, "task_updated(killed) and task_notification(stopped)");
        assert!(stops
            .iter()
            .all(|e| matches!(e, TaskEvent::Status { status: BgTaskStatus::Stopped, .. })));
        // Other system lines stay silent.
        let other = r#"{"type":"system","subtype":"background_tasks_changed","tasks":[]}"#;
        assert!(parse_claude_line(other).unwrap().is_empty());
    }

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
