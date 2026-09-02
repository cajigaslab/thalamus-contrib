fn main() {
  let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

  match target_os.as_str() {
    "windows" => {
      for lib in [
        "Ws2_32", "Secur32", "Bcrypt", "Ncrypt", "Crypt32", "Mfplat", "Ole32", "User32", "dxguid", "uuid", "Mfuuid",
        "strmiids", "Kernel32", "Psapi", "Gdi32", "Vfw32", "OleAut32", "Shlwapi", "oldnames"
      ] {
        println!("cargo:rustc-link-lib=dylib={}", lib);
      }
    }
    _ => {
      if target_os == "macos" {
        // The ffmpeg static libs pull in these system libraries and frameworks
        // (bzlib/zlib compression, and the AVFoundation/AudioToolbox/
        // VideoToolbox/CoreImage/Metal/OpenGL capture + hwaccel paths). They
        // come back from the pkg-config probe below too, but list them
        // explicitly so the link still resolves if pkg-config is unavailable.
        for lib in ["bz2", "z", "iconv"] {
          println!("cargo:rustc-link-lib=dylib={}", lib);
        }
        for framework in [
          "AVFoundation",
          "CoreMedia",
          "CoreVideo",
          "CoreAudio",
          "AudioToolbox",
          "VideoToolbox",
          "CoreImage",
          "CoreGraphics",
          "Metal",
          "OpenGL",
          "AppKit",
          "Foundation",
          "CoreFoundation",
        ] {
          println!("cargo:rustc-link-lib=framework={}", framework);
        }
      }
      // ffmpeg-sys-next links the core avcodec/avdevice/avfilter/avformat/
      // avutil/swresample/swscale libs itself via FFMPEG_DIR, but that
      // raw-path mode bypasses pkg-config, so it never picks up the system
      // libraries those static libs were actually built against: openssl,
      // zlib, va-api and X11 on Linux; the AVFoundation / CoreMedia /
      // CoreVideo / CoreAudio / AudioToolbox / VideoToolbox / CoreImage /
      // Metal / OpenGL frameworks (plus openssl and zlib) on macOS. Query
      // the .pc files (see PKG_CONFIG_PATH in hatch_build.py) for the real
      // link set instead of hand-maintaining a second copy of it here.
      for lib in [
        "libavcodec",
        "libavdevice",
        "libavfilter",
        "libavformat",
        "libavutil",
        "libswresample",
        "libswscale",
      ] {
        if let Err(e) = pkg_config::Config::new().statik(true).probe(lib) {
          println!("cargo:warning=pkg-config probe of {} failed: {}", lib, e);
        }
      }
    }
  }
}
