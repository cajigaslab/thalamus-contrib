//! The OpenH264 functions FFmpeg's libopenh264 encoder calls, forwarded to
//! Cisco's OpenH264 binary, which is loaded at run time. FFmpeg is built
//! against OpenH264's headers and a stub library (cmake/openh264.cmake), so
//! nothing links OpenH264: users download Cisco's binary separately (Thalamus
//! fetches it into ~/.thalamus), which keeps it under Cisco's H.264 patent
//! license. Users who opt out have it deleted (Preferences > H264).
//!
//! The binary is used whenever it's present. When it's missing or another
//! version, WelsCreateSVCEncoder fails, so opening FFmpeg's encoder fails and
//! nothing is encoded.

use std::ffi::{c_int, c_void};
use std::path::PathBuf;
use std::sync::Mutex;

/// The OpenH264 version FFmpeg's wrapper was compiled against
/// (cmake/openh264/include). Its structs must match the binary's.
const VERSION: (u32, u32, u32) = (2, 6, 0);

/// Cisco's binary for this platform, as Thalamus names it in ~/.thalamus
/// (thalamus/openh264.py).
fn library_name() -> Option<&'static str> {
  match (std::env::consts::OS, std::env::consts::ARCH) {
    ("windows", "x86_64") => Some("openh264-2.6.0-win64.dll"),
    ("linux", "x86_64") => Some("libopenh264-2.6.0-linux64.8.so"),
    ("linux", "aarch64") => Some("libopenh264-2.6.0-linux-arm64.8.so"),
    ("macos", "aarch64") => Some("libopenh264-2.6.0-mac-arm64.dylib"),
    ("macos", "x86_64") => Some("libopenh264-2.6.0-mac-x64.dylib"),
    _ => None,
  }
}

pub fn library_path() -> Option<PathBuf> {
  Some(std::env::home_dir()?.join(".thalamus").join(library_name()?))
}

/// Whether Cisco's binary is downloaded, i.e. H.264 can be encoded.
pub fn library_present() -> bool {
  library_path().is_some_and(|path| path.exists())
}

#[repr(C)]
#[derive(Default)]
struct OpenH264Version {
  major: u32,
  minor: u32,
  revision: u32,
  reserved: u32,
}

type CreateEncoder = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type DestroyEncoder = unsafe extern "C" fn(*mut c_void);
type GetVersion = unsafe extern "C" fn(*mut OpenH264Version);

struct Loaded {
  create: CreateEncoder,
  destroy: DestroyEncoder,
  /// Keeps the functions above valid. Never unloaded: encoders created
  /// through it may outlive any one converter.
  _library: libloading::Library,
}

/// The loaded binary. Failures aren't cached, so a binary downloaded after
/// startup is picked up by the next encoder.
static LOADED: Mutex<Option<Loaded>> = Mutex::new(None);

fn load() -> Result<Loaded, String> {
  let path = library_path().ok_or("Cisco publishes no OpenH264 binary for this platform")?;
  if !path.exists() {
    return Err(format!("{} hasn't been downloaded", path.display()));
  }
  // SAFETY: Cisco's OpenH264 binary, verified by Thalamus when downloaded; it
  // has no initialization routines with requirements of their own.
  unsafe {
    let library = libloading::Library::new(&path).map_err(|e| format!("loading {}: {e}", path.display()))?;
    let get_version = *library
      .get::<GetVersion>(b"WelsGetCodecVersionEx\0")
      .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut version = OpenH264Version::default();
    get_version(&mut version);
    if (version.major, version.minor, version.revision) != VERSION {
      return Err(format!(
        "{} is OpenH264 {}.{}.{}, but FFmpeg was built against {}.{}.{}",
        path.display(), version.major, version.minor, version.revision, VERSION.0, VERSION.1, VERSION.2));
    }
    let create = *library
      .get::<CreateEncoder>(b"WelsCreateSVCEncoder\0")
      .map_err(|e| format!("{}: {e}", path.display()))?;
    let destroy = *library
      .get::<DestroyEncoder>(b"WelsDestroySVCEncoder\0")
      .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Loaded { create, destroy, _library: library })
  }
}

/// Called by FFmpeg's libopenh264 encoder to create an OpenH264 encoder.
/// Returns non-zero, which FFmpeg reports as "Unable to create encoder", when
/// the binary isn't present (never downloaded, or deleted because the user
/// opted out) or can't be loaded.
///
/// # Safety
/// `encoder` must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn WelsCreateSVCEncoder(encoder: *mut *mut c_void) -> c_int {
  unsafe { *encoder = std::ptr::null_mut() };
  // A binary loaded earlier isn't used once it's been deleted.
  if !library_present() {
    println!("OpenH264 isn't downloaded, not encoding H.264");
    return 1;
  }
  let mut loaded = LOADED.lock().unwrap();
  if loaded.is_none() {
    match load() {
      Ok(library) => *loaded = Some(library),
      Err(e) => {
        println!("OpenH264 isn't available, not encoding H.264: {e}");
        return 1;
      }
    }
  }
  let create = loaded.as_ref().unwrap().create;
  drop(loaded);
  unsafe { create(encoder) }
}

/// Called by FFmpeg's libopenh264 encoder to destroy an encoder it created.
///
/// # Safety
/// `encoder` must be one WelsCreateSVCEncoder returned, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn WelsDestroySVCEncoder(encoder: *mut c_void) {
  if encoder.is_null() {
    return;
  }
  // An encoder only exists if the library was loaded, and it's never
  // unloaded.
  let destroy = LOADED.lock().unwrap().as_ref().map(|l| l.destroy);
  if let Some(destroy) = destroy {
    unsafe { destroy(encoder) };
  }
}
