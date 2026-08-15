//! Test-only: render a testsrc clip with the local `ffmpeg` binary.

#![cfg(test)]

use std::path::PathBuf;
use std::process::Command;

/// Render a 1 s 25 fps testsrc with the local `ffmpeg` binary; skip the
/// test entirely when no `ffmpeg` is installed. `tag` keeps the path
/// unique per test so parallel runs never delete each other's clip.
pub fn render_test_clip(tag: &str) -> Option<PathBuf> {
    render_test_clip_size(tag, 320, 240)
}

/// [`render_test_clip`] at an explicit resolution.
pub fn render_test_clip_size(tag: &str, width: u32, height: u32) -> Option<PathBuf> {
    let path =
        std::env::temp_dir().join(format!("crabsoup-testsrc-{}-{tag}.mp4", std::process::id()));
    if path.exists() {
        std::fs::remove_file(&path).ok();
    }
    let ok = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=duration=1:size={width}x{height}:rate=25"),
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
        ])
        .arg(&path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        std::fs::remove_file(&path).ok();
        return None;
    }
    Some(path)
}
