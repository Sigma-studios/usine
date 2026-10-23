//! Killing a spawned process together with everything it started: preview
//! servers, validation scripts, and agent CLIs with their running tools.

use std::collections::HashSet;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use tokio::process::{Child, Command};

/// How long a TERMed tree gets to exit before it is KILLed.
pub(crate) const KILL_GRACE: Duration = Duration::from_millis(800);

/// Pids of the agent CLIs currently running, so the app can reap them on quit
/// (see [`take_agents`]): like previews they lead their own process groups, so
/// nothing else takes them down when the app exits.
static AGENTS: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

/// Registration of a running agent CLI in [`AGENTS`]; dropping it (the run's
/// pump ending) unregisters the pid.
pub(crate) struct AgentPid(Option<u32>);

/// Record `child` as a running agent until the returned guard drops.
pub(crate) fn track_agent(child: &Child) -> AgentPid {
    let pid = child.id();
    if let Some(pid) = pid {
        let mut agents = AGENTS.lock().unwrap_or_else(|e| e.into_inner());
        agents.get_or_insert_with(HashSet::new).insert(pid);
    }
    AgentPid(pid)
}

impl Drop for AgentPid {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let mut agents = AGENTS.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(set) = agents.as_mut() {
                set.remove(&pid);
            }
        }
    }
}

/// Drain every registered agent pid — for the app's shutdown reap.
pub(crate) fn take_agents() -> Vec<u32> {
    let mut agents = AGENTS.lock().unwrap_or_else(|e| e.into_inner());
    agents.take().map(|s| s.into_iter().collect()).unwrap_or_default()
}

/// Stop `child` and every process under it — a CLI's foreground tool (a
/// `cargo test`, a dev server) would otherwise outlive it. `child` must have
/// been spawned with `process_group(0)`. Falls back to killing `child` alone.
pub(crate) async fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        kill_group(pid).await;
    }
    let _ = child.start_kill();
}

/// SIGTERM then — once the tree has exited, or after a grace — SIGKILL a
/// process's entire tree. `pid` is a
/// process we spawned as a process-group leader (`process_group(0)`): a
/// preview's `sh -lc <run>`, a validation script, an agent CLI.
///
/// Signalling that group reaps everything that stayed in it, but `pnpm`/`turbo`
/// move their dev servers — and each `--watch` task — into *new* process groups
/// of their own, which a group-only signal misses, leaving them alive to keep
/// writing into the worktree (and racing its removal on merge). So we also snapshot
/// the descendant tree by ppid *before* killing (once the root dies, reparented
/// survivors become unreachable this way) and signal every descendant directly.
#[cfg(unix)]
pub(crate) async fn kill_group(pid: u32) {
    let mut targets = descendants(pid).await;
    targets.push(pid);
    signal_all("-TERM", pid, &targets).await;
    let deadline = tokio::time::Instant::now() + KILL_GRACE;
    while tokio::time::Instant::now() < deadline && any_alive(&targets).await {
        tokio::time::sleep(KILL_POLL).await;
    }
    // Even when the snapshot is all gone: a process forked into the group
    // after it was taken may still be there.
    signal_all("-KILL", pid, &targets).await;
}

/// How often [`kill_group`] checks whether a TERMed tree has exited yet.
#[cfg(unix)]
const KILL_POLL: Duration = Duration::from_millis(25);

/// Whether any of `pids` is still running — zombies (exited, not yet reaped)
/// count as gone. Errs towards "alive" if `ps` can't be read.
#[cfg(unix)]
async fn any_alive(pids: &[u32]) -> bool {
    let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let Ok(out) = Command::new("ps")
        .args(["-o", "stat=", "-p", &list])
        .stderr(Stdio::null())
        .output()
        .await
    else {
        return true;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|stat| !stat.trim().is_empty() && !stat.trim().starts_with('Z'))
}

/// Send `sig` to the process group led by `group` (negated pid) and, belt-and-
/// suspenders, to each pid in `pids` individually — covering children that left
/// the group. `kill` signals every valid target even if some are already gone.
///
/// The `--` before the operands is load-bearing: without it, procps-ng's
/// `kill` (Linux) parses the negated group as an option and degenerates to
/// `kill(-1, sig)` — a signal to every process the user owns, which on a CI
/// runner takes down the runner itself.
#[cfg(unix)]
async fn signal_all(sig: &str, group: u32, pids: &[u32]) {
    let mut args: Vec<String> = Vec::with_capacity(pids.len() + 3);
    args.push(sig.to_string());
    args.push("--".to_string());
    args.push(format!("-{group}"));
    args.extend(pids.iter().map(u32::to_string));
    let _ = Command::new("kill")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// Every descendant pid of `root` (children, grandchildren, …) from a single `ps`
/// snapshot. Empty if `ps` can't be read — the group signal still covers the
/// common case.
#[cfg(unix)]
async fn descendants(root: u32) -> Vec<u32> {
    let Ok(out) = Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .await
    else {
        return Vec::new();
    };
    // ppid -> children.
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split_whitespace();
        if let (Some(pid), Some(ppid)) = (it.next(), it.next()) {
            if let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if let Some(kids) = children.get(&p) {
            for &k in kids {
                found.push(k);
                stack.push(k);
            }
        }
    }
    found
}

#[cfg(not(unix))]
pub(crate) async fn kill_group(_pid: u32) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[tokio::test]
    async fn kill_tree_reaps_children() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 60 & sleep 60"])
            .process_group(0)
            .spawn()
            .unwrap();
        let root = child.id().unwrap();
        // Let the shell start both sleeps.
        let mut kids = Vec::new();
        for _ in 0..50 {
            kids = descendants(root).await;
            if kids.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(kids.len() >= 2, "expected two sleeps, got {kids:?}");
        kill_tree(&mut child).await;
        let _ = child.wait().await;
        for pid in kids {
            // A killed child of a dead shell may linger as a zombie briefly
            // until init reaps it.
            let mut gone = false;
            for _ in 0..50 {
                if !alive(pid) {
                    gone = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(gone, "pid {pid} survived");
        }
    }
}
