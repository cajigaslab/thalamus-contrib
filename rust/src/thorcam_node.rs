use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::api::{
    AnalogData, ImageData, ImageFormat, Json, MainThreadOnly, MainThreadToken, MocapData, Node, NodeData, NodeToken, OffMainSignaler, OnDrop, PredropToken, Request, State, StateAction, StateKey, StateValue, THALAMUS_MODALITY_IMAGE, TaskScope, ThalamusAPI, ThalamusAPIThreadSafe, run_task,
};
use crate::image_viewer::{ImageFrame, ImageViewer};

type IsGetNumberOfCameras = unsafe extern "C" fn(*mut i32) -> i32;
type IsGetCameraList     = unsafe extern "C" fn(*mut u8) -> i32;
type IsInitCamera        = unsafe extern "C" fn(*mut u32, *mut std::ffi::c_void) -> i32;
type IsExitCamera        = unsafe extern "C" fn(u32) -> i32;
type IsGetSensorInfo     = unsafe extern "C" fn(u32, *mut SensorInfoRaw) -> i32;
type IsSetColorMode      = unsafe extern "C" fn(u32, i32) -> i32;
type IsAllocImageMem     = unsafe extern "C" fn(u32, i32, i32, i32, *mut *mut i8, *mut i32) -> i32;
type IsFreeImageMem      = unsafe extern "C" fn(u32, *mut i8, i32) -> i32;
type IsAddToSequence     = unsafe extern "C" fn(u32, *mut i8, i32) -> i32;
type IsClearSequence     = unsafe extern "C" fn(u32) -> i32;
type IsUnlockSeqBuf      = unsafe extern "C" fn(u32, i32, *mut i8) -> i32;
type IsCaptureVideo      = unsafe extern "C" fn(u32, i32) -> i32;
type IsStopLiveVideo     = unsafe extern "C" fn(u32, i32) -> i32;
type IsWaitForNextImage  = unsafe extern "C" fn(u32, u32, *mut *mut i8, *mut i32) -> i32;
type IsInitImageQueue    = unsafe extern "C" fn(u32, i32) -> i32;
type IsExitImageQueue    = unsafe extern "C" fn(u32) -> i32;
type IsSetFrameRate      = unsafe extern "C" fn(u32, f64, *mut f64) -> i32;
type IsAoi               = unsafe extern "C" fn(u32, u32, *mut std::ffi::c_void, u32) -> i32;
type IsExposure          = unsafe extern "C" fn(u32, u32, *mut std::ffi::c_void, u32) -> i32;
type IsSetHardwareGain   = unsafe extern "C" fn(u32, i32, i32, i32, i32) -> i32;
type IsSetAutoParameter  = unsafe extern "C" fn(u32, i32, *mut f64, *mut f64) -> i32;

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
struct IsRect { x: i32, y: i32, width: i32, height: i32 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IsSize2d { width: i32, height: i32 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IsPoint2d { x: i32, y: i32 }

#[repr(C)]
struct Uc480CameraInfoRaw {
    dw_camera_id: u32, dw_device_id: u32, dw_sensor_id: u32, dw_in_use: u32,
    ser_no: [u8; 16], model: [u8; 16],
    dw_status: u32, dw_reserved: [u32; 2],
    full_model_name: [u8; 32],
    dw_reserved2: [u32; 5],
}
const _: () = assert!(std::mem::size_of::<Uc480CameraInfoRaw>() == 112);

#[repr(C)]
struct SensorInfoRaw {
    sensor_id: u16,
    str_sensor_name: [u8; 32],
    n_color_mode: i8,
    _pad: u8,
    n_max_width: u32,
    n_max_height: u32,
    b_master_gain: i32,
    b_r_gain: i32,
    b_g_gain: i32,
    b_b_gain: i32,
    b_glob_shutter: i32,
    w_pixel_size: u16,
    n_upper_left_bayer_pixel: i8,
    reserved: [i8; 13],
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
    _get_camera_list:       IsGetCameraList,
    init_camera:           IsInitCamera,
    exit_camera:           IsExitCamera,
    get_sensor_info:       IsGetSensorInfo,
    set_color_mode:        IsSetColorMode,
    alloc_image_mem:       IsAllocImageMem,
    free_image_mem:        IsFreeImageMem,
    add_to_sequence:       IsAddToSequence,
    clear_sequence:        IsClearSequence,
    unlock_seq_buf:        IsUnlockSeqBuf,
    capture_video:         IsCaptureVideo,
    stop_live_video:       IsStopLiveVideo,
    wait_for_next_image:   IsWaitForNextImage,
    init_image_queue:      IsInitImageQueue,
    exit_image_queue:      IsExitImageQueue,
    set_frame_rate:        IsSetFrameRate,
    aoi:                   IsAoi,
    exposure:              IsExposure,
    set_hardware_gain:     IsSetHardwareGain,
    set_auto_parameter:    IsSetAutoParameter,
}

unsafe impl Send for Uc480Lib {}
unsafe impl Sync for Uc480Lib {}

static UC480: OnceLock<Result<(Uc480Lib, Vec<CameraInfo>), String>> = OnceLock::new();

fn cstr_bytes_to_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

fn load_fn<T: Copy>(library: &libloading::Library, name: &[u8]) -> Result<T, String> {
    unsafe {
        library.get::<T>(name)
            .map(|f| *f)
            .map_err(|e| format!("Failed to find {}: {}", String::from_utf8_lossy(name), e))
    }
}

fn load_uc480() -> Result<(Uc480Lib, Vec<CameraInfo>), String> {
    let lib_name = if cfg!(target_pointer_width = "64") { "uc480_64.dll" } else { "uc480.dll" };

    let library = unsafe {
        libloading::Library::new(lib_name)
            .map_err(|e| format!("Failed to load {}: {}", lib_name, e))?
    };

    let get_number_of_cameras = load_fn::<IsGetNumberOfCameras>(&library, b"is_GetNumberOfCameras\0")?;
    let get_camera_list       = load_fn::<IsGetCameraList>(&library,      b"is_GetCameraList\0")?;
    let init_camera           = load_fn::<IsInitCamera>(&library,         b"is_InitCamera\0")?;
    let exit_camera           = load_fn::<IsExitCamera>(&library,         b"is_ExitCamera\0")?;
    let get_sensor_info       = load_fn::<IsGetSensorInfo>(&library,      b"is_GetSensorInfo\0")?;
    let set_color_mode        = load_fn::<IsSetColorMode>(&library,       b"is_SetColorMode\0")?;
    let alloc_image_mem       = load_fn::<IsAllocImageMem>(&library,      b"is_AllocImageMem\0")?;
    let free_image_mem        = load_fn::<IsFreeImageMem>(&library,       b"is_FreeImageMem\0")?;
    let add_to_sequence       = load_fn::<IsAddToSequence>(&library,      b"is_AddToSequence\0")?;
    let clear_sequence        = load_fn::<IsClearSequence>(&library,      b"is_ClearSequence\0")?;
    let unlock_seq_buf        = load_fn::<IsUnlockSeqBuf>(&library,       b"is_UnlockSeqBuf\0")?;
    let capture_video         = load_fn::<IsCaptureVideo>(&library,       b"is_CaptureVideo\0")?;
    let stop_live_video       = load_fn::<IsStopLiveVideo>(&library,      b"is_StopLiveVideo\0")?;
    let wait_for_next_image   = load_fn::<IsWaitForNextImage>(&library,   b"is_WaitForNextImage\0")?;
    let init_image_queue      = load_fn::<IsInitImageQueue>(&library,     b"is_InitImageQueue\0")?;
    let exit_image_queue      = load_fn::<IsExitImageQueue>(&library,     b"is_ExitImageQueue\0")?;
    let set_frame_rate        = load_fn::<IsSetFrameRate>(&library,       b"is_SetFrameRate\0")?;
    let aoi                   = load_fn::<IsAoi>(&library,                b"is_AOI\0")?;
    let exposure              = load_fn::<IsExposure>(&library,           b"is_Exposure\0")?;
    let set_hardware_gain     = load_fn::<IsSetHardwareGain>(&library,    b"is_SetHardwareGain\0")?;
    let set_auto_parameter    = load_fn::<IsSetAutoParameter>(&library,   b"is_SetAutoParameter\0")?;

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
        (0..num_cameras as usize).map(|i| {
            let raw = unsafe { &*info_base.add(i) };
            CameraInfo {
                camera_id: raw.dw_camera_id,
                device_id: raw.dw_device_id,
                serial_number: cstr_bytes_to_string(&raw.ser_no),
                model: cstr_bytes_to_string(&raw.model),
                full_model_name: cstr_bytes_to_string(&raw.full_model_name),
            }
        }).collect()
    } else {
        vec![]
    };

    Ok((Uc480Lib {
        _library: library,
        _get_number_of_cameras: get_number_of_cameras, _get_camera_list: get_camera_list,
        init_camera, exit_camera, get_sensor_info,
        set_color_mode, alloc_image_mem, free_image_mem,
        add_to_sequence, clear_sequence, unlock_seq_buf,
        capture_video, stop_live_video, wait_for_next_image,
        init_image_queue, exit_image_queue, set_frame_rate,
        aoi, exposure, set_hardware_gain, set_auto_parameter,
    }, cameras))
}

/// The camera's actual settings, mirrored into the node's "Camera Values" dict
/// (and, on a sync, the config keys of the same name) like GenicamNode does.
/// ExposureTime is in microseconds, as in the genicam widget.
#[derive(Clone, Copy, Debug)]
struct CameraValues {
    width_max:     i64,
    height_max:    i64,
    width:         i64,
    height:        i64,
    offset_x:      i64,
    offset_y:      i64,
    exposure_time: f64,
    frame_rate:    f64,
    gain:          f64,
}

/// What the config asks for; `None` for keys that aren't set.
#[derive(Clone, Copy, Debug, Default)]
struct RequestedValues {
    width:         Option<i64>,
    height:        Option<i64>,
    offset_x:      Option<i64>,
    offset_y:      Option<i64>,
    exposure_time: Option<f64>,
    frame_rate:    Option<f64>,
    gain:          Option<f64>,
}

impl RequestedValues {
    fn read(state: &State) -> RequestedValues {
        let number = |key: &str| -> Option<f64> {
            state.get(StateKey::String(key.to_string())).and_then(|v| f64::try_from(v).ok())
        };
        RequestedValues {
            width:         number("Width").map(|v| v as i64),
            height:        number("Height").map(|v| v as i64),
            offset_x:      number("OffsetX").map(|v| v as i64),
            offset_y:      number("OffsetY").map(|v| v as i64),
            exposure_time: number("ExposureTime"),
            frame_rate:    number("AcquisitionFrameRate"),
            gain:          number("Gain"),
        }
    }
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
            (self.aoi)(h_cam, command, &mut value as *mut T as *mut std::ffi::c_void, std::mem::size_of::<T>() as u32)
        };
        if ret == 0 { Some(value) } else { None }
    }

    /// Same as GenicamNode::sanitize_camera: manual gain/exposure/frame rate.
    fn sanitize(&self, h_cam: u32) {
        for param in [IS_SET_ENABLE_AUTO_GAIN, IS_SET_ENABLE_AUTO_SHUTTER, IS_SET_ENABLE_AUTO_FRAMERATE] {
            let mut off = 0.0f64;
            let mut unused = 0.0f64;
            // Not every camera has every auto mode, so errors are expected and ignored.
            unsafe { (self.set_auto_parameter)(h_cam, param, &mut off, &mut unused) };
        }
    }

    fn read_values(&self, h_cam: u32) -> Option<CameraValues> {
        let mut info = unsafe { std::mem::zeroed::<SensorInfoRaw>() };
        let ret = unsafe { (self.get_sensor_info)(h_cam, &mut info) };
        if ret != 0 {
            println!("ThorcamNode: is_GetSensorInfo failed: {}", ret);
            return None;
        }
        let Some(rect) = self.aoi_query::<IsRect>(h_cam, IS_AOI_IMAGE_GET_AOI) else {
            println!("ThorcamNode: is_AOI(GET_AOI) failed");
            return None;
        };

        let mut exposure_ms = 0.0f64;
        let ret = unsafe {
            (self.exposure)(h_cam, IS_EXPOSURE_CMD_GET_EXPOSURE, &mut exposure_ms as *mut f64 as *mut std::ffi::c_void, 8)
        };
        if ret != 0 {
            println!("ThorcamNode: is_Exposure(GET_EXPOSURE) failed: {}", ret);
        }

        let mut frame_rate = 0.0f64;
        let ret = unsafe { (self.set_frame_rate)(h_cam, IS_GET_FRAMERATE, &mut frame_rate) };
        if ret != 0 {
            println!("ThorcamNode: is_SetFrameRate(GET) failed: {}", ret);
        }

        let gain = unsafe {
            (self.set_hardware_gain)(h_cam, IS_GET_MASTER_GAIN, IS_IGNORE_PARAMETER, IS_IGNORE_PARAMETER, IS_IGNORE_PARAMETER)
        };

        Some(CameraValues {
            width_max:     info.n_max_width as i64,
            height_max:    info.n_max_height as i64,
            width:         rect.width as i64,
            height:        rect.height as i64,
            offset_x:      rect.x as i64,
            offset_y:      rect.y as i64,
            exposure_time: exposure_ms * 1000.0,
            frame_rate,
            gain:          gain as f64,
        })
    }

    /// Sets the region of interest, rounding to what the sensor supports. Only
    /// valid while the camera isn't streaming, the image buffers are sized to it.
    fn write_aoi(&self, h_cam: u32, requested: &RequestedValues) {
        if requested.width.is_none() && requested.height.is_none()
            && requested.offset_x.is_none() && requested.offset_y.is_none() {
            return;
        }
        let Some(current) = self.aoi_query::<IsRect>(h_cam, IS_AOI_IMAGE_GET_AOI) else { return };
        let mut info = unsafe { std::mem::zeroed::<SensorInfoRaw>() };
        if unsafe { (self.get_sensor_info)(h_cam, &mut info) } != 0 {
            return;
        }
        let size_min = self.aoi_query::<IsSize2d>(h_cam, IS_AOI_IMAGE_GET_SIZE_MIN).unwrap_or(IsSize2d { width: 1, height: 1 });
        let size_inc = self.aoi_query::<IsSize2d>(h_cam, IS_AOI_IMAGE_GET_SIZE_INC).unwrap_or(IsSize2d { width: 1, height: 1 });
        let pos_inc = self.aoi_query::<IsPoint2d>(h_cam, IS_AOI_IMAGE_GET_POS_INC).unwrap_or(IsPoint2d { x: 1, y: 1 });

        let fit_size = |wanted: i64, min: i32, inc: i32, max: i64| -> i64 {
            let inc = inc.max(1) as i64;
            let min = (min.max(1) as i64).min(max);
            (wanted.clamp(min, max) / inc * inc).max(min)
        };
        let fit_offset = |wanted: i64, inc: i32, size: i64, max: i64| -> i64 {
            let inc = inc.max(1) as i64;
            (wanted.clamp(0, (max - size).max(0)) / inc) * inc
        };

        let width = fit_size(requested.width.unwrap_or(current.width as i64), size_min.width, size_inc.width, info.n_max_width as i64);
        let height = fit_size(requested.height.unwrap_or(current.height as i64), size_min.height, size_inc.height, info.n_max_height as i64);
        let mut rect = IsRect {
            x: fit_offset(requested.offset_x.unwrap_or(current.x as i64), pos_inc.x, width, info.n_max_width as i64) as i32,
            y: fit_offset(requested.offset_y.unwrap_or(current.y as i64), pos_inc.y, height, info.n_max_height as i64) as i32,
            width: width as i32,
            height: height as i32,
        };
        let ret = unsafe {
            (self.aoi)(h_cam, IS_AOI_IMAGE_SET_AOI, &mut rect as *mut IsRect as *mut std::ffi::c_void, std::mem::size_of::<IsRect>() as u32)
        };
        if ret != 0 {
            println!("ThorcamNode: is_AOI(SET_AOI) failed: {}", ret);
        }
    }

    /// Writes the requested values to the camera. The region of interest is
    /// first since it limits the frame rate, and the frame rate limits exposure.
    fn write_values(&self, h_cam: u32, requested: &RequestedValues, allow_aoi: bool) {
        if allow_aoi {
            self.write_aoi(h_cam, requested);
        }
        if let Some(frame_rate) = requested.frame_rate {
            let mut actual = 0.0f64;
            let ret = unsafe { (self.set_frame_rate)(h_cam, frame_rate, &mut actual) };
            if ret != 0 {
                println!("ThorcamNode: is_SetFrameRate failed: {}", ret);
            }
        }
        if let Some(exposure_time) = requested.exposure_time {
            let mut exposure_ms = exposure_time / 1000.0;
            let ret = unsafe {
                (self.exposure)(h_cam, IS_EXPOSURE_CMD_SET_EXPOSURE, &mut exposure_ms as *mut f64 as *mut std::ffi::c_void, 8)
            };
            if ret != 0 {
                println!("ThorcamNode: is_Exposure(SET_EXPOSURE) failed: {}", ret);
            }
        }
        if let Some(gain) = requested.gain {
            let master = gain.round().clamp(0.0, 100.0) as i32;
            let ret = unsafe {
                (self.set_hardware_gain)(h_cam, master, IS_IGNORE_PARAMETER, IS_IGNORE_PARAMETER, IS_IGNORE_PARAMETER)
            };
            if ret != 0 {
                println!("ThorcamNode: is_SetHardwareGain failed: {}", ret);
            }
        }
    }
}

fn camera_label(camera: &CameraInfo) -> String {
    format!("{}:{}", camera.model, camera.serial_number)
}

/// The camera named by the "Camera" key (as listed by get_cameras), or the
/// first camera when none is chosen yet.
fn selected_device_id(state: &State) -> Option<u32> {
    let (_, cameras) = UC480.get()?.as_ref().ok()?;
    match state.get(StateKey::String("Camera".to_string())) {
        Some(StateValue::String(name)) if !name.is_empty() => {
            let found = cameras.iter().find(|c| camera_label(c) == name);
            if found.is_none() {
                println!("ThorcamNode: camera '{}' not found", name);
            }
            found.map(|c| c.device_id)
        }
        _ => cameras.first().map(|c| c.device_id),
    }
}

enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    fn value(&self) -> StateValue {
        match self {
            Number::Int(i) => StateValue::Int(*i),
            Number::Float(f) => StateValue::Float(*f),
        }
    }
}

/// Writes `values` to "Camera Values", and to the config keys of the same name,
/// either always (`overwrite`, a sync) or only where the config has no value yet.
/// Main thread only.
fn publish_values(state: &State, values: &CameraValues, overwrite: bool) {
    let camera_values = match state.get(StateKey::String("Camera Values".to_string())) {
        Some(StateValue::Dict(dict)) => Some(dict),
        _ => None,
    };
    let numbers = [
        ("WidthMax", Number::Int(values.width_max)),
        ("HeightMax", Number::Int(values.height_max)),
        ("Width", Number::Int(values.width)),
        ("Height", Number::Int(values.height)),
        ("OffsetX", Number::Int(values.offset_x)),
        ("OffsetY", Number::Int(values.offset_y)),
        ("ExposureTime", Number::Float(values.exposure_time)),
        ("AcquisitionFrameRate", Number::Float(values.frame_rate)),
        ("Gain", Number::Float(values.gain)),
    ];
    for (key, number) in numbers.iter() {
        if let Some(dict) = &camera_values {
            dict.set(StateKey::String(key.to_string()), number.value());
        }
        if overwrite || !state.contains_key(StateKey::String(key.to_string())) {
            state.set(StateKey::String(key.to_string()), number.value());
        }
    }
}

/// Latest camera frame handed off from the capture thread to the main
/// thread's preview window, overwritten in place each frame -- the viewer
/// only ever needs the most recent one and drops older frames on the floor.
#[derive(Clone)]
struct FrameSnapshot {
    data:   Vec<u8>,
    width:  u32,
    height: u32,
}

struct ThorcamNodeInner {
    api:               ThalamusAPI,
    node_token:        NodeToken,
    state:             State,
    state_connection:  Option<OnDrop>,
    camera_thread:     Option<std::thread::JoinHandle<()>>,
    main_thread_token: MainThreadToken,
    viewer:            Option<ImageViewer>,
    viewer_task:       Option<TaskScope>,
    shared_frame:      Arc<Mutex<Option<FrameSnapshot>>>,
    signaler:          Arc<OffMainSignaler>,
    /// The camera thread's handle while it is streaming, so a sync can adjust the
    /// running camera. Held locked by a sync so the thread can't close it mid-call.
    live_handle:       Arc<Mutex<Option<u32>>>,
}

pub struct ThorcamNode {
    inner: Rc<RefCell<ThorcamNodeInner>>,
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
    Some((values[0] as i32, values[1] as i32, values[2] as i32, values[3] as i32))
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
    state.set(StateKey::String("view_geometry".to_string()), StateValue::List(list));
}

/// Ticks the preview window on the main thread: uploads+presents whatever
/// frame the capture thread most recently deposited in `shared_frame`, and
/// once a second checks whether the window moved/resized to persist that
/// into `view_geometry`. Ends itself once the viewer is closed (by the
/// window's X button -- which also flips `View` back to false -- or by
/// close_viewer). Holds `inner` weakly so it never keeps ThorcamNodeInner
/// alive by itself; see close_viewer for why.
async fn viewer_tick_loop(api: ThalamusAPI, inner: Weak<RefCell<ThorcamNodeInner>>, initial_geometry: (i32, i32, i32, i32)) {
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
        let Some(viewer) = borrow.viewer.as_mut() else { break };

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
            inner.borrow().state.set(StateKey::String("View".to_string()), StateValue::Bool(false));
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
fn open_viewer(inner: &Rc<RefCell<ThorcamNodeInner>>) {
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
            write_geometry(api, &state, default_geometry.0, default_geometry.1, default_geometry.2, default_geometry.3);
            default_geometry
        }
    };
    let (x, y, w, h) = geometry;

    match ImageViewer::new(api, "Thorcam", x, y, w, h) {
        Ok(viewer) => {
            let mut borrow = inner.borrow_mut();
            borrow.viewer = Some(viewer);
            borrow.viewer_task = Some(run_task(viewer_tick_loop(api, Rc::downgrade(inner), geometry)));
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
fn close_viewer(inner: &Rc<RefCell<ThorcamNodeInner>>) {
    inner.borrow_mut().viewer = None;
}

fn start_camera(inner: &Rc<RefCell<ThorcamNodeInner>>) {
    let (api, device_id, requested, wrapped_state, shared_frame, signaler, live_handle) = {
        let borrow = inner.borrow();
        (
            borrow.api,
            selected_device_id(&borrow.state),
            RequestedValues::read(&borrow.state),
            MainThreadOnly::new(borrow.state.clone(), borrow.main_thread_token),
            borrow.shared_frame.clone(),
            borrow.signaler.clone(),
            borrow.live_handle.clone(),
        )
    };

    let Some(device_id) = device_id else {
        println!("ThorcamNode: no cameras available");
        return;
    };

    let mt_api = api.thread_safe();
    signaler.unblock();
    let handle = std::thread::spawn(move || {
        run_camera(mt_api, signaler, device_id, requested, wrapped_state, shared_frame, live_handle);
    });

    let mut borrow = inner.borrow_mut();
    borrow.camera_thread = Some(handle);
}

/// Writes the config to the camera and reads back what it accepted into
/// "Camera Values" and the config, like GenicamNode::sync_config. While
/// streaming only exposure, gain and frame rate are written, the region of
/// interest is fixed by the image buffers until the camera is restarted.
fn sync_config(inner: &Rc<RefCell<ThorcamNodeInner>>) {
    let (state, live_handle) = {
        let borrow = inner.borrow();
        (borrow.state.clone(), borrow.live_handle.clone())
    };
    let Some((lib, _)) = UC480.get().and_then(|r| r.as_ref().ok()) else {
        println!("ThorcamNode: uc480 not initialized");
        return;
    };
    let requested = RequestedValues::read(&state);

    let live = live_handle.lock().unwrap();
    let values = match *live {
        Some(h_cam) => {
            lib.write_values(h_cam, &requested, false);
            lib.read_values(h_cam)
        }
        None => {
            let Some(device_id) = selected_device_id(&state) else {
                println!("ThorcamNode: no cameras available");
                return;
            };
            lib.with_camera(device_id, |h_cam| {
                lib.sanitize(h_cam);
                lib.write_values(h_cam, &requested, true);
                lib.read_values(h_cam)
            }).flatten()
        }
    };
    drop(live);

    if let Some(values) = values {
        publish_values(&state, &values, true);
    }
}

/// Reads the camera's values into "Camera Values", and into the config keys
/// when `overwrite` is set (a different camera was chosen), otherwise only where
/// the config has no value yet. Skipped while the camera thread owns the camera.
fn refresh_camera_values(inner: &Rc<RefCell<ThorcamNodeInner>>, overwrite: bool) {
    let (state, running) = {
        let borrow = inner.borrow();
        (borrow.state.clone(), borrow.camera_thread.is_some())
    };
    if running {
        return;
    }
    let Some((lib, _)) = UC480.get().and_then(|r| r.as_ref().ok()) else { return };
    let Some(device_id) = selected_device_id(&state) else { return };
    // Read only: choosing a camera must not change what is programmed on it. The
    // camera is only written to by a sync or when it starts running.
    let values = lib.with_camera(device_id, |h_cam| lib.read_values(h_cam)).flatten();
    if let Some(values) = values {
        publish_values(&state, &values, overwrite);
    }
}

fn stop_camera(inner: &Rc<RefCell<ThorcamNodeInner>>) {
    let mut borrow = inner.borrow_mut();
    borrow.signaler.block();
    borrow.camera_thread.take().map(|h| h.join());
}

fn run_camera(
    api: ThalamusAPIThreadSafe,
    signaler: Arc<OffMainSignaler>,
    device_id: u32,
    requested: RequestedValues,
    state: MainThreadOnly<State>,
    shared_frame: Arc<Mutex<Option<FrameSnapshot>>>,
    live_handle: Arc<Mutex<Option<u32>>>,
) {
    let lib = match UC480.get().and_then(|r| r.as_ref().ok()) {
        Some((lib, _)) => lib,
        None => { println!("ThorcamNode: uc480 not initialized"); return; }
    };

    let mut h_cam = device_id | IS_USE_DEVICE_ID;
    let ret = unsafe { (lib.init_camera)(&mut h_cam, std::ptr::null_mut()) };
    if ret != 0 {
        println!("ThorcamNode: is_InitCamera failed: {}", ret);
        return;
    }

    unsafe { (lib.set_color_mode)(h_cam, 6) }; // IS_CM_MONO8

    // Same as GenicamNode::start_stream: sync the config to the camera, then read
    // back what the camera actually accepted.
    lib.sanitize(h_cam);
    lib.write_values(h_cam, &requested, true);
    let Some(values) = lib.read_values(h_cam) else {
        unsafe { (lib.exit_camera)(h_cam) };
        return;
    };
    println!("ThorcamNode: {:?}", values);
    api.post_to_main(move |main_thread_token| {
        let state = state.take(main_thread_token);
        publish_values(&state, &values, true);
    });

    let width = values.width as u64;
    let height = values.height as u64;
    let frame_interval = if values.frame_rate > 0.0 {
        Duration::from_secs_f64(1.0 / values.frame_rate)
    } else {
        Duration::from_nanos(16_666_667)
    };

    // Allocate a ring buffer of 3 frames for is_WaitForNextImage
    const NUM_BUFS: usize = 3;
    let mut bufs: Vec<(*mut i8, i32)> = Vec::with_capacity(NUM_BUFS);
    let mut alloc_ok = true;
    for _ in 0..NUM_BUFS {
        let mut p_mem: *mut i8 = std::ptr::null_mut();
        let mut mem_id: i32 = 0;
        let ret = unsafe { (lib.alloc_image_mem)(h_cam, width as i32, height as i32, 8, &mut p_mem, &mut mem_id) };
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

    *live_handle.lock().unwrap() = Some(h_cam);

    loop {
        let mut next_mem: *mut i8 = std::ptr::null_mut();
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
            time: crate::api::time(api.raw),
            frame_interval,
        };

        // Publishes directly from this thread; subscribers read plane() synchronously
        // before this call returns. Ignore if the node was destroyed concurrently.
        match signaler.ready(&data) {
            Ok(v) => {
                if !v { break }
            },
            Err(_) => { break }
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

    // Taken under the lock so an in-flight sync finishes before the camera closes.
    *live_handle.lock().unwrap() = None;

    unsafe { (lib.stop_live_video)(h_cam, 1) }; // IS_WAIT
    unsafe { (lib.exit_image_queue)(h_cam) };
    unsafe { (lib.clear_sequence)(h_cam) };
    for (p, id) in bufs {
        unsafe { (lib.free_image_mem)(h_cam, p, id) };
    }
    unsafe { (lib.exit_camera)(h_cam) };
    println!("ThorcamNode: camera thread exited");
}

struct ThorcamFrame {
    frame_ptr: *const u8,
    frame_len: usize,
    width:     u64,
    height:    u64,
    time:      Duration,
    frame_interval: Duration,
}

impl NodeData for ThorcamFrame {
    fn time(&self) -> Duration {
        self.time
    }
    fn analog(&self) -> Option<&dyn AnalogData> { None }
    fn image(&self) -> Option<&dyn ImageData> {
        Some(self)
    }
    fn mocap(&self) -> Option<&dyn MocapData> { None }
}

impl ImageData for ThorcamFrame {
    fn plane(&self, _channel: i32) -> &[u8] {
        if self.frame_ptr.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.frame_ptr, self.frame_len) }
        }
    }
    fn num_planes(&self) -> u64 { 1 }
    fn format(&self) -> ImageFormat { ImageFormat::Gray }
    fn width(&self) -> u64 { self.width }
    fn height(&self) -> u64 { self.height }
    fn frame_interval(&self) -> Duration { self.frame_interval }
}

impl Node for ThorcamNode {
    fn modalities(&self) -> u32 {
      THALAMUS_MODALITY_IMAGE
    }

    fn signals_offmain(&self) -> bool {
        true
    }

    fn process(&self, handle: Request, request: Json) {
        let api = self.inner.borrow().api;
        let response = match serde_json::from_str::<serde_json::Value>(&request.to_string()) {
            Ok(serde_json::Value::String(s)) if s == "get_cameras" => {
                let cameras: Vec<serde_json::Value> = UC480.get()
                    .and_then(|r| r.as_ref().ok())
                    .map(|(_, cams)| {
                        cams.iter()
                            .map(|c| serde_json::Value::String(camera_label(c)))
                            .collect()
                    })
                    .unwrap_or_default();
                serde_json::to_string(&cameras).unwrap()
            }
            Ok(serde_json::Value::String(s)) if s == "sync_config" => {
                sync_config(&self.inner);
                "null".to_string()
            }
            _ => "null".to_string(),
        };
        handle.respond(&Json::from_string(api, &response));
    }

    fn new(api: ThalamusAPI, node_token: NodeToken, state: State, token: MainThreadToken) -> Self {
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

        if state.get(StateKey::String("Camera Values".to_string())).is_none() {
            state.set(StateKey::String("Camera Values".to_string()), StateValue::Dict(State::make_dict(api)));
        }

        let signaler = Arc::new(OffMainSignaler::new(api, node_token.clone()));
        let inner = Rc::new(RefCell::new(ThorcamNodeInner {
            api,
            node_token,
            state: state.clone(),
            state_connection: None,
            camera_thread: None,
            main_thread_token: token,
            viewer: None,
            viewer_task: None,
            shared_frame: Arc::new(Mutex::new(None)),
            signaler,
            live_handle: Arc::new(Mutex::new(None)),
        }));

        let change_ref = Rc::clone(&inner);
        // The camera seen by the last "Camera" callback. The first one, from the
        // recap of the saved config, must not replace the saved settings.
        let mut last_camera: Option<String> = None;
        let started = Rc::new(std::cell::Cell::new(false));
        let started_in_callback = Rc::clone(&started);
        let state_callback = move |_source: State, _action: StateAction, key: StateValue, value: StateValue| {
            let StateValue::String(key_str) = key else { return };
            match key_str.as_str() {
                "Running" => {
                    stop_camera(&change_ref);
                    if value == StateValue::Bool(true) {
                        start_camera(&change_ref);
                    }
                }
                "Camera" => {
                    let StateValue::String(name) = value else { return };
                    let changed = started_in_callback.get() && last_camera.as_deref() != Some(name.as_str());
                    last_camera = Some(name);
                    if changed {
                        // The running camera is the old one, and its thread is holding it.
                        let running = change_ref.borrow().camera_thread.is_some();
                        if running {
                            stop_camera(&change_ref);
                            let state = change_ref.borrow().state.clone();
                            state.set(StateKey::String("Running".to_string()), StateValue::Bool(false));
                        }
                    }
                    refresh_camera_values(&change_ref, changed);
                }
                "View" => {
                    if value == StateValue::Bool(true) {
                        open_viewer(&change_ref);
                    } else {
                        close_viewer(&change_ref);
                    }
                }
                _ => {}
            }
        };

        inner.borrow_mut().state_connection = Some(state.connect(state_callback));
        state.recap();
        started.set(true);
        // No Camera chosen yet, so the recap above didn't load the values of the default camera.
        if !state.contains_key(StateKey::String("Camera".to_string())) {
            refresh_camera_values(&inner, false);
        }

        ThorcamNode { inner }
    }
}

impl Drop for ThorcamNode {
    fn drop(&mut self) {
        close_viewer(&self.inner);
        stop_camera(&self.inner);
    }
}
