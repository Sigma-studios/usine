//! Rasterize the single source asset `assets/logo.svg` into the "filled" PNG used
//! for the macOS dock icon and the Windows/Linux window icon (those APIs take
//! raster pixels, not SVG). The in-app sidebar uses `logo.svg` directly, so it
//! is the one source of truth; the only thing added here is the white interior
//! disk that distinguishes the filled badge from the transparent in-app mark.
//!
//! It also stamps the binary with the commit it was built from
//! (`USINE_BUILD_SHA`) and the checkout's root (`USINE_SOURCE_ROOT`), which the
//! bottom bar's update check compares against the latest `main` (`update.rs`).

use std::{env, fs, path::Path, process::Command};

const ICON_SIZE: u32 = 1024;

fn main() {
    println!("cargo:rerun-if-changed=assets/logo.svg");
    println!("cargo:rerun-if-changed=build.rs");
    stamp_git_info();

    let svg = fs::read_to_string("assets/logo.svg").expect("read assets/logo.svg");
    // Inject the white interior disk just before the first <path>, turning the
    // transparent source into the filled variant. Keeping this here (rather than
    // as a second checked-in asset) means `logo.svg` stays the only source.
    let filled = svg.replacen(
        "<path",
        r##"<circle cx="500" cy="500" r="418" fill="#ffffff"/><path"##,
        1,
    );
    assert!(
        filled.contains("<circle"),
        "logo.svg had no <path> to anchor the fill"
    );

    let tree = resvg::usvg::Tree::from_str(&filled, &resvg::usvg::Options::default())
        .expect("parse logo.svg");
    let mut pixmap = resvg::tiny_skia::Pixmap::new(ICON_SIZE, ICON_SIZE).expect("allocate pixmap");
    let scale = ICON_SIZE as f32 / tree.size().width();
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("logo_filled.png");
    fs::write(&out, pixmap.encode_png().expect("encode png")).expect("write logo_filled.png");
}

/// Record the source commit and checkout root as compile-time env vars, both
/// empty when git can't say (no git, a source tarball). A `cargo install --git`
/// build qualifies too: cargo's git checkouts keep their `.git`.
fn stamp_git_info() {
    let sha = git(&["rev-parse", "HEAD"]).unwrap_or_default();
    let root = git(&["rev-parse", "--show-toplevel"]).unwrap_or_default();
    println!("cargo:rustc-env=USINE_BUILD_SHA={sha}");
    println!("cargo:rustc-env=USINE_SOURCE_ROOT={root}");
    // Re-stamp when HEAD moves to another branch or the branch itself moves.
    // `--git-path` resolves both inside a worktree (HEAD is per worktree, refs
    // live in the common dir); packed-refs covers a branch that was packed.
    // A packed branch has no loose file to watch, and the pull that moves it
    // writes a new one while HEAD and packed-refs stay put — so also watch
    // HEAD's reflog, which git appends to on every pull or commit on the
    // branch. (Not the `refs/heads` directory: cargo watches it recursively,
    // so every agent commit on any `usine/…` branch would re-stamp.)
    let mut watched = vec![
        "HEAD".to_string(),
        "packed-refs".to_string(),
        "logs/HEAD".to_string(),
    ];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.push(branch);
    }
    for path in watched {
        // A missing path (no packed-refs, a branch that only lives packed)
        // would make cargo rerun this script on every build, so skip those.
        if let Some(p) = git(&["rev-parse", "--git-path", &path]) {
            if Path::new(&p).exists() {
                println!("cargo:rerun-if-changed={p}");
            }
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}
