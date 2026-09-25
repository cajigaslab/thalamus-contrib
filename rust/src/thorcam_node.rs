use std::cell::RefCell;
use std::os::raw::c_char;
use std::rc::{Rc, Weak};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::api::{
  AnalogData, DialogType, ImageData, ImageFormat, Json, MainThreadOnly, MainThreadToken, MocapData, Node,
  NodeConsts, NodeData, NodeToken, OffMainSignaler, OnDrop, PredropToken, Request, State,
  StateAction, StateKey, StateValue, THALAMUS_MODALITY_IMAGE, TaskScope, ThalamusAPI,
  ThalamusAPIThreadSafe, run_task,
};
use crate::image_viewer::{ImageFrame, ImageViewer};

type IsGetNumberOfCameras = unsafe extern "C" fn(*mut i32) -> i32;
type IsGetCameraList = unsafe extern "C" fn(*mut u8) -> i32;
type IsInitCamera = unsafe extern "C" fn(*mut u32, *mut std::ffi::c_void) -> i32;
type IsExitCamera = unsafe extern "C" fn(u32) -> i32;
type IsGetSensorInfo = unsafe extern "C" fn(u32, *mut SensorInfoRaw) -> i32;
type IsSetColorMode = unsafe extern "C" fn(u32, i32) -> i32;
type IsAllocImageMem = unsafe extern "C" fn(u32, i32, i32, i32, *mut *mut c_char, *mut i32) -> i32;
type IsFreeImageMem = unsafe extern "C" fn(u32, *mut c_char, i32) -> i32;
type IsAddToSequence = unsafe extern "C" fn(u32, *mut c_char, i32) -> i32;
type IsClearSequence = unsafe extern "C" fn(u32) -> i32;
type IsUnlockSeqBuf = unsafe extern "C" fn(u32, i32, *mut c_char) -> i32;
type IsCaptureVideo = unsafe extern "C" fn(u32, i32) -> i32;
type IsStopLiveVideo = unsafe extern "C" fn(u32, i32) -> i32;
type IsWaitForNextImage = unsafe extern "C" fn(u32, u32, *mut *mut c_char, *mut i32) -> i32;
type IsInitImageQueue = unsafe extern "C" fn(u32, i32) -> i32;
type IsExitImageQueue = unsafe extern "C" fn(u32) -> i32;
type IsSetFrameRate = unsafe extern "C" fn(u32, f64, *mut f64) -> i32;
type IsAoi = unsafe extern "C" fn(u32, u32, *mut std::ffi::c_void, u32) -> i32;
type IsExposure = unsafe extern "C" fn(u32, u32, *mut std::ffi::c_void, u32) -> i32;
type IsSetHardwareGain = unsafe extern "C" fn(u32, i32, i32, i32, i32) -> i32;
type IsSetAutoParameter = unsafe extern "C" fn(u32, i32, *mut f64, *mut f64) -> i32;

// Constants from uc480.h
const IS_USE_DEVICE_ID: u32 = 0x8000;
const IS_GET_FRAMERATE: f64 = 0x8000 as f64;
const IS_GET_MASTER_GAIN: i32 = 0x8000;
const IS_IGNORE_PARAMETER: i32 = -1;
const IS_SET_ENABLE_AUTO_GAIN: i32 = 0x8800;
const IS_SET_ENABLE_AUTO_SHUTTER: i32 = 0x8802;
const IS_SET_ENABLE_AUTO_FRAMERATE: i32 = 0x8806;
const IS_AOI_IMAGE_SET_AOI: u32 = 0x0001;
const IS_AOI_IMAGE_GET_AOI: u32 = 0x0002;
const IS_AOI_IMAGE_GET_POS_INC: u32 = 0x0011;
const IS_AOI_IMAGE_GET_SIZE_MIN: u32 = 0x0008;
const IS_AOI_IMAGE_GET_SIZE_INC: u32 = 0x0012;
const IS_EXPOSURE_CMD_GET_EXPOSURE: u32 = 7;
const IS_EXPOSURE_CMD_SET_EXPOSURE: u32 = 12;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IsRect {
  x: i32,
  y: i32,
  width: i32,
  height: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IsSize2d {
  width: i32,
  height: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IsPoint2d {
  x: i32,
  y: i32,
}

#[repr(C)]
struct Uc480CameraInfoRaw {
  dw_camera_id: u32,
  dw_device_id: u32,
  dw_sensor_id: u32,
  dw_in_use: u32,
  ser_no: [u8; 16],
  model: [u8; 16],
  dw_status: u32,
  dw_reserved: [u32; 2],
  full_model_name: [u8; 32],
  dw_reserved2: [u32; 5],
}
const _: () = assert!(std::mem::size_of::<Uc480CameraInfoRaw>() == 112);

#[repr(C)]
struct SensorInfoRaw {
  sensor_id: u16,
  str_sensor_name: [u8; 32],
  n_color_mode: c_char,
  _pad: u8,
  n_max_width: u32,
  n_max_height: u32,
  b_master_gain: i32,
  b_r_gain: i32,
  b_g_gain: i32,
  b_b_gain: i32,
  b_glob_shutter: i32,
  w_pixel_size: u16,
  n_upper_left_bayer_pixel: c_char,
  reserved: [c_char; 13],
}
const _: () = assert!(std::mem::size_of::<SensorInfoRaw>() == 80);

#[derive(Clone, Debug)]
pub struct CameraInfo {
  pub camera_id: u32,
  pub device_id: u32,
  pub serial_number: String,
  pub model: String,
  pub full_model_name: String,
}

struct Uc480Lib {
  _library: libloading::Library,
  _get_number_of_cameras: IsGetNumberOfCameras,
  _get_camera_list: IsGetCameraList,
  init_camera: IsInitCamera,
  exit_camera: IsExitCamera,
  get_sensor_info: IsGetSensorInfo,
  set_color_mode: IsSetColorMode,
  alloc_image_mem: IsAllocImageMem,
  free_image_mem: IsFreeImageMem,
  add_to_sequence: IsAddToSequence,
  clear_sequence: IsClearSequence,
  unlock_seq_buf: IsUnlockSeqBuf,
  capture_video: IsCaptureVideo,
  stop_live_video: IsStopLiveVideo,
  wait_for_next_image: IsWaitForNextImage,
  init_image_queue: IsInitImageQueue,
  exit_image_queue: IsExitImageQueue,
  set_frame_rate: IsSetFrameRate,
  aoi: IsAoi,
  exposure: IsExposure,
  set_hardware_gain: IsSetHardwareGain,
  set_auto_parameter: IsSetAutoParameter,
}

unsafe impl Send for Uc480Lib {}
unsafe impl Sync for Uc480Lib {}

static UC480: OnceLock<Result<(Uc480Lib, Vec<CameraInfo>), String>> = OnceLock::new();

/// Cameras (by device ID) with a capture thread, across all Thorcam nodes.
/// Each maps to the ID of the run that flagged it, so a finished run's
/// post_to_main cleanup can't clear the flag of a newer run of the same camera.
static RUNNING_CAMERAS: Mutex<BTreeMap<u32, u64>> = Mutex::new(BTreeMap::new());
static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(0);

fn camera_running(camera: &CameraInfo) -> bool {
  RUNNING_CAMERAS.lock().unwrap().contains_key(&camera.device_id)
}

fn mark_camera_running(camera: &CameraInfo) -> u64 {
  let run_id = NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed);
  RUNNING_CAMERAS.lock().unwrap().insert(camera.device_id, run_id);
  run_id
}

fn mark_camera_stopped(device_id: u32, run_id: u64) {
  let mut running = RUNNING_CAMERAS.lock().unwrap();
  if running.get(&device_id) == Some(&run_id) {
    running.remove(&device_id);
  }
}

fn cstr_bytes_to_string(bytes: &[u8]) -> String {
  let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
  String::from_utf8_lossy(&bytes[..end]).to_string()
}

fn load_fn<T: Copy>(library: &libloading::Library, name: &[u8]) -> Result<T, String> {
  unsafe {
    library
      .get::<T>(name)
      .map(|f| *f)
      .map_err(|e| format!("Failed to find {}: {}", String::from_utf8_lossy(name), e))
  }
}

fn load_uc480() -> Result<(Uc480Lib, Vec<CameraInfo>), String> {
  let lib_name = if cfg!(target_pointer_width = "64") {
    "uc480_64.dll"
  } else {
    "uc480.dll"
  };

  let library = unsafe {
    libloading::Library::new(lib_name).map_err(|e| format!("Failed to load {}: {}", lib_name, e))?
  };

  let get_number_of_cameras =
    load_fn::<IsGetNumberOfCameras>(&library, b"is_GetNumberOfCameras\0")?;
  let get_camera_list = load_fn::<IsGetCameraList>(&library, b"is_GetCameraList\0")?;
  let init_camera = load_fn::<IsInitCamera>(&library, b"is_InitCamera\0")?;
  let exit_camera = load_fn::<IsExitCamera>(&library, b"is_ExitCamera\0")?;
  let get_sensor_info = load_fn::<IsGetSensorInfo>(&library, b"is_GetSensorInfo\0")?;
  let set_color_mode = load_fn::<IsSetColorMode>(&library, b"is_SetColorMode\0")?;
  let alloc_image_mem = load_fn::<IsAllocImageMem>(&library, b"is_AllocImageMem\0")?;
  let free_image_mem = load_fn::<IsFreeImageMem>(&library, b"is_FreeImageMem\0")?;
  let add_to_sequence = load_fn::<IsAddToSequence>(&library, b"is_AddToSequence\0")?;
  let clear_sequence = load_fn::<IsClearSequence>(&library, b"is_ClearSequence\0")?;
  let unlock_seq_buf = load_fn::<IsUnlockSeqBuf>(&library, b"is_UnlockSeqBuf\0")?;
  let capture_video = load_fn::<IsCaptureVideo>(&library, b"is_CaptureVideo\0")?;
  let stop_live_video = load_fn::<IsStopLiveVideo>(&library, b"is_StopLiveVideo\0")?;
  let wait_for_next_image = load_fn::<IsWaitForNextImage>(&library, b"is_WaitForNextImage\0")?;
  let init_image_queue = load_fn::<IsInitImageQueue>(&library, b"is_InitImageQueue\0")?;
  let exit_image_queue = load_fn::<IsExitImageQueue>(&library, b"is_ExitImageQueue\0")?;
  let set_frame_rate = load_fn::<IsSetFrameRate>(&library, b"is_SetFrameRate\0")?;
  let aoi = load_fn::<IsAoi>(&library, b"is_AOI\0")?;
  let exposure = load_fn::<IsExposure>(&library, b"is_Exposure\0")?;
  let set_hardware_gain = load_fn::<IsSetHardwareGain>(&library, b"is_SetHardwareGain\0")?;
  let set_auto_parameter = load_fn::<IsSetAutoParameter>(&library, b"is_SetAutoParameter\0")?;

  let mut num_cameras: i32 = 0;
  let ret = unsafe { get_number_of_cameras(&mut num_cameras) };
  if ret != 0 {
    return Err(format!("is_GetNumberOfCameras returned error code {}", ret));
  }
  println!("ThorcamNode: found {} camera(s)", num_cameras);

  let cameras = if num_cameras > 0 {
    let info_words = std::mem::size_of::<Uc480CameraInfoRaw>() / 4;
    let buf_len = 1 + info_words * num_cameras as usize;
    let mut buf: Vec<u32> = vec![0u32; buf_len];
    buf[0] = num_cameras as u32;

    let ret = unsafe { get_camera_list(buf.as_mut_ptr() as *mut u8) };
    if ret != 0 {
      return Err(format!("is_GetCameraList returned error code {}", ret));
    }

    let info_base = unsafe { buf.as_ptr().add(1) as *const Uc480CameraInfoRaw };
    (0..num_cameras as usize)
      .map(|i| {
        let raw = unsafe { &*info_base.add(i) };
        CameraInfo {
          camera_id: raw.dw_camera_id,
          device_id: raw.dw_device_id,
          serial_number: cstr_bytes_to_string(&raw.ser_no),
          model: cstr_bytes_to_string(&raw.model),
          full_model_name: cstr_bytes_to_string(&raw.full_model_name),
        }
      })
      .collect()
  } else {
    vec![]
  };

  Ok((
    Uc480Lib {
      _library: library,
      _get_number_of_cameras: get_number_of_cameras,
      _get_camera_list: get_camera_list,
      init_camera,
      exit_camera,
      get_sensor_info,
      set_color_mode,
      alloc_image_mem,
      free_image_mem,
      add_to_sequence,
      clear_sequence,
      unlock_seq_buf,
      capture_video,
      stop_live_video,
      wait_for_next_image,
      init_image_queue,
      exit_image_queue,
      set_frame_rate,
      aoi,
      exposure,
      set_hardware_gain,
      set_auto_parameter,
    },
    cameras,
  ))
}

impl Uc480Lib {
  /// Opens the camera just long enough to run `f`. Only valid while the camera
  /// thread isn't holding the camera.
  fn with_camera<R>(&self, device_id: u32, f: impl FnOnce(u32) -> R) -> Option<R> {
    let mut h_cam = device_id | IS_USE_DEVICE_ID;
    let ret = unsafe { (self.init_camera)(&mut h_cam, std::ptr::null_mut()) };
    if ret != 0 {
      println!("ThorcamNode: is_InitCamera failed: {}", ret);
      return None;
    }
    let result = f(h_cam);
    unsafe { (self.exit_camera)(h_cam) };
    Some(result)
  }

  fn aoi_query<T: Default>(&self, h_cam: u32, command: u32) -> Option<T> {
    let mut value = T::default();
    let ret = unsafe {
      (self.aoi)(
        h_cam,
        command,
        &mut value as *mut T as *mut std::ffi::c_void,
        std::mem::size_of::<T>() as u32,
      )
    };
    if ret == 0 { Some(value) } else { None }
  }
}

fn camera_label(camera: &CameraInfo) -> String {
  format!("{}:{}", camera.model, camera.serial_number)
}

/// Latest camera frame handed off from the capture thread to the main
/// thread's preview window, overwritten in place each frame -- the viewer
/// only ever needs the most recent one and drops older frames on the floor.
#[derive(Clone)]
struct FrameSnapshot {
  data: Vec<u8>,
  width: u32,
  height: u32,
}

pub struct ThorcamNode {
  api: ThalamusAPI,
  state: State,
  _state_connection: OnDrop,
  camera_thread: Option<std::thread::JoinHandle<()>>,
  main_thread_token: MainThreadToken,
  viewer: Option<ImageViewer>,
  viewer_task: Option<TaskScope>,
  shared_frame: Arc<Mutex<Option<FrameSnapshot>>>,
  signaler: Arc<OffMainSignaler>,
}

/// Reads `view_geometry` as `(x, y, w, h)` if the key exists and is a list
/// with (at least) 4 int elements -- mirrors `read_geometry` in
/// image_viewer.cpp.
fn read_geometry(state: &State) -> Option<(i32, i32, i32, i32)> {
  let StateValue::List(list) = state.get(StateKey::String("view_geometry".to_string()))? else {
    return None;
  };
  let mut values = Vec::with_capacity(4);
  for entry in &list {
    if let StateValue::Int(v) = entry.val {
      values.push(v);
    }
  }
  if values.len() < 4 {
    return None;
  }
  Some((
    values[0] as i32,
    values[1] as i32,
    values[2] as i32,
    values[3] as i32,
  ))
}

/// Replaces `view_geometry` with a freshly built `[x, y, w, h]` list --
/// mirrors `write_geometry` in image_viewer.cpp, which likewise always
/// reassigns the whole array rather than mutating elements in place.
fn write_geometry(api: ThalamusAPI, state: &State, x: i32, y: i32, w: i32, h: i32) {
  let list = State::make_list(api);
  list.push_int(x as i64);
  list.push_int(y as i64);
  list.push_int(w as i64);
  list.push_int(h as i64);
  state.set(
    StateKey::String("view_geometry".to_string()),
    StateValue::List(list),
  );
}

/// Ticks the preview window on the main thread: uploads+presents whatever
/// frame the capture thread most recently deposited in `shared_frame`, and
/// once a second checks whether the window moved/resized to persist that
/// into `view_geometry`. Ends itself once the viewer is closed (by the
/// window's X button -- which also flips `View` back to false -- or by
/// close_viewer). Holds `inner` weakly so it never keeps ThorcamNodeInner
/// alive by itself; see close_viewer for why.
async fn viewer_tick_loop(
  api: ThalamusAPI,
  inner: Weak<RefCell<ThorcamNode>>,
  initial_geometry: (i32, i32, i32, i32),
) {
  let timer = api.create_timer();
  let mut last_geometry_check = Instant::now();
  let mut last_geometry = initial_geometry;

  loop {
    let _ = timer.sleep(Duration::from_millis(33)).await;

    let Some(inner) = inner.upgrade() else { break };

    // Taken before borrowing `.viewer` mutably below: RefCell's Deref
    // goes through a trait method, so the borrow checker can't split
    // `borrow.shared_frame` and `borrow.viewer` as disjoint fields the
    // way it could for a plain struct -- holding both borrows at once
    // (even of different fields) would conflict.
    let shared_frame = inner.borrow().shared_frame.clone();
    // Cloned, not taken: the renderer re-reads (and re-uploads) whatever
    // the latest frame is every tick, same as the C++ ImageViewer does
    // via node->has_image_data()/plane() -- it's not a one-shot queue.
    // Consuming it here instead would starve one of the two
    // frame-in-flight texture slots whenever a render tick lands between
    // camera frames (camera runs slower than the render loop), leaving
    // it a tick stale and making the display visibly flip back and forth
    // between the fresh and stale slot.
    let snapshot = shared_frame.lock().unwrap().clone();

    let mut borrow = inner.borrow_mut();
    let Some(viewer) = borrow.viewer.as_mut() else {
      break;
    };

    if viewer.should_close() {
      // Only clear `.viewer` here, never `.viewer_task`: this loop IS
      // that task, so dropping its own TaskScope from inside its own
      // poll would deadlock (TaskScope::drop locks the same Task
      // state this poll call is already holding). Leaving the
      // (now-idle) task in place is harmless -- it'll be replaced next
      // time open_viewer runs, or dropped along with the rest of
      // ThorcamNodeInner when the node itself goes away.
      borrow.viewer = None;
      drop(borrow);
      inner.borrow().state.set(
        StateKey::String("View".to_string()),
        StateValue::Bool(false),
      );
      break;
    }

    let frame = snapshot.as_ref().map(|s| ImageFrame {
      data: &s.data,
      width: s.width,
      height: s.height,
      format: ImageFormat::Gray,
    });
    viewer.update(frame);

    let now = Instant::now();
    let geometry_to_write = if now.duration_since(last_geometry_check) >= Duration::from_secs(1) {
      last_geometry_check = now;
      let geometry = viewer.position_size();
      if geometry != last_geometry {
        last_geometry = geometry;
        Some(geometry)
      } else {
        None
      }
    } else {
      None
    };
    drop(borrow);

    if let Some((x, y, w, h)) = geometry_to_write {
      write_geometry(api, &inner.borrow().state, x, y, w, h);
    }
  }
}

/// Opens the preview window if it isn't already open, seeding its position
/// from `view_geometry` (writing a default there first if it doesn't exist
/// yet, matching the C++ ImageViewer constructor).
fn open_viewer(inner: &Rc<RefCell<ThorcamNode>>) {
  if inner.borrow().viewer.is_some() {
    return;
  }
  let (api, state) = {
    let borrow = inner.borrow();
    (borrow.api, borrow.state.clone())
  };

  let geometry = match read_geometry(&state) {
    Some(geometry) => geometry,
    None => {
      let default_geometry = (100, 100, 400, 400);
      write_geometry(
        api,
        &state,
        default_geometry.0,
        default_geometry.1,
        default_geometry.2,
        default_geometry.3,
      );
      default_geometry
    }
  };
  let (x, y, w, h) = geometry;

  match ImageViewer::new(api, "Thorcam", x, y, w, h) {
    Ok(viewer) => {
      let mut borrow = inner.borrow_mut();
      borrow.viewer = Some(viewer);
      borrow.viewer_task = Some(run_task(viewer_tick_loop(
        api,
        Rc::downgrade(inner),
        geometry,
      )));
    }
    Err(e) => println!("ThorcamNode: failed to create image viewer: {}", e),
  }
}

/// Closes the preview window (if open), which immediately releases its
/// Vulkan/SDL resources. Deliberately does NOT touch `viewer_task`: this is
/// called synchronously from the "View" state callback, which viewer_tick_loop's
/// own should_close branch can trigger re-entrantly (state.set() dispatches
/// connected callbacks inline, not posted) -- and that branch runs from
/// inside the task's own poll, so dropping its TaskScope here would try to
/// re-lock the Task state the poll call already holds, deadlocking exactly
/// like the earlier Vulkan-queue double-lock bug. The idle task notices
/// `.viewer` is gone on its own next tick and ends itself; `viewer_task` is
/// only ever replaced (by open_viewer) or dropped (with the rest of
/// ThorcamNodeInner) from contexts that are never inside its own poll.
fn close_viewer(inner: &mut ThorcamNode) {
  inner.viewer = None;
}

struct ThorcamFrame {
  frame_ptr: *const u8,
  frame_len: usize,
  width: u64,
  height: u64,
  time: Duration,
  frame_interval: Duration,
}

impl NodeData for ThorcamFrame {
  fn time(&self) -> Duration {
    self.time
  }
  fn analog(&self) -> Option<&dyn AnalogData> {
    None
  }
  fn image(&self) -> Option<&dyn ImageData> {
    Some(self)
  }
  fn mocap(&self) -> Option<&dyn MocapData> {
    None
  }
}

impl ImageData for ThorcamFrame {
  fn plane(&self, _channel: i32) -> &[u8] {
    if self.frame_ptr.is_null() {
      &[]
    } else {
      unsafe { std::slice::from_raw_parts(self.frame_ptr, self.frame_len) }
    }
  }
  fn num_planes(&self) -> u64 {
    1
  }
  fn format(&self) -> ImageFormat {
    ImageFormat::Gray
  }
  fn width(&self) -> u64 {
    self.width
  }
  fn height(&self) -> u64 {
    self.height
  }
  fn frame_interval(&self) -> Duration {
    self.frame_interval
  }
}

impl NodeConsts for ThorcamNode {
  const MODALITIES: u32 = THALAMUS_MODALITY_IMAGE;
  const SIGNALS_OFFMAIN: bool = true;
}

struct CameraSettings {
  x: i32,
  y: i32,
  width: i32,
  height: i32,
  framerate: f64,
  exposure_us: f64,
  gain: f64,
  max_width: i32,
  max_height: i32,
}

impl ThorcamNode {
  fn stop_loop(&mut self) {
    self.signaler.block();
    self.camera_thread.take().map(|h| h.join());
  }

  fn state_to_settings(&self, state: &State) -> Option<CameraSettings> {
    let Some(StateValue::Int(x)) = state.get("OffsetX") else {
      return None;
    };
    let Some(StateValue::Int(y)) = state.get("OffsetY") else {
      return None;
    };
    let Some(StateValue::Int(width)) = state.get("Width") else {
      return None;
    };
    let Some(StateValue::Int(height)) = state.get("Height") else {
      return None;
    };
    let Some(StateValue::Float(framerate)) = state.get("AcquisitionFrameRate") else {
      return None;
    };
    let Some(StateValue::Float(exposure_us)) = state.get("ExposureTime") else {
      return None;
    };
    let Some(StateValue::Float(gain)) = state.get("Gain") else {
      return None;
    };
    let Some(StateValue::Int(max_width)) = state.get("WidthMax") else {
      return None;
    };
    let Some(StateValue::Int(max_height)) = state.get("HeightMax") else {
      return None;
    };

    Some(CameraSettings {
      x: x.try_into().unwrap(),
      y: y.try_into().unwrap(),
      width: width.try_into().unwrap(),
      height: height.try_into().unwrap(),
      framerate: framerate,
      exposure_us: exposure_us,
      gain: gain,
      max_width: max_width.try_into().unwrap(),
      max_height: max_height.try_into().unwrap(),
    })
  }

  fn settings_to_state(&self, settings: &CameraSettings, state: &State, overwrite: bool) {
    if overwrite || !state.contains_key("OffsetX") {
      state.set("OffsetX", settings.x);
    }
    if overwrite || !state.contains_key("OffsetY") {
      state.set("OffsetY", settings.y);
    }
    if overwrite || !state.contains_key("Width") {
      state.set("Width", settings.width);
    }
    if overwrite || !state.contains_key("Height") {
      state.set("Height", settings.height);
    }
    if overwrite || !state.contains_key("AcquisitionFrameRate") {
      state.set("AcquisitionFrameRate", settings.framerate);
    }
    if overwrite || !state.contains_key("ExposureTime") {
      state.set("ExposureTime", settings.exposure_us);
    }
    if overwrite || !state.contains_key("Gain") {
      state.set("Gain", settings.gain);
    }
    if overwrite || !state.contains_key("WidthMax") {
      state.set("WidthMax", settings.max_width);
    }
    if overwrite || !state.contains_key("HeightMax") {
      state.set("HeightMax", settings.max_height);
    }
  }

  fn settings_to_camera(settings: &CameraSettings, h_cam: u32) {
    let (lib, _) = UC480.get().unwrap().as_ref().unwrap();

    let mut off = 0.0f64;
    let mut unused = 0.0f64;
    unsafe { (lib.set_auto_parameter)(h_cam, IS_SET_ENABLE_AUTO_GAIN, &mut off, &mut unused) };
    unsafe { (lib.set_auto_parameter)(h_cam, IS_SET_ENABLE_AUTO_SHUTTER, &mut off, &mut unused) };
    unsafe { (lib.set_auto_parameter)(h_cam, IS_SET_ENABLE_AUTO_FRAMERATE, &mut off, &mut unused) };

    let CameraSettings {
      x, 
      y, 
      width, 
      height, 
      framerate, 
      exposure_us, 
      gain, 
      ..
    } = settings;

    ThorcamNode::sync_aoi(h_cam, *x as i64, *y as i64, *width as i64, *height as i64);

    let mut actual = 0.0;
    let ret = unsafe { (lib.set_frame_rate)(h_cam, *framerate, &mut actual) };
    if ret != 0 {
      println!("ThorcamNode: is_SetFrameRate failed: {}", ret);
    }

    let mut exposure_ms = exposure_us / 1000.0;
    let ret = unsafe {
      (lib.exposure)(
        h_cam,
        IS_EXPOSURE_CMD_SET_EXPOSURE,
        &mut exposure_ms as *mut f64 as *mut std::ffi::c_void,
        8,
      )
    };
    if ret != 0 {
      println!("ThorcamNode: is_Exposure(SET_EXPOSURE) failed: {}", ret);
    }

    let master = gain.round().clamp(0.0, 100.0) as i32;
    let ret = unsafe {
      (lib.set_hardware_gain)(
        h_cam,
        master,
        IS_IGNORE_PARAMETER,
        IS_IGNORE_PARAMETER,
        IS_IGNORE_PARAMETER,
      )
    };
    if ret != 0 {
      println!("ThorcamNode: is_SetHardwareGain failed: {}", ret);
    }
  }

  fn camera_to_settings(h_cam: u32) -> Option<CameraSettings> {
    let (lib, _) = UC480.get().unwrap().as_ref().unwrap();

    let mut info = unsafe { std::mem::zeroed::<SensorInfoRaw>() };
    let ret = unsafe { (lib.get_sensor_info)(h_cam, &mut info) };
    if ret != 0 {
      println!("ThorcamNode: is_GetSensorInfo failed: {}", ret);
      return None;
    }

    let Some(rect) = lib.aoi_query::<IsRect>(h_cam, IS_AOI_IMAGE_GET_AOI) else {
      println!("ThorcamNode: is_AOI(GET_AOI) failed");
      return None;
    };

    let mut exposure_us = 0.0;
    let ret = unsafe {
      (lib.exposure)(
        h_cam,
        IS_EXPOSURE_CMD_GET_EXPOSURE,
        &mut exposure_us as *mut f64 as *mut std::ffi::c_void,
        8,
      )
    };
    exposure_us *= 1000.0;
    if ret != 0 {
      println!("ThorcamNode: is_Exposure(GET_EXPOSURE) failed: {}", ret);
      return None;
    }

    let mut framerate = 0.0;
    let ret = unsafe { (lib.set_frame_rate)(h_cam, IS_GET_FRAMERATE, &mut framerate) };
    if ret != 0 {
      println!("ThorcamNode: is_SetFrameRate(GET) failed: {}", ret);
      return None;
    }

    let gain = unsafe {
      (lib.set_hardware_gain)(
        h_cam,
        IS_GET_MASTER_GAIN,
        IS_IGNORE_PARAMETER,
        IS_IGNORE_PARAMETER,
        IS_IGNORE_PARAMETER,
      )
    };

    Some(CameraSettings { 
      x: rect.x, 
      y: rect.y, 
      width: rect.width, 
      height: rect.height, 
      framerate, 
      exposure_us, 
      gain: gain as f64, 
      max_width: info.n_max_width.try_into().unwrap(),
      max_height: info.n_max_height.try_into().unwrap(),
     })
  }

  fn sync_aoi(h_cam: u32, x: i64, y: i64, width: i64, height: i64) {
    let (lib, _) = UC480.get().unwrap().as_ref().unwrap();
    
    let size_min = lib
      .aoi_query::<IsSize2d>(h_cam, IS_AOI_IMAGE_GET_SIZE_MIN)
      .unwrap_or(IsSize2d {
        width: 1,
        height: 1,
      });
    let size_inc = lib
      .aoi_query::<IsSize2d>(h_cam, IS_AOI_IMAGE_GET_SIZE_INC)
      .unwrap_or(IsSize2d {
        width: 1,
        height: 1,
      });
    let pos_inc = lib
      .aoi_query::<IsPoint2d>(h_cam, IS_AOI_IMAGE_GET_POS_INC)
      .unwrap_or(IsPoint2d { x: 1, y: 1 });

    let x = (x as i32) / pos_inc.x * pos_inc.x;
    let y = (y as i32) / pos_inc.y * pos_inc.y;
    let width = size_min.width.max((width as i32) / size_inc.width * size_inc.width);
    let height = size_min.height.max((height as i32) / size_inc.height * size_inc.height);
    let mut rect= IsRect { x, y, width, height };
    
    let ret = unsafe {
      (lib.aoi)(
        h_cam,
        IS_AOI_IMAGE_SET_AOI,
        &mut rect as *mut IsRect as *mut std::ffi::c_void,
        std::mem::size_of::<IsRect>() as u32,
      )
    };
    if ret != 0 {
      println!("ThorcamNode: is_AOI(SET_AOI) failed: {}", ret);
    }
  }

  fn get_camera(&self) -> Option<CameraInfo> {
    let (_, cameras) = UC480.get().unwrap().as_ref().unwrap();
    let camera_id = match self.state.get("Camera") {
      Some(StateValue::String(v)) => {
        v
      },
      _ => return None,
    };
    let camera_opt = cameras.iter().find(|c| {
      camera_label(c) == camera_id
    });

    match camera_opt {
      Some(c) => Some(c.clone()),
      None => None
    }
  }

  fn init_camera(camera: &CameraInfo) -> (u32, OnDrop) {
    let (lib, _) = UC480.get().unwrap().as_ref().unwrap();

    let mut h_cam = camera.device_id | IS_USE_DEVICE_ID;
    let ret = unsafe { (lib.init_camera)(&mut h_cam, std::ptr::null_mut()) };
    if ret != 0 {
      println!("ThorcamNode: is_InitCamera failed: {}", ret);
      return (0, OnDrop::noop());
    } else {
      return (h_cam, OnDrop::new(move || {
        unsafe { (lib.exit_camera)(h_cam) };
      }))
    }
  }

  fn sync(&mut self) -> Option<CameraSettings> {
    let Some(StateValue::Dict(mut desired)) = self.state.get("Desired") else {
      return None;
    };
    let Some(StateValue::Dict(actual)) = self.state.get("Actual") else {
      return None;
    };
    let Some(camera) = self.get_camera() else {
      return None;
    };
    if camera_running(&camera) {
      println!("ThorcamNode: can't sync running camera {}", camera_label(&camera));
      return None;
    }

    let (h_cam, _drop) = ThorcamNode::init_camera(&camera);

    //Create d2, fill it with the camera's current settings, merge desired into in.
    //This results in a d2 containing the camera's current settings with requested settings.
    //d2 is then saved to the state and written to the camera.
    let Some(current) = ThorcamNode::camera_to_settings(h_cam) else {
      return None;
    };
    let mut d2 = State::make_dict(self.api);
    self.settings_to_state(&current, &d2, true);
    d2.merge(&desired);
    desired.assign(&d2);

    //Write the desired settings to the camera and read back the result.
    let Some(settings) = self.state_to_settings(&d2) else {
      return None;
    };
    ThorcamNode::settings_to_camera(&settings, h_cam);
    let Some(applied) = ThorcamNode::camera_to_settings(h_cam) else {
      return None;
    };
    self.settings_to_state(&applied, &actual, true);

    Some(applied)
  }

  fn read_camera(&mut self) {
    let Some(StateValue::Dict(actual)) = self.state.get("Actual") else {
      return;
    };
    let Some(StateValue::Dict(mut desired)) = self.state.get("Desired") else {
      return;
    };
    let Some(camera) = self.get_camera() else {
      return;
    };
    if camera_running(&camera) {
      self.api.show_dialog("Thorcam", "Can't read settings from a running camera", DialogType::Warn);
      return;
    }

    let (h_cam, _cam_drop) = ThorcamNode::init_camera(&camera);
    let Some(current) = ThorcamNode::camera_to_settings(h_cam) else {
      return;
    };
    self.settings_to_state(&current, &actual, true);

    let mut d2 = State::make_dict(self.api);
    self.settings_to_state(&current, &d2, true);
    d2.merge(&desired);
    desired.assign(&d2);
  }

  fn camera_loop(
    api: ThalamusAPIThreadSafe, 
    signaler: Arc<OffMainSignaler>, 
    camera: CameraInfo, 
    shared_frame: Arc<Mutex<Option<FrameSnapshot>>>, 
    settings: Option<CameraSettings>) {
    let (h_cam, _cam_drop) = ThorcamNode::init_camera(&camera);

    match settings {
      Some(settings) => ThorcamNode::settings_to_camera(&settings, h_cam),
      None => {}
    };

    let Some(settings) = ThorcamNode::camera_to_settings(h_cam) else {
      println!("ThorcamNode: Failed to read camera settings");
      return;
    };
    let width = settings.width as u64;
    let height = settings.height as u64;
    let frame_interval = if settings.framerate > 0.0 {
      Duration::from_secs_f64(1.0 / settings.framerate)
    } else {
      Duration::from_nanos(16_666_667)
    };

    let (lib, _) = UC480.get().unwrap().as_ref().unwrap();

    unsafe { (lib.set_color_mode)(h_cam, 6) }; // IS_CM_MONO8

    // Allocate a ring buffer of 3 frames for is_WaitForNextImage
    const NUM_BUFS: usize = 3;
    let mut bufs: Vec<(*mut c_char, i32)> = Vec::with_capacity(NUM_BUFS);
    let mut alloc_ok = true;
    for _ in 0..NUM_BUFS {
      let mut p_mem: *mut c_char = std::ptr::null_mut();
      let mut mem_id: i32 = 0;
      let ret = unsafe {
        (lib.alloc_image_mem)(
          h_cam,
          width as i32,
          height as i32,
          8,
          &mut p_mem,
          &mut mem_id,
        )
      };
      if ret != 0 {
        println!("ThorcamNode: is_AllocImageMem failed: {}", ret);
        alloc_ok = false;
        break;
      }
      let ret = unsafe { (lib.add_to_sequence)(h_cam, p_mem, mem_id) };
      if ret != 0 {
        println!("ThorcamNode: is_AddToSequence failed: {}", ret);
        unsafe { (lib.free_image_mem)(h_cam, p_mem, mem_id) };
        alloc_ok = false;
        break;
      }
      bufs.push((p_mem, mem_id));
    }
    if !alloc_ok {
      for (p, id) in bufs {
        unsafe { (lib.free_image_mem)(h_cam, p, id) };
      }
      unsafe { (lib.exit_camera)(h_cam) };
      return;
    }

    let ret = unsafe { (lib.init_image_queue)(h_cam, 0) };
    if ret != 0 {
      println!("ThorcamNode: is_InitImageQueue failed: {}", ret);
      unsafe { (lib.clear_sequence)(h_cam) };
      for (p, id) in bufs {
        unsafe { (lib.free_image_mem)(h_cam, p, id) };
      }
      unsafe { (lib.exit_camera)(h_cam) };
      return;
    }

    let ret = unsafe { (lib.capture_video)(h_cam, 0) }; // IS_DONT_WAIT
    if ret != 0 {
      println!("ThorcamNode: is_CaptureVideo failed: {}", ret);
      unsafe { (lib.exit_image_queue)(h_cam) };
      unsafe { (lib.clear_sequence)(h_cam) };
      for (p, id) in bufs {
        unsafe { (lib.free_image_mem)(h_cam, p, id) };
      }
      unsafe { (lib.exit_camera)(h_cam) };
      return;
    }

    let frame_size = (width * height) as usize;

    loop {
      let mut next_mem: *mut c_char = std::ptr::null_mut();
      let mut next_id: i32 = 0;
      let ret = unsafe { (lib.wait_for_next_image)(h_cam, 1000, &mut next_mem, &mut next_id) };
      if ret != 0 {
        continue;
      }

      let data = ThorcamFrame {
        frame_ptr: next_mem as *const u8,
        frame_len: frame_size,
        width,
        height,
        time: api.time(),
        frame_interval,
      };

      // Publishes directly from this thread; subscribers read plane() synchronously
      // before this call returns. Ignore if the node was destroyed concurrently.
      match signaler.ready(&data) {
        Ok(v) => {
          if !v {
            break;
          }
        }
        Err(_) => break,
      };

      // Copied out before unlock_seq_buf below hands the buffer back to the
      // driver (which may overwrite it): the main-thread preview window
      // reads this asynchronously, well after this call returns.
      *shared_frame.lock().unwrap() = Some(FrameSnapshot {
        data: data.plane(0).to_vec(),
        width: width as u32,
        height: height as u32,
      });

      unsafe { (lib.unlock_seq_buf)(h_cam, next_id, next_mem) };
    }
  }

  fn on_state(rc_this: Rc<RefCell<Self>>, _source: State, _action: StateAction, key: StateValue, value: StateValue) {
    let StateValue::String(key_str) = key else {
      return;
    };

    match key_str.as_str() {
      "Running" => {
        let mut this = rc_this.borrow_mut();
        this.stop_loop();
        if value == StateValue::Bool(true) {
          // Refuse to start if there's no camera or another run (possibly
          // another node's) already has it, rather than taking it over.
          let camera = match this.get_camera() {
            Some(c) if camera_running(&c) => {
              Err(format!("Camera {} is already running", camera_label(&c)))
            }
            Some(c) => Ok(c),
            None => Err("No camera selected".to_string()),
          };
          let camera = match camera {
            Ok(camera) => camera,
            Err(message) => {
              this.api.show_dialog("Thorcam", &message, DialogType::Error);
              // Release the borrow first: if this set notifies synchronously it
              // re-enters on_state, which borrows the node mutably.
              let state = this.state.clone();
              drop(this);
              state.set("Running", false);
              return;
            }
          };

          let api = this.api.thread_safe();
          let signaler = this.signaler.clone();
          signaler.unblock();
          let wrapped_state = MainThreadOnly::new(this.state.clone(), this.main_thread_token);
          let settings = this.sync();
          let shared_frame = this.shared_frame.clone();
          let device_id = camera.device_id;
          let run_id = mark_camera_running(&camera);
          this.camera_thread = Some(std::thread::spawn(move || {
            ThorcamNode::camera_loop(api, signaler, camera, shared_frame, settings);
            api.post_to_main(move |main_thread_token| {
              mark_camera_stopped(device_id, run_id);
              let state = wrapped_state.take(main_thread_token);
              state.set("Running", false);
            });
          }));
        }
      },
      "Camera" => {
        rc_this.borrow_mut().read_camera();
      },
      "View" => {
        if value == StateValue::Bool(true) {
          open_viewer(&rc_this);
        } else {
          close_viewer(&mut rc_this.borrow_mut());
        }
      }
      _ => {}
    }
  }
}

impl ThorcamNode {
  fn process(&mut self, handle: Request, request: Json) {
    let api = self.api;
    let response = match serde_json::from_str::<serde_json::Value>(&request.to_string()) {
      Ok(serde_json::Value::String(s)) if s == "get_cameras" => {
        let cameras: Vec<serde_json::Value> = UC480
          .get()
          .and_then(|r| r.as_ref().ok())
          .map(|(_, cams)| {
            cams
              .iter()
              .map(|c| serde_json::Value::String(camera_label(c)))
              .collect()
          })
          .unwrap_or_default();
        serde_json::to_string(&cameras).unwrap()
      }
      Ok(serde_json::Value::String(s)) if s == "sync_config" => {
        if self.get_camera().is_some_and(|camera| camera_running(&camera)) {
          self.api.show_dialog("Thorcam", "Can't sync running camera", DialogType::Error);
        } else {
          self.sync();
        }
        "null".to_string()
      }
      _ => "null".to_string(),
    };
    handle.respond(&Json::from_string(api, &response));
  }
}

impl Node for ThorcamNode {
  fn new(
    api: ThalamusAPI,
    node_token: NodeToken,
    state: State,
    main_thread_token: MainThreadToken,
  ) -> Rc<RefCell<Self>> {
    let init_result = UC480.get_or_init(load_uc480);
    match init_result {
      Ok((_, cameras)) => {
        for cam in cameras {
          println!(
            "ThorcamNode: camera — id={}, serial={}, model={}, full_name={}",
            cam.camera_id, cam.serial_number, cam.model, cam.full_model_name
          );
        }
      }
      Err(e) => println!("ThorcamNode: uc480 init failed: {}", e),
    }

    let result = Rc::new_cyclic(|weak: &Weak<RefCell<Self>>| {
      let signaler = Arc::new(OffMainSignaler::new(api, node_token.clone()));

      let weak2 = weak.clone();
      let callback = move |source, action, key, value| {
        if let Some(strong) = weak2.upgrade() {
          ThorcamNode::on_state(strong, source, action, key, value);
        }
      };
      let _state_connection = state.connect(callback);
      RefCell::new(Self {
        api,
        _state_connection,
        main_thread_token,
        state: state.clone(),
        signaler,
        camera_thread: None,
        viewer: None,
        viewer_task: None,
        shared_frame: Arc::new(Mutex::new(None)),
      })
    });

    let weak = Rc::downgrade(&result);
    node_token.set_process(move |handle, request| {
      if let Some(strong) = weak.upgrade() {
        strong.borrow_mut().process(handle, request);
      }
    });

    state.recap();
    result.borrow_mut().sync();
    result
  }
}

impl Drop for ThorcamNode {
  fn drop(&mut self) {
    close_viewer(self);
    self.stop_loop();
  }
}
