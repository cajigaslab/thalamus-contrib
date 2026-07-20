fn main() {
  let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

  match target_os.as_str() {
    "windows" => {
      for lib in [
        "Ws2_32", "Secur32", "Bcrypt", "Mfplat", "Ole32", "User32", "dxguid", "uuid", "Mfuuid",
        "strmiids", "Kernel32", "Psapi",
      ] {
        println!("cargo:rustc-link-lib=dylib={}", lib);
      }
    }
    "macos" => {
      println!("cargo:rustc-link-lib=dylib=bz2");
    }
    _ => {
      // ffmpeg-sys-next links the core avcodec/avdevice/avfilter/avformat/
      // avutil/swresample/swscale libs itself via FFMPEG_DIR, but that
      // raw-path mode bypasses pkg-config, so it never picks up the system
      // libraries (openssl, zlib, va-api, X11, ...) those static libs were
      // actually built against. Query the .pc files (see PKG_CONFIG_PATH
      // in .cargo/config.toml / hatch_build.py) for the real link set
      // instead of hand-maintaining a second copy of it here.
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
