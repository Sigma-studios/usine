//! OS notifications ("your session finished"), so a long-running session
//! doesn't have to be watched.
//!
//! macOS posts through `osascript`'s `display notification`, which needs no
//! bundle or entitlement. Clicking such a notification focuses nothing —
//! deep-linking back into a session needs `UNUserNotificationCenter` from a
//! bundled app, which is a follow-up. Other platforms are a no-op for now.

use usine_core::{truncate_chars, Session, SessionStatus, SessionTurnKind};

/// Longest notification body, in chars.
const BODY_MAX: usize = 140;

/// Post a notification titled "Usine" with `subtitle` and `body`. Fire and
/// forget: it runs on its own thread, and a failure is logged, never toasted.
pub fn notify(subtitle: String, body: String) {
    #[cfg(target_os = "macos")]
    std::thread::spawn(move || {
        let script = format!(
            "display notification \"{}\" with title \"Usine\" subtitle \"{}\" sound name \"Glass\"",
            applescript_escape(&body),
            applescript_escape(&subtitle)
        );
        match std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .output()
        {
            Ok(out) if !out.status.success() => tracing::warn!(
                "notification failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => tracing::warn!("notification failed: {e}"),
            Ok(_) => {}
        }
    });
    #[cfg(not(target_os = "macos"))]
    let _ = (subtitle, body);
}

/// Escape a string for an AppleScript string literal: backslashes first, then
/// quotes. Newlines become spaces — a notification shows one or two lines.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r'], " ")
}

/// The notification a session's update calls for: `Some((subtitle, body))`
/// when a turn just ended (the status moved off `Running`), `None` otherwise.
pub fn session_notification(
    previous: Option<&SessionStatus>,
    session: &Session,
) -> Option<(String, String)> {
    if !matches!(previous, Some(SessionStatus::Running)) {
        return None;
    }
    let title = &session.title;
    let (subtitle, body) = match &session.status {
        SessionStatus::Running => return None,
        SessionStatus::NeedsAnswer(iv) => {
            (format!("Needs an answer · {title}"), iv.question.clone())
        }
        SessionStatus::Finished => {
            // A user's Stop also lands on Finished; that one needs no ping.
            if session
                .turns
                .last()
                .is_some_and(|t| t.kind == SessionTurnKind::Error)
            {
                return None;
            }
            let last = session
                .last_turn_of(SessionTurnKind::Agent)
                .map(|t| t.text.clone())
                .unwrap_or_default();
            (format!("Finished · {title}"), last)
        }
        SessionStatus::Failed { message } => (format!("Failed · {title}"), message.clone()),
    };
    let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    Some((subtitle, truncate_chars(&body, BODY_MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use usine_core::{Effort, Intervention, ModelSpec, Provider, SessionAccess, SessionTurn};
    use uuid::Uuid;

    fn session() -> Session {
        Session::new(
            Uuid::new_v4(),
            PathBuf::from("/tmp"),
            Provider::Claude,
            ModelSpec::new("opus", Effort::High),
            SessionAccess::FullAccess,
            "Fix it",
            vec![],
        )
    }

    #[test]
    fn escapes_backslashes_before_quotes() {
        assert_eq!(applescript_escape(r#"a\"b"#), r#"a\\\"b"#);
        assert_eq!(applescript_escape("a\nb"), "a b");
    }

    #[test]
    fn only_a_turn_ending_notifies() {
        let mut s = session();
        s.status = SessionStatus::Finished;
        s.push_turn(SessionTurn::new(SessionTurnKind::Agent, "All   done\nhere"));
        assert_eq!(
            session_notification(Some(&SessionStatus::Running), &s),
            Some(("Finished · Fix it".into(), "All done here".into()))
        );
        // Already finished, or a fresh insert: no ping.
        assert_eq!(
            session_notification(Some(&SessionStatus::Finished), &s),
            None
        );
        assert_eq!(session_notification(None, &s), None);
    }

    #[test]
    fn questions_and_failures_notify_but_a_stop_does_not() {
        let mut s = session();
        s.status = SessionStatus::NeedsAnswer(Intervention {
            request_id: "r".into(),
            question: "q".repeat(300),
            options: vec![],
        });
        let (sub, body) = session_notification(Some(&SessionStatus::Running), &s).unwrap();
        assert_eq!(sub, "Needs an answer · Fix it");
        assert_eq!(body.chars().count(), BODY_MAX);

        s.status = SessionStatus::Failed {
            message: "boom".into(),
        };
        assert_eq!(
            session_notification(Some(&SessionStatus::Running), &s)
                .unwrap()
                .1,
            "boom"
        );

        s.status = SessionStatus::Finished;
        s.push_turn(SessionTurn::new(SessionTurnKind::Error, "Stopped by user"));
        assert_eq!(
            session_notification(Some(&SessionStatus::Running), &s),
            None
        );
    }
}
