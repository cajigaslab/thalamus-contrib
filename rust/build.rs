/// Names bindgen emits differently from plugin.h.
#[derive(Debug)]
struct ThalamusCallbacks;

impl bindgen::callbacks::ParseCallbacks for ThalamusCallbacks {
  fn item_name(&self, item: bindgen::callbacks::ItemInfo) -> Option<String> {
    // crate::api::ThalamusAPI is the safe wrapper; the raw table keeps the
    // Raw suffix it has always had on the Rust side.
    (item.name == "ThalamusAPI").then(|| "ThalamusAPIRaw".to_string())
  }

  fn int_macro(&self, name: &str, _value: i64) -> Option<bindgen::callbacks::IntKind> {
    // Match the C types the macros are written with (UINT64_C / INT32_C).
    if name.starts_with("THALAMUS_SDL_WINDOW_") {
      Some(bindgen::callbacks::IntKind::U64)
    } else if name.starts_with("THALAMUS_MODALITY_") {
      Some(bindgen::callbacks::IntKind::U32)
    } else if name.starts_with("THALAMUS_SDL_SYSTEM_CURSOR_")
      || name == "THALAMUS_OPERATION_ABORTED"
    {
      Some(bindgen::callbacks::IntKind::I32)
    } else {
      None
    }
  }
}

/// Generates Rust declarations for everything in Thalamus's plugin.h and
/// modalities.h (vendored under include/thalamus from the Thalamus devel branch).
fn generate_thalamus_api() {
  let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
  bindgen::Builder::default()
    .header("include/thalamus/plugin.h")
    .header("include/thalamus/modalities.h")
    .clang_args(["-x", "c", "-std=c99", "-Iinclude"])
    .allowlist_type("Thalamus.*|THALAMUS_.*|Vk.*")
    .allowlist_var("THALAMUS_.*")
    // Thalamus's own enums may gain values this build doesn't know about, so
    // they must not be Rust enums (an unknown discriminant would be UB).
    .newtype_enum("Thalamus.*")
    .prepend_enum_name(false)
    // Evaluates macros like UINT64_C(...) that bindgen can't parse itself.
    .clang_macro_fallback()
    .clang_macro_fallback_build_dir(&out_dir)
    .derive_debug(true)
    .derive_default(true)
    .parse_callbacks(Box::new(ThalamusCallbacks))
    .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
    .generate()
    .expect("failed to generate bindings from the Thalamus headers")
    .write_to_file(out_dir.join("thalamus_api.rs"))
    .expect("failed to write Thalamus bindings");
}

/// Compiles the GLSL shaders under src/shaders to SPIR-V in OUT_DIR, where
/// the code includes them from (e.g. `convert.comp` -> `convert.comp.spv`).
/// Uses naga so no shader compiler has to be installed.
fn compile_shaders() {
  let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
  let shaders = [("convert.comp", naga::ShaderStage::Compute)];
  for (name, stage) in shaders {
    let path = format!("src/shaders/{name}");
    println!("cargo:rerun-if-changed={path}");
    let source = std::fs::read_to_string(&path).expect("failed to read shader");

    let mut module = naga::front::glsl::Frontend::default()
      .parse(&naga::front::glsl::Options::from(stage), &source)
      .unwrap_or_else(|e| panic!("{}", e.emit_to_string_with_path(&source, &path)));
    // The GLSL frontend leaves behind types for every builtin overload (e.g.
    // an image type per imageStore format), and the SPIR-V backend would
    // declare capabilities for them that the device may not have enabled.
    naga::compact::compact(&mut module, naga::compact::KeepUnused::No);
    let info = naga::valid::Validator::new(
      naga::valid::ValidationFlags::all(),
      // Push constants, which naga calls immediates.
      naga::valid::Capabilities::IMMEDIATES,
    )
    .validate(&module)
    .unwrap_or_else(|e| panic!("{}", e.emit_to_string_with_path(&source, &path)));
    let options = naga::back::spv::Options {
      // Thalamus creates a Vulkan 1.0 instance.
      lang_version: (1, 0),
      ..Default::default()
    };
    let words = naga::back::spv::write_vec(&module, &info, &options, None)
      .unwrap_or_else(|e| panic!("failed to write SPIR-V for {path}: {e}"));
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    std::fs::write(out_dir.join(format!("{name}.spv")), bytes)
      .expect("failed to write compiled shader");
  }
}

fn main() {
  generate_thalamus_api();
  compile_shaders();

  let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

  match target_os.as_str() {
    "windows" => {
      for lib in [
        "Ws2_32", "Secur32", "Bcrypt", "Ncrypt", "Crypt32", "Mfplat", "Ole32", "User32", "dxguid",
        "uuid", "Mfuuid", "strmiids", "Kernel32", "Psapi", "Gdi32", "Vfw32", "OleAut32", "Shlwapi",
        "oldnames",
      ] {
        println!("cargo:rustc-link-lib=dylib={}", lib);
      }
      // libvpx and libopus, which the source-built FFmpeg's VP9 and Opus
      // encoders call (see hatch_build.py). libavcodec's link flags aren't
      // read from pkg-config here, so they're linked explicitly.
      for (var, lib) in [("VPX_LIB_DIR", "vpx"), ("OPUS_LIB_DIR", "opus")] {
        println!("cargo:rerun-if-env-changed={}", var);
        if let Ok(dir) = std::env::var(var) {
          println!("cargo:rustc-link-search=native={}", dir);
          println!("cargo:rustc-link-lib=static={}", lib);
        }
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
