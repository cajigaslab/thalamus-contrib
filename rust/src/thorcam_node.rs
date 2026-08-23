use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::api::{
    AnalogData, ImageData, ImageFormat, MainThreadToken, MocapData, Node, NodeData, NodeToken,
    OnDrop, PredropToken, Request, Json, State, StateAction, StateKey, StateValue, ThalamusAPI,
    ThalamusAPIThreadSafe, THALAMUS_MODALITY_IMAGE, run_task, TaskScope,
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
    }, cameras))
}

/// Latest camera frame handed off from the capture thread to the main
/// thread's preview window, overwritten in place each frame -- the viewer
/// only ever needs the most recent one and drops older frames on the floor.
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
    stop_flag:         Arc<AtomicBool>,
    main_thread_token: MainThreadToken,
    viewer:            Option<ImageViewer>,
    viewer_task:       Option<TaskScope>,
    shared_frame:      Arc<Mutex<Option<FrameSnapshot>>>,
}

pub struct ThorcamNode {
    inner: Rc<RefCell<ThorcamNodeInner>>,
}


fn start_camera(inner: &Rc<RefCell<ThorcamNodeInner>>) {
    let inner_clone = Rc::clone(inner);
    // Don't spawn the new capture thread until the old one (if any) has fully
    // exited: stop_camera's join happens off the main thread, so without this
    // hand-off a stale thread could still be mid-shutdown (e.g. still holding
    // the device open) when a new one tries to start.
    stop_camera(inner, move || {
        start_camera_impl(&inner_clone);
    });
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
        let snapshot = shared_frame.lock().unwrap().take();

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

fn start_camera_impl(inner: &Rc<RefCell<ThorcamNodeInner>>) {
    let (api, device_id, node_token, shared_frame) = {
        let borrow = inner.borrow();
        let device_id = UC480.get()
            .and_then(|r| r.as_ref().ok())
            .and_then(|(_, cameras)| cameras.first())
            .map(|c| c.device_id);
        (borrow.api, device_id, borrow.node_token.clone(), borrow.shared_frame.clone())
    };

    let Some(device_id) = device_id else {
        println!("ThorcamNode: no cameras available");
        return;
    };

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_flag);

    let mt_api = api.thread_safe();
    let handle = std::thread::spawn(move || {
        run_camera(mt_api, node_token, stop_clone, device_id, shared_frame);
    });

    let mut borrow = inner.borrow_mut();
    borrow.stop_flag = stop_flag;
    borrow.camera_thread = Some(handle);
}

fn stop_camera<F: FnOnce() + 'static>(inner: &Rc<RefCell<ThorcamNodeInner>>, on_stopped: F) {
    let (handle, api, main_thread_token) = {
        let mut borrow = inner.borrow_mut();
        borrow.stop_flag.store(true, Ordering::Relaxed);
        (borrow.camera_thread.take(), borrow.api, borrow.main_thread_token)
    };
    // Never join on the main thread: the capture thread's ready_offmain call
    // synchronously posts to and blocks on the main thread, so a blocking join
    // here could deadlock against it. join_then hands the join off to a
    // threadpool thread and runs on_stopped once back on the main thread.
    api.join_then(handle, main_thread_token, on_stopped);
}

fn run_camera(
    api: ThalamusAPIThreadSafe,
    node_token: NodeToken,
    stop: Arc<AtomicBool>,
    device_id: u32,
    shared_frame: Arc<Mutex<Option<FrameSnapshot>>>,
) {
    let lib = match UC480.get().and_then(|r| r.as_ref().ok()) {
        Some((lib, _)) => lib,
        None => { println!("ThorcamNode: uc480 not initialized"); return; }
    };

    let mut h_cam = device_id | 0x8000; // IS_USE_DEVICE_ID
    let ret = unsafe { (lib.init_camera)(&mut h_cam, std::ptr::null_mut()) };
    if ret != 0 {
        println!("ThorcamNode: is_InitCamera failed: {}", ret);
        return;
    }

    let mut info = unsafe { std::mem::zeroed::<SensorInfoRaw>() };
    let ret = unsafe { (lib.get_sensor_info)(h_cam, &mut info) };
    if ret != 0 {
        println!("ThorcamNode: is_GetSensorInfo failed: {}", ret);
        unsafe { (lib.exit_camera)(h_cam) };
        return;
    }
    let width = info.n_max_width as u64;
    let height = info.n_max_height as u64;
    println!("ThorcamNode: camera resolution {}x{}", width, height);

    unsafe { (lib.set_color_mode)(h_cam, 6) }; // IS_CM_MONO8

    let mut actual_fps: f64 = 0.0;
    let ret = unsafe { (lib.set_frame_rate)(h_cam, 60.0, &mut actual_fps) };
    if ret != 0 {
        println!("ThorcamNode: is_SetFrameRate failed: {}", ret);
    } else {
        println!("ThorcamNode: frame rate set to {:.2} FPS", actual_fps);
    }

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

    while !stop.load(Ordering::Relaxed) {
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
        };

        // Publishes directly from this thread; subscribers read plane() synchronously
        // before this call returns. Ignore if the node was destroyed concurrently.
        let _ = api.ready_offmain(&data, &node_token);

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
    fn frame_interval(&self) -> Duration { Duration::from_nanos(16_666_667) } // ~60 FPS
}

impl Node for ThorcamNode {
    fn modalities(&self) -> u32 {
      THALAMUS_MODALITY_IMAGE
    }

    fn process(&self, handle: Request, request: Json) {
        let api = self.inner.borrow().api;
        let response = match serde_json::from_str::<serde_json::Value>(&request.to_string()) {
            Ok(serde_json::Value::String(s)) if s == "get_cameras" => {
                let cameras: Vec<serde_json::Value> = UC480.get()
                    .and_then(|r| r.as_ref().ok())
                    .map(|(_, cams)| {
                        cams.iter()
                            .map(|c| serde_json::Value::String(
                                format!("{}:{}", c.model, c.serial_number)
                            ))
                            .collect()
                    })
                    .unwrap_or_default();
                serde_json::to_string(&cameras).unwrap()
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

        let inner = Rc::new(RefCell::new(ThorcamNodeInner {
            api,
            node_token,
            state: state.clone(),
            state_connection: None,
            camera_thread: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            main_thread_token: token,
            viewer: None,
            viewer_task: None,
            shared_frame: Arc::new(Mutex::new(None)),
        }));

        let change_ref = Rc::clone(&inner);
        let state_callback = move |_source: State, _action: StateAction, key: StateValue, value: StateValue| {
            let StateValue::String(key_str) = key else { return };
            match key_str.as_str() {
                "Running" => {
                    if value == StateValue::Bool(true) {
                        start_camera(&change_ref);
                    } else {
                        stop_camera(&change_ref, || {});
                    }
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

        ThorcamNode { inner }
    }

    fn predrop(&self, token: PredropToken) {
        close_viewer(&self.inner);
        stop_camera(&self.inner, move || {
            token.ready();
        });
    }
}

impl Drop for ThorcamNode {
    fn drop(&mut self) {
        close_viewer(&self.inner);
        stop_camera(&self.inner, || {});
    }
}
