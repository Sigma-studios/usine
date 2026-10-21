//! Self-update: find out whether `main` has moved past the commit this binary
//! was built from, rebuild, and relaunch. Usine has no version numbers — "an
//! update" is new commits on `main` of the upstream repo — so `build.rs`
//! stamps every binary with its source commit (`USINE_BUILD_SHA`) and the
//! checkout it came from (`USINE_SOURCE_ROOT`).
//!
//! Two ways of running Usine are supported:
//! - **Installed** with `cargo install --git …` (the binary lives in
//!   `$CARGO_HOME/bin`): the check asks the remote with `git ls-remote`, and
//!   updating re-runs that `cargo install`.
//! - **From source** (`cargo run` in a checkout): the check fetches `origin
//!   main` in the checkout, and updating pulls fast-forward and rebuilds into
//!   the same target dir, so the binary that's running gets replaced.
//!
//! The UI half lives in `ui/update.rs`; this module holds the process work and
//! the small pure helpers it is tested through. Demo mode (`USINE_SIM=1`)
//! simulates the whole flow without touching git or cargo.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use crate::state::demo_mode;

/// Upstream repo — what `cargo install --git` installs from and what an
/// installed binary's check asks.
pub const REPO_URL: &str = "https://github.com/sigma-studios/usine";
const BRANCH: &str = "main";
/// The commit this binary was built from; empty if git couldn't say.
pub const BUILD_SHA: &str = env!("USINE_BUILD_SHA");
/// The checkout this binary was built in; empty if git couldn't say.
const SOURCE_ROOT: &str = env!("USINE_SOURCE_ROOT");
/// How many trailing lines of a failed command's output an error carries.
const ERROR_TAIL_LINES: usize = 15;
/// A check that hangs (a stalled network, a credential prompt) gives up after
/// this long. The install/build has no such limit — a cold build is slow.
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
/// Set on the relaunched process: the pid of the instance it replaces.
const WAIT_PID_ENV: &str = "USINE_RELAUNCH_WAIT_PID";

static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Remember the running binary's path. Called first thing in `main`: on Linux,
/// once cargo replaces the file, `current_exe` reports it with a
/// " (deleted)" suffix.
pub fn record_exe_path() {
    EXE.get_or_init(|| std::env::current_exe().ok().map(strip_deleted));
}

fn exe_path() -> Option<PathBuf> {
    EXE.get_or_init(|| std::env::current_exe().ok().map(strip_deleted))
        .clone()
}

fn strip_deleted(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => PathBuf::from(s),
        None => path,
    }
}

/// How the running copy was installed, which decides how it checks and updates.
#[derive(Clone, Debug, PartialEq)]
pub enum InstallMode {
    /// A `cargo install` binary in `$CARGO_HOME/bin`; `cargo` is the one next
    /// to it, so updating doesn't depend on `PATH`.
    Installed { cargo: PathBuf },
    /// Built in a git checkout that still exists.
    Source { root: PathBuf },
    /// Neither — checking still works against the remote, updating doesn't.
    Unknown,
}

/// Work out the install mode from the binary's location, `$CARGO_HOME`, and the
/// checkout recorded at build time (`is_repo` says whether a path is a git repo).
pub fn detect_mode(
    exe: Option<&Path>,
    cargo_home: Option<&Path>,
    source_root: &str,
    is_repo: impl Fn(&Path) -> bool,
) -> InstallMode {
    if let (Some(exe), Some(home)) = (exe, cargo_home) {
        let bin = home.join("bin");
        if exe.parent() == Some(bin.as_path()) {
            let cargo = bin.join(if cfg!(windows) { "cargo.exe" } else { "cargo" });
            return InstallMode::Installed { cargo };
        }
    }
    if !source_root.is_empty() {
        let root = PathBuf::from(source_root);
        if is_repo(&root) {
            return InstallMode::Source { root };
        }
    }
    InstallMode::Unknown
}

/// The mode of the running copy.
pub fn mode() -> InstallMode {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".cargo")));
    detect_mode(
        exe_path().as_deref(),
        cargo_home.as_deref(),
        SOURCE_ROOT,
        |p| p.join(".git").exists(),
    )
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Why this copy can't update itself, if it can't — the toggle's disabled
/// tooltip. Demo mode can always "update".
pub fn install_blocker(mode: &InstallMode) -> Option<&'static str> {
    if demo_mode() {
        return None;
    }
    match mode {
        InstallMode::Unknown => Some(
            "This copy wasn't installed with `cargo install` and its source checkout is gone, so it can't update itself.",
        ),
        // `cargo install` can't overwrite a running `.exe`.
        InstallMode::Installed { .. } if cfg!(windows) => Some(
            "Windows can't replace a running Usine: quit it and re-run `cargo install` by hand.",
        ),
        _ => None,
    }
}

/// Whether the one automatic check at launch should run. Not for a build from
/// a linked worktree: that's a card's preview (self-hosting), whose commit sits
/// on a `usine/…` branch — "behind main" is meaningless there, and its fetch
/// would move refs shared with the user's own checkout.
pub fn launch_check_enabled() -> bool {
    launch_check_enabled_from(std::env::var_os("USINE_NO_UPDATE_CHECK"))
        && !is_linked_worktree(Path::new(SOURCE_ROOT))
}

/// A linked worktree's `.git` is a file pointing into the main repo; the main
/// checkout's is a directory.
fn is_linked_worktree(root: &Path) -> bool {
    !root.as_os_str().is_empty() && root.join(".git").is_file()
}

fn launch_check_enabled_from(no_check: Option<OsString>) -> bool {
    match no_check {
        None => true,
        Some(v) => v.is_empty() || v == "0",
    }
}

/// The short form of the running commit, for the tooltip.
pub fn build_sha_short() -> &'static str {
    BUILD_SHA.get(..7).unwrap_or(BUILD_SHA)
}

/// What a check found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CheckOutcome {
    UpToDate,
    /// `commits` is how far behind, when the check can count (source builds).
    Available {
        commits: Option<usize>,
    },
}

/// Compare the running build against the latest `main`.
pub async fn check(mode: &InstallMode) -> Result<CheckOutcome, String> {
    if demo_mode() {
        tokio::time::sleep(Duration::from_secs(1)).await;
        return Ok(CheckOutcome::Available { commits: Some(3) });
    }
    if BUILD_SHA.is_empty() {
        return Err("this build doesn't know which commit it came from".into());
    }
    tokio::time::timeout(CHECK_TIMEOUT, check_real(mode))
        .await
        .unwrap_or_else(|_| Err("timed out reaching the repository".into()))
}

async fn check_real(mode: &InstallMode) -> Result<CheckOutcome, String> {
    match mode {
        InstallMode::Source { root } => {
            run(git(root).args(["fetch", "--quiet", "origin", BRANCH])).await?;
            let range = format!("{BUILD_SHA}..origin/{BRANCH}");
            let out = run(git(root).args(["rev-list", "--count", &range])).await?;
            let behind: usize = out
                .trim()
                .parse()
                .map_err(|_| format!("unexpected `git rev-list` output: {}", out.trim()))?;
            Ok(if behind == 0 {
                CheckOutcome::UpToDate
            } else {
                CheckOutcome::Available {
                    commits: Some(behind),
                }
            })
        }
        InstallMode::Installed { .. } | InstallMode::Unknown => {
            let refname = format!("refs/heads/{BRANCH}");
            let mut cmd = tokio::process::Command::new("git");
            cmd.args(["ls-remote", REPO_URL, &refname]);
            let out = run(quiet_git(&mut cmd)).await?;
            let latest = parse_ls_remote(&out, &refname)
                .ok_or_else(|| format!("couldn't read `{refname}` from {REPO_URL}"))?;
            Ok(if latest == BUILD_SHA {
                CheckOutcome::UpToDate
            } else {
                CheckOutcome::Available { commits: None }
            })
        }
    }
}

/// The commit `refname` points at in `git ls-remote` output (`<sha>\t<ref>`
/// lines), if it's there and looks like a full sha.
fn parse_ls_remote(out: &str, refname: &str) -> Option<String> {
    out.lines().find_map(|line| {
        let (sha, name) = line.split_once('\t')?;
        let sha = sha.trim();
        (name.trim() == refname && sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| sha.to_string())
    })
}

/// Build the new version in place: `cargo install` for an installed copy, a
/// fast-forward pull and rebuild for a source checkout. The running process
/// keeps running the old code until `relaunch_after_exit`.
pub async fn install(mode: &InstallMode) -> Result<(), String> {
    if demo_mode() {
        tokio::time::sleep(Duration::from_secs(3)).await;
        return Ok(());
    }
    if let Some(why) = install_blocker(mode) {
        return Err(why.into());
    }
    match mode {
        InstallMode::Installed { cargo } => {
            let mut cmd = tokio::process::Command::new(cargo);
            cmd.args(install_args());
            run(&mut cmd).await.map(drop)
        }
        InstallMode::Source { root } => {
            let branch = run(git(root).args(["rev-parse", "--abbrev-ref", "HEAD"])).await?;
            if branch.trim() != BRANCH {
                return Err(format!(
                    "Update by hand: {} is on `{}`, not `{BRANCH}`.",
                    root.display(),
                    branch.trim()
                ));
            }
            let dirty =
                run(git(root).args(["status", "--porcelain", "--untracked-files=no"])).await?;
            if !dirty.trim().is_empty() {
                return Err(format!(
                    "Update by hand: {} has uncommitted changes.",
                    root.display()
                ));
            }
            run(git(root).args(["pull", "--ff-only", "--quiet", "origin", BRANCH])).await?;
            let mut cmd = tokio::process::Command::new(cargo_for_source());
            cmd.current_dir(root)
                .args(build_args(!cfg!(debug_assertions)));
            // Build into the target dir the running binary sits in (it may be
            // a shared one), so the new binary lands where the relaunch looks.
            if let Some(dir) = exe_path().as_deref().and_then(target_dir_of) {
                cmd.env("CARGO_TARGET_DIR", dir);
            }
            run(&mut cmd).await.map(drop)
        }
        InstallMode::Unknown => unreachable!("install_blocker refuses Unknown"),
    }
}

fn install_args() -> Vec<&'static str> {
    vec!["install", "--git", REPO_URL, "usine-app", "--locked"]
}

fn build_args(release: bool) -> Vec<&'static str> {
    let mut args = vec!["build", "-p", "usine-app"];
    if release {
        args.push("--release");
    }
    args
}

/// `cargo run` hands its own path down as `$CARGO`; otherwise the one in
/// `$CARGO_HOME/bin`, or whatever `PATH` finds.
fn cargo_for_source() -> PathBuf {
    if let Some(cargo) = std::env::var_os("CARGO") {
        return PathBuf::from(cargo);
    }
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".cargo")));
    home.map(|h| h.join("bin").join("cargo"))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("cargo"))
}

/// The target dir a cargo-built binary sits in: `<target>/<profile>/usine`.
fn target_dir_of(exe: &Path) -> Option<PathBuf> {
    let profile_dir = exe.parent()?;
    let profile = profile_dir.file_name()?.to_str()?;
    matches!(profile, "debug" | "release")
        .then(|| profile_dir.parent().map(Path::to_path_buf))
        .flatten()
}

fn git(root: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(root);
    quiet_git(&mut cmd);
    cmd
}

/// Never block on a credential prompt: the app has no terminal to answer it.
fn quiet_git(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    cmd.env("GIT_TERMINAL_PROMPT", "0")
}

/// Run to completion; stdout on success, the tail of the output on failure.
async fn run(cmd: &mut tokio::process::Command) -> Result<String, String> {
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("couldn't run {:?}: {e}", cmd.as_std().get_program()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}\n{stderr}");
    let tail = tail_lines(&combined, ERROR_TAIL_LINES);
    Err(if tail.is_empty() {
        format!("{:?} failed ({})", cmd.as_std().get_program(), out.status)
    } else {
        tail
    })
}

/// The last `n` non-blank lines of `text`.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Start the (new) binary again, detached, with the same arguments and
/// environment. It waits for this process to exit before opening the database
/// (`wait_for_previous_instance`), so the caller quits right after.
pub fn relaunch_after_exit() -> Result<(), String> {
    let exe = exe_path().ok_or("couldn't find the running binary")?;
    if !exe.exists() {
        return Err(format!("{} is missing", exe.display()));
    }
    let pid = std::process::id().to_string();
    crate::spawn_detached(
        &exe,
        &[("USINE_DETACHED", "1"), (WAIT_PID_ENV, pid.as_str())],
    )
    .map(drop)
    .map_err(|e| format!("couldn't start {}: {e}", exe.display()))
}

/// On a relaunch, wait (up to 30s) for the instance being replaced to exit:
/// it still holds the database's one-writer lock until it does.
pub fn wait_for_previous_instance() {
    let Some(pid) = std::env::var(WAIT_PID_ENV).ok() else {
        return;
    };
    // Not for anything this process launches in turn.
    std::env::remove_var(WAIT_PID_ENV);
    let Ok(pid) = pid.parse::<u32>() else {
        return;
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while process_alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // Signal 0 probes without delivering anything; EPERM still means alive.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    // No cheap probe without extra deps; the old window closes within moments
    // of spawning us, so a short fixed wait covers it.
    static WAITED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if WAITED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    std::thread::sleep(Duration::from_secs(3));
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_in_cargo_home_bin_is_installed() {
        let home = Path::new("/home/u/.cargo");
        let mode = detect_mode(
            Some(Path::new("/home/u/.cargo/bin/usine")),
            Some(home),
            "/src/usine",
            |_| true,
        );
        let cargo = if cfg!(windows) { "cargo.exe" } else { "cargo" };
        assert_eq!(
            mode,
            InstallMode::Installed {
                cargo: home.join("bin").join(cargo)
            }
        );
    }

    #[test]
    fn a_binary_elsewhere_with_its_checkout_is_a_source_build() {
        let mode = detect_mode(
            Some(Path::new("/src/usine/target/debug/usine")),
            Some(Path::new("/home/u/.cargo")),
            "/src/usine",
            |p| p == Path::new("/src/usine"),
        );
        assert_eq!(
            mode,
            InstallMode::Source {
                root: PathBuf::from("/src/usine")
            }
        );
    }

    #[test]
    fn no_install_and_no_checkout_is_unknown() {
        let exe = Some(Path::new("/opt/usine"));
        let home = Some(Path::new("/home/u/.cargo"));
        assert_eq!(
            detect_mode(exe, home, "/gone", |_| false),
            InstallMode::Unknown
        );
        assert_eq!(detect_mode(exe, home, "", |_| true), InstallMode::Unknown);
        assert_eq!(detect_mode(None, None, "", |_| true), InstallMode::Unknown);
    }

    #[test]
    fn ls_remote_output_is_parsed_strictly() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let refname = "refs/heads/main";
        assert_eq!(
            parse_ls_remote(&format!("{sha}\t{refname}\n"), refname),
            Some(sha.to_string())
        );
        // Other refs on other lines are skipped.
        let multi = format!("{sha}\trefs/heads/mainline\n{sha}\t{refname}\n");
        assert_eq!(parse_ls_remote(&multi, refname), Some(sha.to_string()));
        assert_eq!(parse_ls_remote("", refname), None);
        assert_eq!(parse_ls_remote("garbage", refname), None);
        assert_eq!(
            parse_ls_remote(&format!("nothex\t{refname}"), refname),
            None
        );
        assert_eq!(
            parse_ls_remote(&format!("{sha}\trefs/heads/other"), refname),
            None
        );
    }

    #[test]
    fn install_reruns_cargo_install_from_the_repo() {
        assert_eq!(
            install_args(),
            ["install", "--git", REPO_URL, "usine-app", "--locked"]
        );
    }

    #[test]
    fn only_a_release_build_rebuilds_with_release() {
        assert_eq!(build_args(false), ["build", "-p", "usine-app"]);
        assert_eq!(build_args(true), ["build", "-p", "usine-app", "--release"]);
    }

    #[test]
    fn the_target_dir_is_two_levels_up_from_a_profile_dir() {
        assert_eq!(
            target_dir_of(Path::new("/c/agent-target/debug/usine")),
            Some(PathBuf::from("/c/agent-target"))
        );
        assert_eq!(
            target_dir_of(Path::new("/src/target/release/usine")),
            Some(PathBuf::from("/src/target"))
        );
        assert_eq!(target_dir_of(Path::new("/usr/local/bin/usine")), None);
    }

    #[test]
    fn errors_keep_the_last_non_blank_lines() {
        let text = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let tail = tail_lines(&text, 3);
        assert_eq!(tail, "line 18\nline 19\nline 20");
        assert_eq!(tail_lines("one\n", 15), "one");
        assert_eq!(tail_lines("\n \n", 15), "");
    }

    #[test]
    fn a_deleted_suffix_is_stripped_from_the_exe_path() {
        assert_eq!(
            strip_deleted(PathBuf::from("/bin/usine (deleted)")),
            PathBuf::from("/bin/usine")
        );
        assert_eq!(
            strip_deleted(PathBuf::from("/bin/usine")),
            PathBuf::from("/bin/usine")
        );
    }

    #[test]
    fn the_launch_check_honours_the_opt_out() {
        assert!(launch_check_enabled_from(None));
        assert!(launch_check_enabled_from(Some("".into())));
        assert!(launch_check_enabled_from(Some("0".into())));
        assert!(!launch_check_enabled_from(Some("1".into())));
        assert!(!launch_check_enabled_from(Some("true".into())));
    }

    #[test]
    fn only_a_linked_worktree_counts_as_one() {
        let dir = std::env::temp_dir().join(format!("usine-wt-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("main/.git")).unwrap();
        std::fs::create_dir_all(dir.join("wt")).unwrap();
        std::fs::write(dir.join("wt/.git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
        assert!(!is_linked_worktree(&dir.join("main")));
        assert!(is_linked_worktree(&dir.join("wt")));
        assert!(!is_linked_worktree(Path::new("")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
