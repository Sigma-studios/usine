# Vendored `dioxus-desktop`

Upstream: `dioxus-desktop` **0.7.9** from crates.io, copied verbatim (the
published package, so `Cargo.toml` is the normalized manifest) and wired in via
`[patch.crates-io]` in the workspace `Cargo.toml`. The version number is kept so
`Cargo.lock` keeps resolving. Files the build never reads were left out: the
package's own `Cargo.lock`, `Cargo.toml.orig`, `headless_tests/` (and their
`[[test]]` entries in `Cargo.toml`), `.vscode/`, `tsconfig.json` and the
architecture diagram.

## Why

Usine warns before closing while agent runs are in progress. Upstream
`WindowCloseBehaviour` can only hide or close a window on a close request; there
is no way to keep the window open and let the app decide. Hiding is not an
option: on macOS a hidden fullscreen window leaves its Space.

## Diff against upstream

```diff
--- src/config.rs
+++ src/config.rs
@@ pub enum WindowCloseBehaviour {
     /// Window will close
     WindowCloses,
+
+    /// Close requests are ignored: the window stays open and the app decides
+    /// what to do (e.g. confirm, then switch to `WindowCloses` and close).
+    /// Usine addition — see `VENDORED.md`.
+    WindowStays,
 }
--- src/app.rs
+++ src/app.rs
@@ pub fn handle_close_requested(&mut self, id: WindowId) {
+            // The app handles the request itself (Usine addition — see VENDORED.md)
+            WindowCloseBehaviour::WindowStays => {}
+
             // If the window is set to close, we can remove it from the list of webviews
```

## Dropping it

Once upstream ships an equivalent variant, delete `vendor/`, the
`[patch.crates-io]` entry and the `exclude` in the workspace `Cargo.toml`, and
rename the variant in `crates/app/src/main.rs` / `ui/confirm.rs` if needed.
