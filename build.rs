fn main() {
    // fdk-aac is installed from source into /usr/local (no distro package).
    println!("cargo:rustc-link-search=native=/usr/local/lib");

    // The `video` feature links whatever system libav* ffmpeg-next finds,
    // and video.video decodes whatever the container probe selects, so the
    // linked FFmpeg version is the security boundary. Floor: FFmpeg 8.1.2
    // (CVE-2026-8461 "PixelSmash" — MagicYUV heap OOB write — plus the
    // same-window MACE6/RASC/vf_hqdn3d decoder CVEs, fixed in 8.1.2).
    // pkg-config reports libavcodec 62.x for FFmpeg 8, but cannot
    // distinguish patch releases, so the check is the conservative FFmpeg-8
    // floor and ≥ 8.1.2 is documented as the requirement.
    if std::env::var_os("CARGO_FEATURE_VIDEO").is_some() {
        let output = std::process::Command::new("pkg-config")
            .args(["--modversion", "libavcodec"])
            .output();
        let version = match output {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            Ok(_) => {
                panic!("pkg-config could not report libavcodec: the `video` feature requires FFmpeg >= 8.1.2")
            }
            Err(e) => {
                panic!("pkg-config failed ({e}): the `video` feature requires FFmpeg >= 8.1.2")
            }
        };
        let major = version.split('.').next().and_then(|v| v.parse::<u32>().ok());
        if major.unwrap_or(0) < 62 {
            panic!(
                "the `video` feature requires FFmpeg >= 8.1.2 (libavcodec >= 62.x), found libavcodec {version}. \
                 CVE-2026-8461 (\"PixelSmash\", heap OOB write in the MagicYUV decoder) and the same-window \
                 decoder CVEs are fixed in 8.1.2, and video.video decodes whatever the container probe \
                 selects. Use backports or the FFmpeg release build."
            );
        }
    }
}
