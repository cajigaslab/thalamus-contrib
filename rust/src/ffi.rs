use std::{ffi::CString};
use std::ptr;
use crate::api::{ThalamusAPI, PredropToken, NodeData};
use crate::api::ImageFormat;

pub const THALAMUS_MODALITY_ANALOG: u32 = 1;
pub const THALAMUS_MODALITY_MOCAP: u32 = 2;
pub const THALAMUS_MODALITY_IMAGE: u32 = 4;
pub const THALAMUS_MODALITY_TEXT: u32 = 8;
pub const THALAMUS_MODALITY_STIM: u32 = 16;

#[repr(C)]
pub struct ThalamusNode {
    pub c_impl: *mut ::std::os::raw::c_void,
    pub time_ns: ::std::option::Option<unsafe extern "C" fn(arg1: *mut ThalamusNode) -> u64>,
    pub analog: *mut ThalamusAnalogNode,
    pub mocap: *mut ThalamusMocapNode,
    pub image: *mut ThalamusImageNode,
    pub text: *mut ThalamusTextNode,
    pub plugin_impl: *mut ::std::os::raw::c_void,
    pub process: ::std::option::Option<unsafe extern "C" fn(*mut ThalamusNode, *mut ThalamusRequestHandle, *mut ThalamusJson)>,
    pub predrop: ::std::option::Option<unsafe extern "C" fn(*mut ThalamusNode)>,
    pub signals_offmain: u8,
}

#[repr(C)]
pub struct ThalamusAnalogNode {
    pub data: ::std::option::Option<
        unsafe extern "C" fn(
            *mut ThalamusDoubleSpan,
            node: *mut ThalamusNode,
            channel: ::std::os::raw::c_int,
        ),
    >,
    pub short_data: ::std::option::Option<
        unsafe extern "C" fn(
            *mut ThalamusShortSpan,
            node: *mut ThalamusNode,
            channel: ::std::os::raw::c_int,
        ),
    >,
    pub int_data: ::std::option::Option<
        unsafe extern "C" fn(
            *mut ThalamusIntSpan,
            node: *mut ThalamusNode,
            channel: ::std::os::raw::c_int,
        ),
    >,
    pub ulong_data: ::std::option::Option<
        unsafe extern "C" fn(
            *mut ThalamusULongSpan,
            node: *mut ThalamusNode,
            channel: ::std::os::raw::c_int,
        ),
    >,
    pub num_channels: ::std::option::Option<
        unsafe extern "C" fn(node: *mut ThalamusNode) -> ::std::os::raw::c_int,
    >,
    pub sample_interval_ns: ::std::option::Option<
        unsafe extern "C" fn(node: *mut ThalamusNode, channel: ::std::os::raw::c_int) -> u64,
    >,
    pub has_analog_data: ::std::option::Option<
        unsafe extern "C" fn(node: *mut ThalamusNode) -> ::std::os::raw::c_char,
    >,
    pub is_short_data: ::std::option::Option<
        unsafe extern "C" fn(node: *mut ThalamusNode) -> ::std::os::raw::c_char,
    >,
    pub is_int_data: ::std::option::Option<
        unsafe extern "C" fn(node: *mut ThalamusNode) -> ::std::os::raw::c_char,
    >,
    pub is_ulong_data: ::std::option::Option<
        unsafe extern "C" fn(node: *mut ThalamusNode) -> ::std::os::raw::c_char,
    >,
    pub is_transformed: ::std::option::Option<
        unsafe extern "C" fn(node: *mut ThalamusNode) -> ::std::os::raw::c_char,
    >,
    pub scale: ::std::option::Option<
        unsafe extern "C" fn(node: *mut ThalamusNode, channel: ::std::os::raw::c_int) -> f64,
    >,
    pub offset: ::std::option::Option<
        unsafe extern "C" fn(node: *mut ThalamusNode, channel: ::std::os::raw::c_int) -> f64,
    >,
    pub name: ::std::option::Option<
        unsafe extern "C" fn(
            *mut ThalamusByteSpan,
            node: *mut ThalamusNode,
            channel: ::std::os::raw::c_int,
        ),
    >,
}

#[repr(C)]
#[derive(Debug, PartialEq)]
pub enum ThalamusImageFormat {
    Gray = 0,
    RGB = 1,
    YUYV422 = 2,
    YUV420P = 3,
    YUVJ420P = 4,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusImageNode {
    pub plane: ::std::option::Option<
        unsafe extern "C" fn(
            *mut ThalamusByteSpan,
            node: *mut ThalamusNode,
            channel: ::std::os::raw::c_int,
        ),
    >,
    pub num_planes: ::std::option::Option<
        unsafe extern "C" fn(
            node: *mut ThalamusNode,
        ) -> u64,
    >,
    pub format: ::std::option::Option<
        unsafe extern "C" fn(
            node: *mut ThalamusNode,
        ) -> ThalamusImageFormat,
    >,
    pub width: ::std::option::Option<
        unsafe extern "C" fn(
            node: *mut ThalamusNode,
        ) -> u64,
    >,
    pub height: ::std::option::Option<
        unsafe extern "C" fn(
            node: *mut ThalamusNode,
        ) -> u64,
    >,
    pub frame_interval_ns: ::std::option::Option<
        unsafe extern "C" fn(
            node: *mut ThalamusNode,
        ) -> u64,
    >,
    pub has_image_data: ::std::option::Option<
        unsafe extern "C" fn(node: *mut ThalamusNode) -> ::std::os::raw::c_char,
    >,
}

impl ThalamusAnalogNode {
  pub fn new() -> ThalamusAnalogNode {
    ThalamusAnalogNode {
      data: None,
      short_data: None,
      int_data: None,
      ulong_data: None,
      num_channels: None,
      sample_interval_ns: None,
      has_analog_data: None,
      is_short_data: None,
      is_int_data: None,
      is_ulong_data: None,
      is_transformed: None,
      scale: None,
      offset: None,
      name: None
    }
  }
}

impl ThalamusImageNode {
  pub fn new() -> ThalamusImageNode {
    ThalamusImageNode {
      plane: None,
      num_planes: None,
      format: None,
      width: None,
      height: None,
      frame_interval_ns: None,
      has_image_data: None,
    }
  }
}

pub const THALAMUS_STATE_ACTION_SET: ThalamusStateAction = 0;
pub const THALAMUS_STATE_ACTION_DELETE: ThalamusStateAction = 1;
pub type ThalamusStateAction = ::std::os::raw::c_int;

pub type ThalamusStateRecursiveCallback = ::std::option::Option<
    unsafe extern "C" fn(
        source: *mut ThalamusState,
        action: ThalamusStateAction,
        key: *mut ThalamusState,
        value: *mut ThalamusState,
        data: *mut ::std::os::raw::c_void,
    ),
>;

pub type IoContextPostCallback = ::std::option::Option<
    unsafe extern "C" fn(data: *mut ::std::os::raw::c_void),
>;

pub type ThalamusIOCallback = ::std::option::Option<
    unsafe extern "C" fn(err: *mut ThalamusErrorCode, count: u64, data: *mut ::std::os::raw::c_void),
>;

pub type ThalamusNodeGetCallback = ::std::option::Option<
    unsafe extern "C" fn(err: *mut ThalamusNode, data: *mut ::std::os::raw::c_void),
>;
pub type ThalamusNodeReadyCallback = ::std::option::Option<
    unsafe extern "C" fn(err: *mut ThalamusNode, data: *mut ::std::os::raw::c_void),
>;

/// Generates `ThalamusAPIRaw` with every function field wrapped in `Option`,
/// plus a `sanitize` method that nulls out any field whose position (0-based,
/// in the order listed below) is >= `version`.
///
/// `version` is filled in by Thalamus as a count of how many of these
/// functions it actually initialized. Since this struct is only ever
/// appended to, a thalamus-contrib build newer than the running Thalamus may
/// know about fields Thalamus never wrote to; sanitize() ensures those read
/// back as `None` instead of whatever raw memory happened to be there.
macro_rules! thalamus_api_raw {
    ( $( $field:ident : $fnty:ty ),* $(,)? ) => {
        #[repr(C)]
        #[derive(Debug, Copy, Clone)]
        pub struct ThalamusAPIRaw {
            pub version: i32,
            $(pub $field: ::std::option::Option<$fnty>,)*
        }

        impl ThalamusAPIRaw {
            pub fn sanitize(&mut self) {
                let mut i: i32 = 0;
                $(
                    if i >= self.version {
                        self.$field = None;
                    }
                    i += 1;
                )*
                let _ = i;
            }
        }
    };
}

thalamus_api_raw! {
    state_is_dict: unsafe extern "C" fn(arg1: *mut ThalamusState) -> ::std::os::raw::c_char,
    state_is_list: unsafe extern "C" fn(arg1: *mut ThalamusState) -> ::std::os::raw::c_char,
    state_is_string: unsafe extern "C" fn(arg1: *mut ThalamusState) -> ::std::os::raw::c_char,
    state_is_int: unsafe extern "C" fn(arg1: *mut ThalamusState) -> ::std::os::raw::c_char,
    state_is_float: unsafe extern "C" fn(arg1: *mut ThalamusState) -> ::std::os::raw::c_char,
    state_is_null: unsafe extern "C" fn(arg1: *mut ThalamusState) -> ::std::os::raw::c_char,
    state_is_bool: unsafe extern "C" fn(arg1: *mut ThalamusState) -> ::std::os::raw::c_char,
    state_get_string: unsafe extern "C" fn(out: *mut ThalamusCharSpan, arg1: *mut ThalamusState),
    state_get_int: unsafe extern "C" fn(arg1: *mut ThalamusState) -> i64,
    state_get_float: unsafe extern "C" fn(arg1: *mut ThalamusState) -> f64,
    state_get_bool: unsafe extern "C" fn(arg1: *mut ThalamusState) -> ::std::os::raw::c_char,
    state_get_at_name: unsafe extern "C" fn(
        arg1: *mut ThalamusState,
        arg2: *const ThalamusCharSpan,
    ) -> *mut ThalamusState,
    state_get_at_index: unsafe extern "C" fn(arg1: *mut ThalamusState, arg2: u64) -> *mut ThalamusState,
    state_dec_ref: unsafe extern "C" fn(arg1: *mut ThalamusState),
    state_inc_ref: unsafe extern "C" fn(arg1: *mut ThalamusState),
    state_recursive_change_connect: unsafe extern "C" fn(
        state: *mut ThalamusState,
        callback: ThalamusStateRecursiveCallback,
        data: *mut ::std::os::raw::c_void,
    ) -> *mut ThalamusStateConnection,
    state_recursive_change_disconnect: unsafe extern "C" fn(state: *mut ThalamusStateConnection),
    timer_create: unsafe extern "C" fn() -> *mut ThalamusTimer,
    timer_destroy: unsafe extern "C" fn(arg1: *mut ThalamusTimer),
    timer_expire_after_ns: unsafe extern "C" fn(arg1: *mut ThalamusTimer, arg2: u64),
    timer_async_wait:  unsafe extern "C" fn(
        arg1: *mut ThalamusTimer,
        arg2: ThalamusTimerCallback,
        arg3: *mut ::std::os::raw::c_void,
    ),
    error_code_value: unsafe extern "C" fn(arg1: *mut ThalamusErrorCode) -> ::std::os::raw::c_int,
    node_ready: unsafe extern "C" fn(arg1: *const ThalamusNode),
    time_ns: unsafe extern "C" fn() -> u64,
    error_code_operation_aborted: unsafe extern "C" fn() -> i32,
    state_recap: unsafe extern "C" fn(arg1: *mut ThalamusState),

    state_set_at_name_state: unsafe extern "C" fn(arg1: *mut ThalamusState, arg2: *const ThalamusCharSpan, arg3: *mut ThalamusState),
    state_set_at_name_string: unsafe extern "C" fn(arg1: *mut ThalamusState, arg2: *const ThalamusCharSpan, arg3: *const ThalamusCharSpan),
    state_set_at_name_int: unsafe extern "C" fn(arg1: *mut ThalamusState, arg2: *const ThalamusCharSpan, arg3: i64),
    state_set_at_name_float: unsafe extern "C" fn(arg1: *mut ThalamusState, arg2: *const ThalamusCharSpan, arg3: f64),
    state_set_at_name_null: unsafe extern "C" fn(arg1: *mut ThalamusState, arg2: *const ThalamusCharSpan),
    state_set_at_name_bool: unsafe extern "C" fn(arg1: *mut ThalamusState, arg2: *const ThalamusCharSpan, i8),

    state_set_at_index_state: unsafe extern "C" fn(arg1: *mut ThalamusState, i64, arg3: *mut ThalamusState),
    state_set_at_index_string: unsafe extern "C" fn(arg1: *mut ThalamusState, i64, arg3: *const ThalamusCharSpan),
    state_set_at_index_int: unsafe extern "C" fn(arg1: *mut ThalamusState, i64, arg3: i64),
    state_set_at_index_float: unsafe extern "C" fn(arg1: *mut ThalamusState, i64, arg3: f64),
    state_set_at_index_null: unsafe extern "C" fn(arg1: *mut ThalamusState, i64),
    state_set_at_index_bool: unsafe extern "C" fn(arg1: *mut ThalamusState, i64, i8),

    io_context_post: unsafe extern "C" fn(arg1: IoContextPostCallback, data: *mut ::std::os::raw::c_void),

    trace_event_begin: unsafe extern "C" fn(arg1: *const ThalamusCharSpan),

    trace_event_end: unsafe extern "C" fn(),

    serial_port_create: unsafe extern "C" fn() -> *mut ThalamusSerialPort,

    serial_port_destroy: unsafe extern "C" fn(*mut ThalamusSerialPort),

    serial_set_baud_rate: unsafe extern "C" fn(*mut ThalamusSerialPort, u32),

    serial_port_open: unsafe extern "C" fn(*mut ThalamusSerialPort, *const ThalamusCharSpan),

    serial_port_error: unsafe extern "C" fn(*mut ThalamusSerialPort) -> *mut ThalamusErrorCode,

    serial_port_read_until: unsafe extern "C" fn(*mut ThalamusSerialPort, *mut ThalamusStreamBuf, *const ThalamusCharSpan, ThalamusIOCallback, *mut ::std::os::raw::c_void),

    serial_port_read_some: unsafe extern "C" fn(*mut ThalamusSerialPort, *mut ThalamusMutableByteSpan, ThalamusIOCallback, *mut ::std::os::raw::c_void),

    serial_port_read: unsafe extern "C" fn(*mut ThalamusSerialPort, *mut ThalamusMutableByteSpan, ThalamusIOCallback, *mut ::std::os::raw::c_void),

    serial_port_write: unsafe extern "C" fn(*mut ThalamusSerialPort, *mut ThalamusByteSpan, ThalamusIOCallback, *mut ::std::os::raw::c_void),

    streambuf_create: unsafe extern "C" fn() -> *mut ThalamusStreamBuf,
    streambuf_destroy: unsafe extern "C" fn(*mut ThalamusStreamBuf),
    streambuf_to_span: unsafe extern "C" fn(*mut ThalamusCharSpan, *mut ThalamusStreamBuf),
    streambuf_consume: unsafe extern "C" fn(*mut ThalamusStreamBuf, u64),
    streambuf_size: unsafe extern "C" fn(*mut ThalamusStreamBuf) -> u64,
    charspan_release: unsafe extern "C" fn(*mut ThalamusCharSpan),

    error_code_message: unsafe extern "C" fn(*mut ThalamusCharSpan, *mut ThalamusErrorCode),

    json_to_string: unsafe extern "C" fn(*mut ThalamusCharSpan, *mut ThalamusJson),
    json_from_string: unsafe extern "C" fn(*mut ThalamusCharSpan) -> *mut ThalamusJson,

    request_respond: unsafe extern "C" fn(*mut ThalamusRequestHandle, *mut ThalamusJson),

    json_inc_ref: unsafe extern "C" fn(*mut ThalamusJson),
    json_dec_ref: unsafe extern "C" fn(*mut ThalamusJson),

    node_get_node: unsafe extern "C" fn(*mut ThalamusNodeSelector, ThalamusNodeGetCallback, *mut ::std::os::raw::c_void) -> *mut ThalamusNodeGetConnection,

    node_ready_connect: unsafe extern "C" fn(*mut ThalamusNode, ThalamusNodeReadyCallback, *mut ::std::os::raw::c_void) -> *mut ThalamusNodeReadyConnection,

    node_get_node_disconnect: unsafe extern "C" fn(*mut ThalamusNodeGetConnection),
    node_ready_disconnect: unsafe extern "C" fn(*mut ThalamusNodeReadyConnection),

    node_channels_changed: unsafe extern "C" fn(*mut ThalamusNode),

    node_channels_changed_connect: unsafe extern "C" fn(*mut ThalamusNode, ThalamusNodeReadyCallback, *mut ::std::os::raw::c_void) -> *mut ThalamusNodeReadyConnection,
    node_channels_changed_disconnect: unsafe extern "C" fn(*mut ThalamusNodeReadyConnection),

    node_inc_ref: unsafe extern "C" fn(*mut ThalamusNode),
    node_dec_ref: unsafe extern "C" fn(*mut ThalamusNode),

    state_parent: unsafe extern "C" fn(arg1: *mut ThalamusState) -> *mut ThalamusState,

    state_iter_create: unsafe extern "C" fn(*mut ThalamusState) -> *mut ThalamusStateIter,
    state_iter_next: unsafe extern "C" fn(*mut ThalamusStateIter) -> u8,
    state_iter_key: unsafe extern "C" fn(*mut ThalamusStateIter) -> *mut ThalamusState,
    state_iter_value: unsafe extern "C" fn(*mut ThalamusStateIter) -> *mut ThalamusState,
    state_iter_destroy: unsafe extern "C" fn(*mut ThalamusStateIter),

    state_key_of: unsafe extern "C" fn(*mut ThalamusState, *mut ThalamusState) -> *mut ThalamusState,

    state_recap_with: unsafe extern "C" fn(*mut ThalamusState,
                                               callback: ThalamusStateRecursiveCallback,
                                               data: *mut ::std::os::raw::c_void),

    node_get_state: unsafe extern "C" fn(*mut ThalamusNode) -> *mut ThalamusState,

    threadpool_post: unsafe extern "C" fn(arg1: IoContextPostCallback, data: *mut ::std::os::raw::c_void),

    node_ready_multithreaded_connect: unsafe extern "C" fn(*mut ThalamusNode, ThalamusNodeReadyCallback, *mut ::std::os::raw::c_void) -> *mut ThalamusNodeReadyConnection,

    node_ready_offmain: unsafe extern "C" fn(arg1: *const ThalamusNode),
    node_predrop_ready: unsafe extern "C" fn(arg1: *const ThalamusNode),

    get_vulkan_instance: unsafe extern "C" fn() -> VkInstance,
    get_vulkan_device: unsafe extern "C" fn() -> VkDevice,
    get_vulkan_physical_device: unsafe extern "C" fn() -> VkPhysicalDevice,
    get_vulkan_queue: unsafe extern "C" fn() -> VkQueue,
    create_vulkan_command_pool: unsafe extern "C" fn() -> VkCommandPool,
    lock_vulkan_queue: unsafe extern "C" fn() -> *mut ThalamusVkQueueLock,
    unlock_vulkan_queue: unsafe extern "C" fn(*mut ThalamusVkQueueLock),

    sdl_create_window: unsafe extern "C" fn(name: *mut ThalamusCharSpan, width: i32, height: i32, flags: u64) -> *mut ThalamusSDLWindow,
    sdl_destroy_window: unsafe extern "C" fn(*mut ThalamusSDLWindow),
    sdl_set_window_position: unsafe extern "C" fn(*mut ThalamusSDLWindow, i32, i32),
    sdl_set_window_size: unsafe extern "C" fn(*mut ThalamusSDLWindow, i32, i32),
    sdl_set_window_title: unsafe extern "C" fn(*mut ThalamusSDLWindow, *mut ThalamusCharSpan),
    sdl_get_error: unsafe extern "C" fn(*mut ThalamusCharSpan),
    sdl_vulkan_create_surface: unsafe extern "C" fn(*mut ThalamusSDLWindow, VkInstance, *const VkAllocationCallbacks, *mut VkSurfaceKHR) -> u8,
    sdl_get_window_size_in_pixels: unsafe extern "C" fn(*mut ThalamusSDLWindow, *mut i32, *mut i32),
    sdl_get_window_id: unsafe extern "C" fn(*mut ThalamusSDLWindow) -> u32,
    sdl_get_window_position: unsafe extern "C" fn(*mut ThalamusSDLWindow, *mut i32, *mut i32),
    sdl_get_window_size: unsafe extern "C" fn(*mut ThalamusSDLWindow, *mut i32, *mut i32),
    sdl_events_subscribe: unsafe extern "C" fn(ThalamusSDLEventCallback, *mut ::std::os::raw::c_void) -> *mut ThalamusSDLEventSubscription,
    sdl_events_unsubscribe: unsafe extern "C" fn(*mut ThalamusSDLEventSubscription),

    sdl_get_clipboard_text: unsafe extern "C" fn(*mut ThalamusCharSpan),
    sdl_set_clipboard_text: unsafe extern "C" fn(*const ThalamusCharSpan) -> u8,
    sdl_create_system_cursor: unsafe extern "C" fn(i32) -> *mut ThalamusSDLCursor,
    sdl_set_cursor: unsafe extern "C" fn(*mut ThalamusSDLCursor) -> u8,
    sdl_destroy_cursor: unsafe extern "C" fn(*mut ThalamusSDLCursor),
    sdl_show_cursor: unsafe extern "C" fn() -> u8,
    sdl_hide_cursor: unsafe extern "C" fn() -> u8,

    state_make_dict: unsafe extern "C" fn() -> *mut ThalamusState,
    state_make_list: unsafe extern "C" fn() -> *mut ThalamusState,

    state_set_at_name_state_with_callback: unsafe extern "C" fn(*mut ThalamusState, *const ThalamusCharSpan, *mut ThalamusState, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_set_at_name_string_with_callback: unsafe extern "C" fn(*mut ThalamusState, *const ThalamusCharSpan, *const ThalamusCharSpan, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_set_at_name_int_with_callback: unsafe extern "C" fn(*mut ThalamusState, *const ThalamusCharSpan, i64, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_set_at_name_float_with_callback: unsafe extern "C" fn(*mut ThalamusState, *const ThalamusCharSpan, f64, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_set_at_name_null_with_callback: unsafe extern "C" fn(*mut ThalamusState, *const ThalamusCharSpan, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_set_at_name_bool_with_callback: unsafe extern "C" fn(*mut ThalamusState, *const ThalamusCharSpan, i8, IoContextPostCallback, *mut ::std::os::raw::c_void),

    state_set_at_index_state_with_callback: unsafe extern "C" fn(*mut ThalamusState, i64, *mut ThalamusState, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_set_at_index_string_with_callback: unsafe extern "C" fn(*mut ThalamusState, i64, *const ThalamusCharSpan, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_set_at_index_int_with_callback: unsafe extern "C" fn(*mut ThalamusState, i64, i64, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_set_at_index_float_with_callback: unsafe extern "C" fn(*mut ThalamusState, i64, f64, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_set_at_index_null_with_callback: unsafe extern "C" fn(*mut ThalamusState, i64, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_set_at_index_bool_with_callback: unsafe extern "C" fn(*mut ThalamusState, i64, i8, IoContextPostCallback, *mut ::std::os::raw::c_void),

    state_push_state_with_callback: unsafe extern "C" fn(*mut ThalamusState, *mut ThalamusState, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_push_string_with_callback: unsafe extern "C" fn(*mut ThalamusState, *const ThalamusCharSpan, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_push_int_with_callback: unsafe extern "C" fn(*mut ThalamusState, i64, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_push_float_with_callback: unsafe extern "C" fn(*mut ThalamusState, f64, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_push_null_with_callback: unsafe extern "C" fn(*mut ThalamusState, IoContextPostCallback, *mut ::std::os::raw::c_void),
    state_push_bool_with_callback: unsafe extern "C" fn(*mut ThalamusState, i8, IoContextPostCallback, *mut ::std::os::raw::c_void),
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusNodeGetConnection {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusNodeReadyConnection {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusRequestHandle {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusJson {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusSerialPort {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusStreamBuf {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusErrorCode {
    _unused: [u8; 0],
}

pub type ThalamusTimerCallback = ::std::option::Option<
    unsafe extern "C" fn(arg1: *mut ThalamusErrorCode, data: *mut ::std::os::raw::c_void),
>;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusStateConnection {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusTimer {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusState {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusStateIter {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusIoContext {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusNodeGraph {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusVkQueueLock {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VkInstance_T {
    _unused: [u8; 0],
}
pub type VkInstance = *mut VkInstance_T;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VkPhysicalDevice_T {
    _unused: [u8; 0],
}
pub type VkPhysicalDevice = *mut VkPhysicalDevice_T;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VkDevice_T {
    _unused: [u8; 0],
}
pub type VkDevice = *mut VkDevice_T;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VkQueue_T {
    _unused: [u8; 0],
}
pub type VkQueue = *mut VkQueue_T;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VkCommandPool_T {
    _unused: [u8; 0],
}
pub type VkCommandPool = *mut VkCommandPool_T;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VkSurfaceKHR_T {
    _unused: [u8; 0],
}
pub type VkSurfaceKHR = *mut VkSurfaceKHR_T;

/// Only ever used as a `*const` pointer (always passed as null from Rust --
/// we have no allocation callbacks to supply), so it's never dereferenced;
/// kept opaque like the Vk*_T handles above rather than modeling its real
/// (function-pointer-filled) fields.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct VkAllocationCallbacks {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusSDLWindow {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusSDLEventSubscription {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusSDLCursor {
    _unused: [u8; 0],
}

pub type ThalamusSDLEventCallback = ::std::option::Option<
    unsafe extern "C" fn(event: *mut ThalamusSDLEvent, data: *mut ::std::os::raw::c_void),
>;

// SDL window creation flags (subset of THALAMUS_SDL_WINDOW_* in plugin.h --
// numeric values must stay in lockstep with that header).
pub const THALAMUS_SDL_WINDOW_HIDDEN: u64 = 0x0000000000000008;
pub const THALAMUS_SDL_WINDOW_RESIZABLE: u64 = 0x0000000000000020;
pub const THALAMUS_SDL_WINDOW_VULKAN: u64 = 0x0000000010000000;

// Mirrors THALAMUS_SDL_SYSTEM_CURSOR_* in plugin.h (itself mirroring SDL3's
// SDL_SystemCursor ordinals) -- only the entries imgui ever requests.
pub const THALAMUS_SDL_SYSTEM_CURSOR_DEFAULT: i32 = 0;
pub const THALAMUS_SDL_SYSTEM_CURSOR_TEXT: i32 = 1;
pub const THALAMUS_SDL_SYSTEM_CURSOR_WAIT: i32 = 2;
pub const THALAMUS_SDL_SYSTEM_CURSOR_CROSSHAIR: i32 = 3;
pub const THALAMUS_SDL_SYSTEM_CURSOR_PROGRESS: i32 = 4;
pub const THALAMUS_SDL_SYSTEM_CURSOR_NWSE_RESIZE: i32 = 5;
pub const THALAMUS_SDL_SYSTEM_CURSOR_NESW_RESIZE: i32 = 6;
pub const THALAMUS_SDL_SYSTEM_CURSOR_EW_RESIZE: i32 = 7;
pub const THALAMUS_SDL_SYSTEM_CURSOR_NS_RESIZE: i32 = 8;
pub const THALAMUS_SDL_SYSTEM_CURSOR_MOVE: i32 = 9;
pub const THALAMUS_SDL_SYSTEM_CURSOR_NOT_ALLOWED: i32 = 10;
pub const THALAMUS_SDL_SYSTEM_CURSOR_POINTER: i32 = 11;

// SDL event type tags actually consumed by the imgui backend (subset of
// THALAMUS_SDL_EventType in plugin_window_event.h).
pub const THALAMUS_SDL_EVENT_QUIT: u32 = 0x100;
pub const THALAMUS_SDL_EVENT_WINDOW_RESIZED: u32 = 0x206;
pub const THALAMUS_SDL_EVENT_WINDOW_FOCUS_GAINED: u32 = 0x20E;
pub const THALAMUS_SDL_EVENT_WINDOW_FOCUS_LOST: u32 = 0x20F;
pub const THALAMUS_SDL_EVENT_WINDOW_CLOSE_REQUESTED: u32 = 0x210;
pub const THALAMUS_SDL_EVENT_KEY_DOWN: u32 = 0x300;
pub const THALAMUS_SDL_EVENT_KEY_UP: u32 = 0x301;
pub const THALAMUS_SDL_EVENT_MOUSE_MOTION: u32 = 0x400;
pub const THALAMUS_SDL_EVENT_MOUSE_BUTTON_DOWN: u32 = 0x401;
pub const THALAMUS_SDL_EVENT_MOUSE_BUTTON_UP: u32 = 0x402;
pub const THALAMUS_SDL_EVENT_MOUSE_WHEEL: u32 = 0x403;

/// Mirrors THALAMUS_SDL_Event (itself a shadow of SDL3's SDL_Event union --
/// see plugin_window_event.h). Rather than transcribe all ~35 variants, this
/// stays an opaque, correctly-sized byte buffer; decode only the variants
/// actually used via the `as_*` accessors below, gated on `event_type()`
/// matching the corresponding THALAMUS_SDL_EVENT_* constant first.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ThalamusSDLEvent {
    pub bytes: [u8; 128],
}

impl ThalamusSDLEvent {
    pub fn event_type(&self) -> u32 {
        u32::from_ne_bytes(self.bytes[0..4].try_into().unwrap())
    }
    /// # Safety
    /// `event_type()` must be one of the THALAMUS_SDL_EVENT_WINDOW_* constants.
    pub unsafe fn as_window(&self) -> &ThalamusSDLWindowEvent {
        unsafe { &*(self.bytes.as_ptr() as *const ThalamusSDLWindowEvent) }
    }
    /// # Safety
    /// `event_type()` must be THALAMUS_SDL_EVENT_KEY_DOWN or _KEY_UP.
    pub unsafe fn as_key(&self) -> &ThalamusSDLKeyboardEvent {
        unsafe { &*(self.bytes.as_ptr() as *const ThalamusSDLKeyboardEvent) }
    }
    /// # Safety
    /// `event_type()` must be THALAMUS_SDL_EVENT_MOUSE_MOTION.
    pub unsafe fn as_mouse_motion(&self) -> &ThalamusSDLMouseMotionEvent {
        unsafe { &*(self.bytes.as_ptr() as *const ThalamusSDLMouseMotionEvent) }
    }
    /// # Safety
    /// `event_type()` must be THALAMUS_SDL_EVENT_MOUSE_BUTTON_DOWN or _UP.
    pub unsafe fn as_mouse_button(&self) -> &ThalamusSDLMouseButtonEvent {
        unsafe { &*(self.bytes.as_ptr() as *const ThalamusSDLMouseButtonEvent) }
    }
    /// # Safety
    /// `event_type()` must be THALAMUS_SDL_EVENT_MOUSE_WHEEL.
    pub unsafe fn as_mouse_wheel(&self) -> &ThalamusSDLMouseWheelEvent {
        unsafe { &*(self.bytes.as_ptr() as *const ThalamusSDLMouseWheelEvent) }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusSDLWindowEvent {
    pub r#type: u32,
    pub reserved: u32,
    pub timestamp: u64,
    pub window_id: u32,
    pub data1: i32,
    pub data2: i32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusSDLKeyboardEvent {
    pub r#type: u32,
    pub reserved: u32,
    pub timestamp: u64,
    pub window_id: u32,
    pub which: u32,
    pub scancode: u32,
    pub key: u32,
    pub modifiers: u16,
    pub raw: u16,
    pub down: bool,
    pub repeat: bool,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusSDLMouseMotionEvent {
    pub r#type: u32,
    pub reserved: u32,
    pub timestamp: u64,
    pub window_id: u32,
    pub which: u32,
    pub state: u32,
    pub x: f32,
    pub y: f32,
    pub xrel: f32,
    pub yrel: f32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusSDLMouseButtonEvent {
    pub r#type: u32,
    pub reserved: u32,
    pub timestamp: u64,
    pub window_id: u32,
    pub which: u32,
    pub button: u8,
    pub down: bool,
    pub clicks: u8,
    pub padding: u8,
    pub x: f32,
    pub y: f32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusSDLMouseWheelEvent {
    pub r#type: u32,
    pub reserved: u32,
    pub timestamp: u64,
    pub window_id: u32,
    pub which: u32,
    pub x: f32,
    pub y: f32,
    pub direction: u32,
    pub mouse_x: f32,
    pub mouse_y: f32,
    pub integer_x: i32,
    pub integer_y: i32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusDoubleSpan {
    pub data: *const f64,
    pub size: u64,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusShortSpan {
    pub data: *const ::std::os::raw::c_short,
    pub size: u64,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusIntSpan {
    pub data: *const ::std::os::raw::c_int,
    pub size: u64,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusULongSpan {
    pub data: *const u64,
    pub size: u64,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusCharSpan {
    pub data: *const i8,
    pub size: u64,
    pub owns_data: i8,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusByteSpan {
    pub data: *const u8,
    pub size: u64,
}
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusMutableByteSpan {
    pub data: *mut u8,
    pub size: u64,
}
  
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusMocapSegment {
  pub frame: u32,
  pub segment_id: u32,
  pub time: u32,
  pub position: [f32; 3],
  pub rotation: [f32; 4],
  pub actor: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusMocapSegmentSpan {
  pub data: *const ThalamusMocapSegment,
  pub size: u64
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusNodeSelector {
  pub name: ThalamusCharSpan,
  pub _type: ThalamusCharSpan,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusMocapNode {
    pub segments: ::std::option::Option<
        unsafe extern "C" fn(
            *mut ThalamusMocapSegmentSpan,
            node: *mut ThalamusNode,
        ),
    >,
    pub pose_name: ::std::option::Option<
        unsafe extern "C" fn(
            *mut ThalamusCharSpan,
            node: *mut ThalamusNode,
        ),
    >,
    pub has_motion_data: ::std::option::Option<
        unsafe extern "C" fn(node: *mut ThalamusNode) -> ::std::os::raw::c_char,
    >,
}

impl ThalamusMocapNode {
  pub fn new() -> ThalamusMocapNode {
    ThalamusMocapNode {
      segments: None,
      pose_name: None,
      has_motion_data: None,
    }
  }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ThalamusTextNode {
    pub text: ::std::option::Option<
        unsafe extern "C" fn(
            *mut ThalamusCharSpan,
            node: *mut ThalamusNode,
        ),
    >,
    pub has_text_data: ::std::option::Option<
        unsafe extern "C" fn(node: *mut ThalamusNode) -> ::std::os::raw::c_char,
    >,
}

impl ThalamusTextNode {
  pub fn new() -> ThalamusTextNode {
    ThalamusTextNode {
      text: None,
      has_text_data: None,
    }
  }
}

#[repr(C)]
pub struct ThalamusNodeFactory {
    type_: ThalamusCharSpan,
    create: ::std::option::Option<
        unsafe extern "C" fn(
            factory: *mut ThalamusNodeFactory,
            arg1: *mut ThalamusState,
            arg2: *mut ThalamusIoContext,
            arg3: *mut ThalamusNodeGraph,
        ) -> *mut ThalamusNode,
    >,
    destroy: ::std::option::Option<unsafe extern "C" fn(factory: *mut ThalamusNodeFactory, arg1: *mut ThalamusNode)>,
    prepare: ::std::option::Option<unsafe extern "C" fn(factory: *mut ThalamusNodeFactory) -> ::std::os::raw::c_char>,
    cleanup: ::std::option::Option<unsafe extern "C" fn(factory: *mut ThalamusNodeFactory)>,
    api: *mut ThalamusAPIRaw,
    c_str: CString
}

pub(crate) struct PluginImpl {
  api: crate::api::ThalamusAPI,
  node_token: crate::api::NodeToken,
  node: Box<dyn crate::api::Node>,
  pub(crate) data: Option<&'static dyn NodeData>,
}

fn deref_plugin_impl(c_node: &ThalamusNode) -> &PluginImpl {
    unsafe { &*(c_node.plugin_impl as *const PluginImpl) }
}

pub(crate) fn plugin_impl_ptr(c_node: *mut ThalamusNode) -> *mut PluginImpl {
    unsafe { (*c_node).plugin_impl as *mut PluginImpl }
}

pub extern "C" fn c_node_analog_data(output: *mut ThalamusDoubleSpan, raw_node: *mut ThalamusNode, channel: ::std::os::raw::c_int) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();

  let result = analog.data(channel);
  unsafe {
    (&mut *output).data = result.as_ptr();
    (&mut *output).size = result.len() as u64;
  }
}

pub extern "C" fn c_node_analog_short_data(output: *mut ThalamusShortSpan, raw_node: *mut ThalamusNode, channel: ::std::os::raw::c_int) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();

  let result = analog.short_data(channel);
  unsafe {
    (&mut *output).data = result.as_ptr();
    (&mut *output).size = result.len() as u64;
  }
}

pub extern "C" fn c_node_analog_int_data(output: *mut ThalamusIntSpan, raw_node: *mut ThalamusNode, channel: ::std::os::raw::c_int) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();

  let result = analog.int_data(channel);
  unsafe {
    (&mut *output).data = result.as_ptr();
    (&mut *output).size = result.len() as u64;
  }
}

pub extern "C" fn c_node_analog_ulong_data(output: *mut ThalamusULongSpan, raw_node: *mut ThalamusNode, channel: ::std::os::raw::c_int) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();

  let result = analog.ulong_data(channel);
  unsafe {
    (&mut *output).data = result.as_ptr();
    (&mut *output).size = result.len() as u64;
  }
}

pub extern "C" fn c_node_analog_num_channels(raw_node: *mut ThalamusNode) -> i32 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  analog.num_channels()
}

pub extern "C" fn c_node_analog_sample_interval_ns(raw_node: *mut ThalamusNode, channel: ::std::os::raw::c_int) -> u64 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  analog.sample_interval(channel).as_nanos() as u64
}

pub extern "C" fn c_node_analog_name(output: *mut ThalamusByteSpan, raw_node: *mut ThalamusNode, channel: ::std::os::raw::c_int) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  let result = analog.name(channel);
  unsafe {
    (&mut *output).data = result.as_ptr();
    (&mut *output).size = result.len() as u64;
  }
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_analog_has_analog_data(raw_node: *mut ThalamusNode) -> i8 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.and_then(|data| data.analog());
  if analog.is_some() {1}else{0}
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_analog_is_short_data(raw_node: *mut ThalamusNode) -> i8 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  if analog.is_short_data() {1}else{0}
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_analog_is_int_data(raw_node: *mut ThalamusNode) -> i8 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  if analog.is_int_data() {1}else{0}
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_analog_is_ulong_data(raw_node: *mut ThalamusNode) -> i8 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  if analog.is_ulong_data() {1}else{0}
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_analog_is_transformed(raw_node: *mut ThalamusNode) -> i8 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  if analog.is_transformed() {1}else{0}
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_analog_scale(raw_node: *mut ThalamusNode, channel: i32) -> f64 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  analog.scale(channel)
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_analog_offset(raw_node: *mut ThalamusNode, channel: i32) -> f64 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  analog.offset(channel)
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_time_ns(raw_node: *mut ThalamusNode) -> u64 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let data = deref_plugin_impl(c_node).data.unwrap();
  data.time().as_nanos() as u64
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_process(raw_node: *mut ThalamusNode, arg1: *mut ThalamusRequestHandle, arg2: *mut ThalamusJson) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let _impl = deref_plugin_impl(c_node);

  let handle = crate::api::Request{ api: _impl.api, handle: arg1};
  let json = crate::api::Json::new(_impl.api, arg2);
  _impl.node.process(handle, json);
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_predrop(raw_node: *mut ThalamusNode) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let _impl = deref_plugin_impl(c_node);

  let token = PredropToken{api: _impl.api.raw, node: raw_node};
  _impl.node.predrop(token);
}

pub extern "C" fn c_node_image_plane(output: *mut ThalamusByteSpan, raw_node: *mut ThalamusNode, channel: ::std::os::raw::c_int) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let image = deref_plugin_impl(c_node).data.unwrap().image().unwrap();

  let result = image.plane(channel);
  unsafe {
    (&mut *output).data = result.as_ptr();
    (&mut *output).size = result.len() as u64;
  }
}

pub extern "C" fn c_node_image_num_planes(raw_node: *mut ThalamusNode) -> u64 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let image = deref_plugin_impl(c_node).data.unwrap().image().unwrap();
  image.num_planes()
}

pub extern "C" fn c_node_image_format(raw_node: *mut ThalamusNode) -> ThalamusImageFormat {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let image = deref_plugin_impl(c_node).data.unwrap().image().unwrap();
  match image.format() {
    ImageFormat::Gray => ThalamusImageFormat::Gray,
    ImageFormat::RGB => ThalamusImageFormat::RGB,
    ImageFormat::YUYV422 => ThalamusImageFormat::YUYV422,
    ImageFormat::YUV420P => ThalamusImageFormat::YUV420P,
    ImageFormat::YUVJ420P => ThalamusImageFormat::YUVJ420P,
  }
}

pub extern "C" fn c_node_image_width(raw_node: *mut ThalamusNode) -> u64 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let image = deref_plugin_impl(c_node).data.unwrap().image().unwrap();
  image.width()
}

pub extern "C" fn c_node_image_height(raw_node: *mut ThalamusNode) -> u64 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let image = deref_plugin_impl(c_node).data.unwrap().image().unwrap();
  image.height()
}

pub extern "C" fn c_node_image_frame_interval_ns(raw_node: *mut ThalamusNode) -> u64 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let image = deref_plugin_impl(c_node).data.unwrap().image().unwrap();
  image.frame_interval().as_nanos() as u64
}

#[allow(non_snake_case)]
pub extern "C" fn c_node_image_has_image_data(raw_node: *mut ThalamusNode) -> i8 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let image = deref_plugin_impl(c_node).data.and_then(|data| data.image());
  if image.is_some() {1}else{0}
}

pub extern "C" fn c_node_mocap_segments(output: *mut ThalamusMocapSegmentSpan, raw_node: *mut ThalamusNode) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let mocap = deref_plugin_impl(c_node).data.unwrap().mocap().unwrap();
  let result = mocap.segments();
  unsafe {
    (&mut *output).data = result.as_ptr();
    (&mut *output).size = result.len() as u64;
  }
}

pub extern "C" fn c_node_mocap_pose_name(output: *mut ThalamusCharSpan, raw_node: *mut ThalamusNode) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let mocap = deref_plugin_impl(c_node).data.unwrap().mocap().unwrap();
  let result = mocap.pose_name();
  unsafe {
    (&mut *output).data = result.as_ptr() as *const i8;
    (&mut *output).size = result.len() as u64;
  }
}

#[allow(non_snake_case)]
pub extern "C" fn c_node_mocap_has_motion_data(raw_node: *mut ThalamusNode) -> i8 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let mocap = deref_plugin_impl(c_node).data.and_then(|data| data.mocap());
  if mocap.is_some() {1}else{0}
}

pub extern "C" fn c_node_text_text(output: *mut ThalamusCharSpan, raw_node: *mut ThalamusNode) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let text = deref_plugin_impl(c_node).data.unwrap().text().unwrap();
  let result = text.text();
  unsafe {
    (&mut *output).data = result.as_ptr() as *const i8;
    (&mut *output).size = result.len() as u64;
  }
}

#[allow(non_snake_case)]
pub extern "C" fn c_node_text_has_text_data(raw_node: *mut ThalamusNode) -> i8 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let text = deref_plugin_impl(c_node).data.and_then(|data| data.text());
  if text.is_some() {1}else{0}
}

fn wrap_analog(c_node: &mut ThalamusNode) {
  c_node.analog = Box::into_raw(Box::new(ThalamusAnalogNode::new()));
  unsafe {
    (*c_node.analog).data = Some(c_node_analog_data);
    (*c_node.analog).short_data = Some(c_node_analog_short_data);
    (*c_node.analog).int_data = Some(c_node_analog_int_data);
    (*c_node.analog).ulong_data = Some(c_node_analog_ulong_data);
    (*c_node.analog).num_channels = Some(c_node_analog_num_channels);
    (*c_node.analog).sample_interval_ns = Some(c_node_analog_sample_interval_ns);
    (*c_node.analog).name = Some(c_node_analog_name);
    (*c_node.analog).has_analog_data = Some(c_node_analog_has_analog_data);
    (*c_node.analog).is_short_data = Some(c_node_analog_is_short_data);
    (*c_node.analog).is_int_data = Some(c_node_analog_is_int_data);
    (*c_node.analog).is_ulong_data = Some(c_node_analog_is_ulong_data);
    (*c_node.analog).is_transformed = Some(c_node_analog_is_transformed);
    (*c_node.analog).scale = Some(c_node_analog_scale);
    (*c_node.analog).offset = Some(c_node_analog_offset);
  }
}

fn wrap_image(c_node: &mut ThalamusNode) {
  c_node.image = Box::into_raw(Box::new(ThalamusImageNode::new()));
  unsafe {
    (*c_node.image).plane = Some(c_node_image_plane);
    (*c_node.image).num_planes = Some(c_node_image_num_planes);
    (*c_node.image).format = Some(c_node_image_format);
    (*c_node.image).width = Some(c_node_image_width);
    (*c_node.image).height = Some(c_node_image_height);
    (*c_node.image).frame_interval_ns = Some(c_node_image_frame_interval_ns);
    (*c_node.image).has_image_data = Some(c_node_image_has_image_data);
  }
}

fn wrap_mocap(c_node: &mut ThalamusNode) {
  c_node.mocap = Box::into_raw(Box::new(ThalamusMocapNode::new()));
  unsafe {
    (*c_node.mocap).segments = Some(c_node_mocap_segments);
    (*c_node.mocap).pose_name = Some(c_node_mocap_pose_name);
    (*c_node.mocap).has_motion_data = Some(c_node_mocap_has_motion_data);
  }
}

fn wrap_text(c_node: &mut ThalamusNode) {
  c_node.text = Box::into_raw(Box::new(ThalamusTextNode::new()));
  unsafe {
    (*c_node.text).text = Some(c_node_text_text);
    (*c_node.text).has_text_data = Some(c_node_text_has_text_data);
  }
}

extern "C" fn create_node_template<T: crate::api::Node + 'static>(factory: *mut ThalamusNodeFactory, state: *mut ThalamusState, _io_context: *mut ThalamusIoContext, _graph: *mut ThalamusNodeGraph) -> *mut ThalamusNode {
  println!("create_node_template");
  let api_raw = unsafe { (*factory).api };
  let c_node = Box::into_raw(Box::new(ThalamusNode {
    c_impl: ptr::null_mut() as *mut ::std::os::raw::c_void,
    time_ns: None,
    analog: ptr::null_mut() as *mut ThalamusAnalogNode,
    mocap: ptr::null_mut() as *mut ThalamusMocapNode,
    image: ptr::null_mut() as *mut ThalamusImageNode,
    text: ptr::null_mut() as *mut ThalamusTextNode,
    plugin_impl: ptr::null_mut() as *mut ::std::os::raw::c_void,
    process: None,
    predrop: None,
    signals_offmain: 0,
  }));
  let c_node_ref = unsafe {&mut*c_node};
  let api = ThalamusAPI{raw: api_raw};
  let node_token = crate::api::NodeToken::new(c_node);

  let token = unsafe { crate::api::MainThreadToken::new_in_main_thread_callback() };
  let node = T::new(api, node_token.clone(), crate::api::State::new(api, state), token);
  c_node_ref.signals_offmain = if node.signals_offmain() { 1 } else { 0 };
  let modalities = node.modalities();

  let result = Box::new(PluginImpl {
    api,
    node_token,
    node: Box::new(node) as Box<dyn crate::api::Node>,
    data: None,
  });

  c_node_ref.time_ns = Some(c_node_time_ns);
  c_node_ref.process = Some(c_node_process);
  c_node_ref.predrop = Some(c_node_predrop);

  if modalities & THALAMUS_MODALITY_ANALOG != 0 {
    wrap_analog(c_node_ref);
  }
  if modalities & THALAMUS_MODALITY_MOCAP != 0 {
    wrap_mocap(c_node_ref);
  }
  if modalities & THALAMUS_MODALITY_IMAGE != 0 {
    wrap_image(c_node_ref);
  }
  if modalities & THALAMUS_MODALITY_TEXT != 0 {
    wrap_text(c_node_ref);
  }

  c_node_ref.plugin_impl = Box::into_raw(result) as *mut ::std::os::raw::c_void;
  c_node
}

unsafe extern "C" fn destroy_node_template(_factory: *mut ThalamusNodeFactory, node_raw: *mut ThalamusNode) {
  println!("destroy_node_template");
  unsafe {
    let node = &*node_raw;
    let plugin = &*(node.plugin_impl as *const PluginImpl);
    // Null the token first so any NodeToken clone still held by a pending
    // post_to_main/post_to_threadpool callback sees the node as destroyed
    // instead of dereferencing freed memory.
    plugin.node_token.destroy();
    drop(Box::from_raw(node.plugin_impl as *mut PluginImpl));
    drop(Box::from_raw(node_raw));
  }
}

extern "C" fn prepare_node_template<T: crate::api::Node>(_factory: *mut ThalamusNodeFactory) -> ::std::os::raw::c_char {
  if T::prepare() { 1 } else { 0 }
}

extern "C" fn cleanup_node_template<T: crate::api::Node>(_factory: *mut ThalamusNodeFactory) {
  T::cleanup();
}

impl ThalamusNodeFactory {
  pub fn new<T: crate::api::Node + 'static>(name: &str, api: *mut ThalamusAPIRaw) -> *mut ThalamusNodeFactory {
    println!("ThalamusNodeFactory::new {}", name);
    let c_name = CString::new(name).unwrap();
    let result = Box::into_raw(Box::new(ThalamusNodeFactory {
      type_: ThalamusCharSpan { data: c_name.as_ptr(), size: name.len() as u64, owns_data: 0 },
      create: Some(create_node_template::<T>),
      destroy: Some(destroy_node_template),
      prepare: Some(prepare_node_template::<T>),
      cleanup: Some(cleanup_node_template::<T>),
      api: api,
      c_str: c_name
    }));
    result as *mut ThalamusNodeFactory
  }
}

