//! Capturing a child process's output: stream each line live to the UI
//! transcript while keeping a bounded tail of it, so a failure can quote what
//! the command actually said instead of just its exit status. Shared by the
//! validation gate and the preview's setup/run/teardown processes.

use std::collections::VecDeque;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

use super::*;

/// A bounded tail of a command's output: the last `max_lines` lines capped at
/// `max_bytes`, with a truncation marker once anything fell off. The
/// interesting part of a failing build/test/setup run is almost always the end.
pub(super) struct OutputTail {
    lines: VecDeque<String>,
    bytes: usize,
    truncated: bool,
    max_lines: usize,
    max_bytes: usize,
}

impl OutputTail {
    pub(super) fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            truncated: false,
            max_lines,
            max_bytes,
        }
    }

    pub(super) fn shared(max_lines: usize, max_bytes: usize) -> SharedTail {
        Arc::new(Mutex::new(Self::new(max_lines, max_bytes)))
    }

    pub(super) fn push(&mut self, mut line: String) {
        // A single line over the byte cap (a one-line JSON log with a stack, a
        // minified-bundle error) would otherwise evict itself along with
        // everything else — keep its end instead, it's usually the error.
        if line.len() + 1 > self.max_bytes {
            let mut start = line.len() + 1 - self.max_bytes;
            while !line.is_char_boundary(start) {
                start += 1;
            }
            line.drain(..start);
            self.truncated = true;
        }
        self.bytes += line.len() + 1;
        self.lines.push_back(line);
        while self.lines.len() > self.max_lines || self.bytes > self.max_bytes {
            if let Some(dropped) = self.lines.pop_front() {
                self.bytes -= dropped.len() + 1;
                self.truncated = true;
            } else {
                break;
            }
        }
    }

    /// The kept lines, newline-joined, behind a truncation marker if anything
    /// fell off. Empty when the command printed nothing (blank lines aside).
    pub(super) fn render(&self) -> String {
        let mut out = String::new();
        if self.truncated {
            out.push_str("…(output truncated)\n");
        }
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out.trim_end().to_string()
    }
}

pub(super) type SharedTail = Arc<Mutex<OutputTail>>;

/// Forward a child stream's lines to the UI transcript live (not persisted —
/// command output can be voluminous) while also collecting the bounded tail.
pub(super) fn spawn_capture_reader<S: AsyncRead + Unpin + Send + 'static>(
    evt_tx: UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
    stream: S,
    tail: SharedTail,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            lock(&tail).push(line.clone());
            let _ = evt_tx.unbounded_send(ExecutorEvent::transcript(card_id, now_millis(), line));
        }
    })
}

/// How long [`drain`] waits for the readers once the child has exited.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Wait (bounded) for a child's readers to reach EOF, so the tail holds the
/// last lines it wrote before it is rendered. Bounded because a background
/// process the command left behind (`pnpm dev &`) inherits the pipes and holds
/// them open long after the command itself exited.
pub(super) async fn drain(readers: Vec<tokio::task::JoinHandle<()>>) {
    let all = futures::future::join_all(readers);
    let _ = tokio::time::timeout(DRAIN_GRACE, all).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_last_lines_with_a_marker() {
        let mut tail = OutputTail::new(2, 1024);
        for l in ["one", "two", "three"] {
            tail.push(l.to_string());
        }
        assert_eq!(tail.render(), "…(output truncated)\ntwo\nthree");
    }

    #[test]
    fn byte_cap_drops_old_lines() {
        let mut tail = OutputTail::new(100, 10);
        tail.push("aaaa".into()); // 5 bytes
        tail.push("bbbb".into()); // 10
        assert_eq!(tail.render(), "aaaa\nbbbb");
        tail.push("cc".into()); // 13 → drop "aaaa"
        assert_eq!(tail.render(), "…(output truncated)\nbbbb\ncc");
    }

    #[test]
    fn oversized_line_keeps_its_end() {
        let mut tail = OutputTail::new(100, 8);
        tail.push("old".into());
        tail.push("xxxxxxxxxxé-error".into());
        assert_eq!(tail.render(), "…(output truncated)\n-error");
        tail.push("ok".into());
        assert_eq!(tail.render(), "…(output truncated)\nok");
    }

    #[test]
    fn empty_and_blank_output_render_empty() {
        let mut tail = OutputTail::new(5, 1024);
        assert_eq!(tail.render(), "");
        tail.push(String::new());
        assert_eq!(tail.render(), "");
    }
}
