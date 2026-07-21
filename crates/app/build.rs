//! Rasterize the single source asset `assets/logo.svg` into the "filled" PNG used
//! for the macOS dock icon and the Windows/Linux window icon (those APIs take
//! raster pixels, not SVG). The in-app sidebar uses `logo.svg` directly, so it
//! is the one source of truth; the only thing added here is the white interior
//! disk that distinguishes the filled badge from the transparent in-app mark.

use std::{env, fs, path::Path};

const ICON_SIZE: u32 = 1024;

fn main() {
    println!("cargo:rerun-if-changed=assets/logo.svg");
    println!("cargo:rerun-if-changed=build.rs");

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
