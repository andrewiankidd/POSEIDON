// Tauri build hook - emits the platform-specific glue (Windows app manifest,
// macOS Info.plist, Linux .desktop entries) so the binary behaves as a real
// OS-integrated app. Reads `tauri.conf.json` next to this file.
//
// It also copies the repo's `docs/*.md` into the web bundle at
// `frontend/web/assets/docs/` *before* Tauri embeds `frontendDist`, so the
// in-app documentation viewer's markdown ships inside the binary and is always
// accurate to this build. The frontend fetches those files by relative URL, so
// the same mechanism works in the webview and on a static web host.
use std::fs;
use std::path::Path;

fn main() {
    // Re-run (and so re-embed `frontendDist`) whenever the frontend bundle
    // changes. Without this, a frontend-only edit doesn't trigger this build
    // script, so Tauri keeps embedding a STALE frontend into the binary while the
    // Rust side recompiles - the app silently runs old UI.
    println!("cargo:rerun-if-changed=../../frontend/web");
    copy_docs();
    tauri_build::build()
}

/// Best-effort recursive copy of `docs/` → `frontend/web/assets/docs/`, preserving
/// subdirectories (so `docs/features/*.md` + `docs/features/screenshots/*.png`
/// land under `assets/docs/features/…`). Copies markdown + images only. A docs
/// copy failure must never break the app build, so every step degrades to a no-op.
fn copy_docs() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let root = Path::new(&manifest).join("..").join(".."); // repo root
    let src = root.join("docs");
    let dst = root
        .join("frontend")
        .join("web")
        .join("assets")
        .join("docs");
    println!("cargo:rerun-if-changed={}", src.display());
    copy_docs_dir(&src, &dst);
}

/// Recurse `src` into `dst`, copying only `.md` and image files.
fn copy_docs_dir(src: &Path, dst: &Path) {
    const COPY_EXTS: &[&str] = &["md", "png", "jpg", "jpeg", "gif", "svg", "webp"];
    if fs::create_dir_all(dst).is_err() {
        return;
    }
    let Ok(entries) = fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name() else { continue };
        if path.is_dir() {
            copy_docs_dir(&path, &dst.join(name));
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| COPY_EXTS.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(false)
        {
            let _ = fs::copy(&path, dst.join(name));
        }
    }
}
