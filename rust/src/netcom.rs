//! Runtime binding to Neuralynx's NetCom client (NetComClient3_x64.dll), used
//! by the NEURALYNX node to stream from Cheetah.
//!
//! NetCom is only distributed as a C++ DLL, so it is loaded with libloading
//! and its MSVC-mangled exports are called directly: on x64 Windows member
//! functions use the ordinary Win64 calling convention with `this` as the
//! first argument, so `extern "C"` function pointers taking the object
//! pointer first call them correctly. Exports that pass std::wstring across
//! the DLL boundary are avoided; the *Internal C-style variants the header's
//! inline wrappers use are called instead.
//!
//! The DLL is optional: load() fails (and the node disables itself) when it,
//! or its Visual C++ 2012 runtime dependency, isn't installed.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// NetCom's DAS object type for continuously sampled channels.
pub const CSC_TYPE: &str = "CscAcqEnt";
/// NetCom's DAS object type for event streams.
pub const EVENT_TYPE: &str = "EventAcqEnt";

const DLL_NAME: &str = "NetComClient3_x64.dll";

/// Environment variable naming a directory to look for the NetCom DLL in
/// before the default search order.
const DIR_ENV: &str = "THALAMUS_NETCOM_DIR";

const MAX_CSC_SAMPLES: usize = 512;
const EVENT_STRING_SIZE: usize = 128;

/// NlxDataTypes::CRRec (byte packed).
#[repr(C, packed)]
struct CRRec {
  qw_time_stamp: u64,
  dw_channel_num: u32,
  dw_sample_freq: u32,
  dw_num_valid_samples: u32,
  sn_samples: [i16; MAX_CSC_SAMPLES],
}
const _: () = assert!(std::mem::size_of::<CRRec>() == 1044);

/// NlxDataTypes::EventRec (byte packed).
#[repr(C, packed)]
struct EventRec {
  nstx: i16,
  npkt_id: i16,
  npkt_data_size: i16,
  qw_time_stamp: u64,
  nevent_id: i16,
  nttl: i16,
  ncrc: i16,
  ndummy1: i16,
  ndummy2: i16,
  dn_extra: [i32; 8],
  event_string: [u8; EVENT_STRING_SIZE],
}
const _: () = assert!(std::mem::size_of::<EventRec>() == 184);

type Obj = *mut c_void;
type CscCallback = unsafe extern "C" fn(*mut c_void, *const CRRec, i32, *const u16);
type EventCallback = unsafe extern "C" fn(*mut c_void, *const EventRec, i32, *const u16);
type ConnectionLostCallback = unsafe extern "C" fn(*mut c_void);
type MessageCallback = unsafe extern "C" fn(*mut c_void, i32, *mut u16);

struct Functions {
  get_new_client: unsafe extern "C" fn() -> Obj,
  delete_client: unsafe extern "C" fn(Obj),
  connect_to_server: unsafe extern "C" fn(Obj, *const u16, bool) -> u8,
  disconnect_from_server: unsafe extern "C" fn(Obj) -> u8,
  are_we_connected: unsafe extern "C" fn(Obj) -> u8,
  open_stream: unsafe extern "C" fn(Obj, *const u16) -> u8,
  set_application_name: unsafe extern "C" fn(Obj, *const u16) -> u8,
  get_das_objects_and_types:
    unsafe extern "C" fn(Obj, *mut *mut *mut u16, *mut *mut *mut u16, *mut i32) -> u8,
  send_command: unsafe extern "C" fn(Obj, *const u16, *mut i32, *mut *mut *mut u16, *mut i32) -> u8,
  get_client_version_string: unsafe extern "C" fn(Obj, *mut *mut u16),
  free_array: unsafe extern "C" fn(Obj, *mut c_void),
  set_callback_csc: unsafe extern "C" fn(Obj, Option<CscCallback>, *mut c_void),
  set_callback_ev: unsafe extern "C" fn(Obj, Option<EventCallback>, *mut c_void),
  set_callback_connection_lost: unsafe extern "C" fn(Obj, Option<ConnectionLostCallback>, *mut c_void),
  set_callback_message: unsafe extern "C" fn(Obj, Option<MessageCallback>, *mut c_void),
}

/// The loaded DLL. Kept loaded for the life of the process.
pub struct NetCom {
  _library: libloading::Library,
  functions: Functions,
  path: String,
}

// The struct only holds function pointers into a DLL that is never unloaded.
unsafe impl Send for NetCom {}
unsafe impl Sync for NetCom {}

impl NetCom {
  /// Where the DLL was loaded from (or its bare name if found by the
  /// system's search order).
  pub fn path(&self) -> &str {
    &self.path
  }
}

static NETCOM: OnceLock<Result<NetCom, String>> = OnceLock::new();

/// Loads NetCom the first time it's called and returns the result every time.
pub fn load() -> Result<&'static NetCom, String> {
  NETCOM.get_or_init(load_impl).as_ref().map_err(|e| e.clone())
}

/// Places to try, in order: $THALAMUS_NETCOM_DIR, the system's DLL search
/// order (the application directory, PATH, ...), then Neuralynx's install
/// directories under Program Files.
fn candidates() -> Vec<PathBuf> {
  let mut result = Vec::new();
  if let Some(dir) = std::env::var_os(DIR_ENV) {
    result.push(PathBuf::from(dir).join(DLL_NAME));
  }
  result.push(PathBuf::from(DLL_NAME));
  for var in ["ProgramFiles", "ProgramFiles(x86)"] {
    let Some(program_files) = std::env::var_os(var) else {
      continue;
    };
    let neuralynx = PathBuf::from(program_files).join("Neuralynx");
    result.push(neuralynx.join(DLL_NAME));
    // e.g. Neuralynx\Cheetah, Neuralynx\NetComDevelopmentPackage_v3.1.0
    let Ok(entries) = std::fs::read_dir(&neuralynx) else {
      continue;
    };
    let mut dirs: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
      result.push(dir.join(DLL_NAME));
      result.push(dir.join("Runtime").join(DLL_NAME));
    }
  }
  result
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn open_library(path: &std::path::Path) -> Result<libloading::Library, libloading::Error> {
  use libloading::os::windows::{LOAD_WITH_ALTERED_SEARCH_PATH, Library};
  // A full path also searches the DLL's own directory for its dependencies.
  let flags = if path.is_absolute() { LOAD_WITH_ALTERED_SEARCH_PATH } else { 0 };
  unsafe { Library::load_with_flags(path, flags).map(Into::into) }
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn load_impl() -> Result<NetCom, String> {
  let mut errors = Vec::new();
  for path in candidates() {
    if path.is_absolute() && !path.exists() {
      continue;
    }
    match open_library(&path) {
      Ok(library) => {
        let functions = load_functions(&library)?;
        return Ok(NetCom {
          _library: library,
          functions,
          path: path.display().to_string(),
        });
      }
      Err(e) => errors.push(format!("{}: {}", path.display(), e)),
    }
  }
  Err(format!(
    "{DLL_NAME} not found or failed to load (it also needs the Visual C++ 2012 x64 runtime). Tried: {}",
    errors.join("; ")
  ))
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
fn load_impl() -> Result<NetCom, String> {
  // Only referenced on Windows.
  let _ = candidates;
  let _ = load_functions;
  Err("NetCom is only available on 64-bit Windows".to_string())
}

fn symbol<T: Copy>(library: &libloading::Library, name: &str) -> Result<T, String> {
  let mut bytes = name.as_bytes().to_vec();
  bytes.push(0);
  unsafe {
    library
      .get::<T>(&bytes)
      .map(|f| *f)
      .map_err(|e| format!("{DLL_NAME} is missing {name}: {e}"))
  }
}

fn load_functions(library: &libloading::Library) -> Result<Functions, String> {
  Ok(Functions {
    get_new_client: symbol(library, "?GetNewNetComClient@NlxNetCom@@YAPEAVNetComClient@1@XZ")?,
    delete_client: symbol(library, "?DeleteNetComClient@NlxNetCom@@YAXPEAVNetComClient@1@@Z")?,
    connect_to_server: symbol(library, "?ConnectToServer@NetComClient@NlxNetCom@@QEAA_NQEB_W_N@Z")?,
    disconnect_from_server: symbol(library, "?DisconnectFromServer@NetComClient@NlxNetCom@@QEAA_NXZ")?,
    are_we_connected: symbol(library, "?AreWeConnected@NetComClient@NlxNetCom@@QEAA_NXZ")?,
    open_stream: symbol(library, "?OpenStream@NetComClient@NlxNetCom@@QEAA_NQEB_W@Z")?,
    set_application_name: symbol(library, "?SetApplicationName@NetComClient@NlxNetCom@@QEAA_NQEB_W@Z")?,
    get_das_objects_and_types: symbol(
      library,
      "?GetDASObjectsAndTypesInternal@NetComClient@NlxNetCom@@AEAA_NAEAPEAPEA_W0AEAH@Z",
    )?,
    send_command: symbol(
      library,
      "?SendCommandInternal@NetComClient@NlxNetCom@@AEAA_NQEB_WAEAHAEAPEAPEA_W1@Z",
    )?,
    get_client_version_string: symbol(
      library,
      "?GetClientVersionStringInternal@NetComClient@NlxNetCom@@AEAAXAEAPEA_W@Z",
    )?,
    free_array: symbol(library, "?FreeArray@NetComClient@NlxNetCom@@AEAAXPEAX@Z")?,
    set_callback_csc: symbol(
      library,
      "?SetCallbackFunctionCSC@NetComClient@NlxNetCom@@QEAAXP6AXPEAXPEAUCRRec@NlxDataTypes@@HQEB_W@Z0@Z",
    )?,
    set_callback_ev: symbol(
      library,
      "?SetCallbackFunctionEV@NetComClient@NlxNetCom@@QEAAXP6AXPEAXPEAUEventRec@NlxDataTypes@@HQEB_W@Z0@Z",
    )?,
    set_callback_connection_lost: symbol(
      library,
      "?SetCallbackFunctionConnectionLost@NetComClient@NlxNetCom@@QEAAXP6AXPEAX@Z0@Z",
    )?,
    set_callback_message: symbol(
      library,
      "?SetCallbackFunctionMessage@NetComClient@NlxNetCom@@QEAAXP6AXPEAXHPEA_W@Z0@Z",
    )?,
  })
}

/// A null terminated UTF-16 copy of `s`, NetCom's wchar_t strings.
fn wide(s: &str) -> Vec<u16> {
  s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads a null terminated UTF-16 string. Empty for null.
unsafe fn from_wide(ptr: *const u16) -> String {
  if ptr.is_null() {
    return String::new();
  }
  let mut len = 0;
  while unsafe { *ptr.add(len) } != 0 {
    len += 1;
  }
  String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// One continuously sampled record.
pub struct CscRecord {
  pub sample_rate: u32,
  pub samples: Vec<i16>,
}

/// One event record.
pub struct EventRecord {
  /// Cheetah's timestamp, in microseconds.
  pub timestamp_us: u64,
  pub event_id: i16,
  pub ttl: i16,
  pub string: String,
}

/// Receives a client's streams. Called on NetCom's threads.
pub trait Handler: Send + Sync {
  fn csc(&self, object: &str, records: &[CscRecord]);
  fn event(&self, object: &str, records: &[EventRecord]);
  fn connection_lost(&self);
  fn message(&self, message_type: i32, message: &str);
}

/// Runs a callback body, swallowing panics: unwinding into NetCom's C++
/// frames would abort the process.
fn guard(name: &str, f: impl FnOnce()) {
  if catch_unwind(AssertUnwindSafe(f)).is_err() {
    println!("NetCom {name} callback panicked");
  }
}

type HandlerBox = Arc<dyn Handler>;

unsafe extern "C" fn csc_trampoline(context: *mut c_void, records: *const CRRec, count: i32, object: *const u16) {
  guard("CSC", || {
    let handler = unsafe { &*(context as *const HandlerBox) };
    let object = unsafe { from_wide(object) };
    let raw = if records.is_null() || count <= 0 {
      &[][..]
    } else {
      unsafe { std::slice::from_raw_parts(records, count as usize) }
    };
    let records: Vec<CscRecord> = raw
      .iter()
      .map(|r| {
        let valid = (r.dw_num_valid_samples as usize).min(MAX_CSC_SAMPLES);
        // Copied out: fields of a packed struct can't be borrowed.
        let samples = r.sn_samples;
        CscRecord {
          sample_rate: r.dw_sample_freq,
          samples: samples[..valid].to_vec(),
        }
      })
      .collect();
    handler.csc(&object, &records);
  });
}

unsafe extern "C" fn event_trampoline(context: *mut c_void, records: *const EventRec, count: i32, object: *const u16) {
  guard("event", || {
    let handler = unsafe { &*(context as *const HandlerBox) };
    let object = unsafe { from_wide(object) };
    let raw = if records.is_null() || count <= 0 {
      &[][..]
    } else {
      unsafe { std::slice::from_raw_parts(records, count as usize) }
    };
    let records: Vec<EventRecord> = raw
      .iter()
      .map(|r| {
        let bytes = r.event_string;
        EventRecord {
          timestamp_us: r.qw_time_stamp,
          event_id: r.nevent_id,
          ttl: r.nttl,
          string: event_string(&bytes),
        }
      })
      .collect();
    handler.event(&object, &records);
  });
}

unsafe extern "C" fn connection_lost_trampoline(context: *mut c_void) {
  guard("connection lost", || {
    let handler = unsafe { &*(context as *const HandlerBox) };
    handler.connection_lost();
  });
}

unsafe extern "C" fn message_trampoline(context: *mut c_void, message_type: i32, message: *mut u16) {
  guard("message", || {
    let handler = unsafe { &*(context as *const HandlerBox) };
    let message = unsafe { from_wide(message) };
    handler.message(message_type, &message);
  });
}

/// An event record's null padded string.
fn event_string(bytes: &[u8]) -> String {
  let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
  String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// A NetComClient. Dropping it disconnects and deletes the client; callbacks
/// stop before the drop returns.
pub struct Client {
  netcom: &'static NetCom,
  obj: Obj,
  /// Passed to NetCom as the callbacks' context; freed after the client is
  /// deleted.
  handler: *mut HandlerBox,
}

// NetComClient may be used from any one thread at a time; Client isn't Sync.
unsafe impl Send for Client {}

impl Client {
  /// Creates a client that reports everything it receives to `handler`.
  pub fn new(netcom: &'static NetCom, handler: Arc<dyn Handler>) -> Result<Client, String> {
    let f = &netcom.functions;
    let obj = unsafe { (f.get_new_client)() };
    if obj.is_null() {
      return Err("GetNewNetComClient returned null".to_string());
    }
    let handler = Box::into_raw(Box::new(handler));
    let context = handler as *mut c_void;
    unsafe {
      (f.set_callback_csc)(obj, Some(csc_trampoline), context);
      (f.set_callback_ev)(obj, Some(event_trampoline), context);
      (f.set_callback_connection_lost)(obj, Some(connection_lost_trampoline), context);
      (f.set_callback_message)(obj, Some(message_trampoline), context);
    }
    Ok(Client { netcom, obj, handler })
  }

  fn f(&self) -> &Functions {
    &self.netcom.functions
  }

  pub fn version(&self) -> String {
    let mut ptr: *mut u16 = std::ptr::null_mut();
    unsafe {
      (self.f().get_client_version_string)(self.obj, &mut ptr);
      let result = from_wide(ptr);
      if !ptr.is_null() {
        (self.f().free_array)(self.obj, ptr as *mut c_void);
      }
      result
    }
  }

  pub fn set_application_name(&self, name: &str) -> bool {
    let name = wide(name);
    unsafe { (self.f().set_application_name)(self.obj, name.as_ptr()) != 0 }
  }

  /// Connects to a NetCom server by PC name or IP address. Blocks.
  pub fn connect(&self, server: &str) -> bool {
    let server = wide(server);
    unsafe { (self.f().connect_to_server)(self.obj, server.as_ptr(), true) != 0 }
  }

  pub fn open_stream(&self, object: &str) -> bool {
    let object = wide(object);
    unsafe { (self.f().open_stream)(self.obj, object.as_ptr()) != 0 }
  }

  /// The server's data acquisition objects as (name, type) pairs.
  pub fn das_objects(&self) -> Option<Vec<(String, String)>> {
    let mut names: *mut *mut u16 = std::ptr::null_mut();
    let mut types: *mut *mut u16 = std::ptr::null_mut();
    let mut count: i32 = -1;
    let ok = unsafe { (self.f().get_das_objects_and_types)(self.obj, &mut names, &mut types, &mut count) != 0 };
    if !ok {
      return None;
    }
    // Mirrors NetComClient::GetDASObjectsAndTypes in NetComClient.h.
    let mut result = Vec::new();
    unsafe {
      for i in 0..count.max(0) as usize {
        let name = *names.add(i);
        let kind = *types.add(i);
        result.push((from_wide(name), from_wide(kind)));
        (self.f().free_array)(self.obj, name as *mut c_void);
        (self.f().free_array)(self.obj, kind as *mut c_void);
      }
      if !names.is_null() {
        (self.f().free_array)(self.obj, names as *mut c_void);
      }
      if !types.is_null() {
        (self.f().free_array)(self.obj, types as *mut c_void);
      }
    }
    Some(result)
  }

  /// Sends a Cheetah command. Returns whether the server reported success
  /// and its reply values, or None if the command couldn't be sent.
  pub fn send_command(&self, command: &str) -> Option<(bool, Vec<String>)> {
    let command = wide(command);
    let mut succeeded: i32 = -1;
    let mut replies: *mut *mut u16 = std::ptr::null_mut();
    let mut count: i32 = 0;
    let ok = unsafe {
      (self.f().send_command)(self.obj, command.as_ptr(), &mut succeeded, &mut replies, &mut count) != 0
    };
    if !ok {
      return None;
    }
    // Mirrors the vector overload of NetComClient::SendCommand.
    let mut values = Vec::new();
    unsafe {
      for i in 0..count.max(0) as usize {
        let reply = *replies.add(i);
        values.push(from_wide(reply));
        (self.f().free_array)(self.obj, reply as *mut c_void);
      }
      if !replies.is_null() {
        (self.f().free_array)(self.obj, replies as *mut c_void);
      }
    }
    Some((succeeded == 1, values))
  }
}

impl Drop for Client {
  fn drop(&mut self) {
    unsafe {
      if (self.f().are_we_connected)(self.obj) != 0 {
        (self.f().disconnect_from_server)(self.obj);
      }
      (self.f().delete_client)(self.obj);
      drop(Box::from_raw(self.handler));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn wide_strings_round_trip() {
    let w = wide("CSC1 µ");
    assert_eq!(*w.last().unwrap(), 0);
    assert_eq!(unsafe { from_wide(w.as_ptr()) }, "CSC1 µ");
    assert_eq!(unsafe { from_wide(std::ptr::null()) }, "");
  }

  #[test]
  fn event_strings_stop_at_null() {
    let mut bytes = [0u8; EVENT_STRING_SIZE];
    bytes[..5].copy_from_slice(b"TTL 1");
    assert_eq!(event_string(&bytes), "TTL 1");
    assert_eq!(event_string(&[b'a'; 4]), "aaaa");
  }

  #[cfg(not(all(windows, target_arch = "x86_64")))]
  #[test]
  fn load_fails_off_windows() {
    assert!(load().is_err());
  }
}
