use crate::api::ImageFormat;
use crate::api::{IntoNodeHandle, NodeData, PredropToken, ThalamusAPI};
use std::os::raw::c_char;
use std::ptr;

/// Bindgen output for Thalamus's plugin.h and modalities.h (see build.rs).
mod generated {
  #![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code
  )]
  include!(concat!(env!("OUT_DIR"), "/thalamus_api.rs"));
}

pub use generated::*;

impl ThalamusAPIRaw {
  /// Copies the function table Thalamus passed in into a Rust-owned struct,
  /// leaving every function field Thalamus didn't provide as `None`.
  ///
  /// `version` is filled in by Thalamus as a count of how many of these
  /// functions it actually initialized. Since this struct is only ever
  /// appended to, a thalamus-contrib build newer than the running Thalamus
  /// knows about fields that don't exist in Thalamus's (smaller) struct at
  /// all, so only `version` and the first `version` function pointers are
  /// read from `host`; nothing past that is read or written.
  ///
  /// # Safety
  /// `host` must point to a ThalamusAPI whose `version` field is accurate.
  pub unsafe fn copy_from_host(host: *const ThalamusAPIRaw) -> ThalamusAPIRaw {
    type Slot = ::std::option::Option<unsafe extern "C" fn()>;
    const FIRST: usize = ::std::mem::offset_of!(ThalamusAPIRaw, state_is_dict);
    const TAIL: usize = ::std::mem::size_of::<ThalamusAPIRaw>() - FIRST;
    const COUNT: usize = TAIL / ::std::mem::size_of::<Slot>();
    // Everything after `version` must be a function pointer for the
    // slot-wise copy below to be valid.
    const _: () = assert!(TAIL % ::std::mem::size_of::<Slot>() == 0);

    unsafe {
      // Read through a raw pointer rather than a reference: `host` may
      // point to fewer bytes than size_of::<ThalamusAPIRaw>().
      let version = ::std::ptr::addr_of!((*host).version).read();
      let initialized = version.clamp(0, COUNT as i32) as usize;
      // All-zero is valid: version 0 and every function field None.
      let mut api: ThalamusAPIRaw = ::std::mem::zeroed();
      ::std::ptr::copy_nonoverlapping(
        host as *const u8,
        &mut api as *mut ThalamusAPIRaw as *mut u8,
        FIRST + initialized * ::std::mem::size_of::<Slot>(),
      );
      api
    }
  }
}

/// Holds the Node trait object however Node::new() produced it: freshly
/// boxed by the framework (the common case, when new() returns Self), or as
/// the Arc/Rc/Arc<Mutex<_>>/Rc<RefCell<_>> a node's own new() handed back
/// (e.g. because it already shared that pointer with a background thread or
/// another owner during construction). See IntoNodeHandle.
pub enum NodeHandle {
  Owned(Box<dyn crate::api::Node>),
  Shared(std::sync::Arc<dyn crate::api::Node>),
  Local(std::rc::Rc<dyn crate::api::Node>),
  SharedLocked(std::sync::Arc<std::sync::Mutex<dyn crate::api::Node>>),
  LocalLocked(std::rc::Rc<std::cell::RefCell<dyn crate::api::Node>>),
}

impl NodeHandle {
  /// Calls f with a reference to the underlying Node. Takes a closure
  /// rather than returning &dyn Node because the locked variants can only
  /// hand out a reference for the duration of a lock()/borrow() call.
  fn with_node<R>(&self, f: impl FnOnce(&dyn crate::api::Node) -> R) -> R {
    match self {
      NodeHandle::Owned(b) => f(b.as_ref()),
      NodeHandle::Shared(a) => f(a.as_ref()),
      NodeHandle::Local(r) => f(r.as_ref()),
      NodeHandle::SharedLocked(m) => f(&*m.lock().unwrap()),
      NodeHandle::LocalLocked(c) => f(&*c.borrow()),
    }
  }
}

pub(crate) struct PluginImpl {
  api: crate::api::ThalamusAPI,
  node_token: crate::api::NodeToken,
  node: NodeHandle,
  pub(crate) data: Option<&'static dyn NodeData>,
}

fn deref_plugin_impl(c_node: &ThalamusNode) -> &PluginImpl {
  unsafe { &*(c_node.plugin_impl as *const PluginImpl) }
}

pub(crate) fn plugin_impl_ptr(c_node: *mut ThalamusNode) -> *mut PluginImpl {
  unsafe { (*c_node).plugin_impl as *mut PluginImpl }
}

pub extern "C" fn c_node_analog_data(
  output: *mut ThalamusDoubleSpan,
  raw_node: *mut ThalamusNode,
  channel: ::std::os::raw::c_int,
) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();

  let result = analog.data(channel);
  unsafe {
    (&mut *output).data = result.as_ptr();
    (&mut *output).size = result.len() as u64;
  }
}

pub extern "C" fn c_node_analog_short_data(
  output: *mut ThalamusShortSpan,
  raw_node: *mut ThalamusNode,
  channel: ::std::os::raw::c_int,
) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();

  let result = analog.short_data(channel);
  unsafe {
    (&mut *output).data = result.as_ptr();
    (&mut *output).size = result.len() as u64;
  }
}

pub extern "C" fn c_node_analog_int_data(
  output: *mut ThalamusIntSpan,
  raw_node: *mut ThalamusNode,
  channel: ::std::os::raw::c_int,
) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();

  let result = analog.int_data(channel);
  unsafe {
    (&mut *output).data = result.as_ptr();
    (&mut *output).size = result.len() as u64;
  }
}

pub extern "C" fn c_node_analog_ulong_data(
  output: *mut ThalamusULongSpan,
  raw_node: *mut ThalamusNode,
  channel: ::std::os::raw::c_int,
) {
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

pub extern "C" fn c_node_analog_sample_interval_ns(
  raw_node: *mut ThalamusNode,
  channel: ::std::os::raw::c_int,
) -> u64 {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  analog.sample_interval(channel).as_nanos() as u64
}

pub extern "C" fn c_node_analog_name(
  output: *mut ThalamusCharSpan,
  raw_node: *mut ThalamusNode,
  channel: ::std::os::raw::c_int,
) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  let result = analog.name(channel);
  unsafe {
    (&mut *output).data = result.as_ptr() as *const c_char;
    (&mut *output).size = result.len() as u64;
    (&mut *output).owns_data = 0;
  }
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_analog_has_analog_data(raw_node: *mut ThalamusNode) -> c_char {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node)
    .data
    .and_then(|data| data.analog());
  if analog.is_some() { 1 } else { 0 }
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_analog_is_short_data(raw_node: *mut ThalamusNode) -> c_char {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  if analog.is_short_data() { 1 } else { 0 }
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_analog_is_int_data(raw_node: *mut ThalamusNode) -> c_char {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  if analog.is_int_data() { 1 } else { 0 }
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_analog_is_ulong_data(raw_node: *mut ThalamusNode) -> c_char {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  if analog.is_ulong_data() { 1 } else { 0 }
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_analog_is_transformed(raw_node: *mut ThalamusNode) -> c_char {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let analog = deref_plugin_impl(c_node).data.unwrap().analog().unwrap();
  if analog.is_transformed() { 1 } else { 0 }
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
pub extern "C" fn c_node_process(
  raw_node: *mut ThalamusNode,
  arg1: *mut ThalamusRequestHandle,
  arg2: *mut ThalamusJson,
) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let _impl = deref_plugin_impl(c_node);

  let handle = crate::api::Request {
    api: _impl.api,
    handle: arg1,
  };
  let json = crate::api::Json::new(_impl.api, arg2);
  _impl.node.with_node(|node| node.process(handle, json));
}
#[allow(non_snake_case)]
pub extern "C" fn c_node_predrop(raw_node: *mut ThalamusNode) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let _impl = deref_plugin_impl(c_node);

  let token = PredropToken {
    api: _impl.api.raw,
    node: raw_node,
  };
  _impl.node.with_node(|node| node.predrop(token));
}

pub extern "C" fn c_node_image_plane(
  output: *mut ThalamusByteSpan,
  raw_node: *mut ThalamusNode,
  channel: ::std::os::raw::c_int,
) {
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
    ImageFormat::NV12 => ThalamusImageFormat::NV12,
    ImageFormat::BGR => ThalamusImageFormat::BGR,
    ImageFormat::MJPEG => ThalamusImageFormat::MJPEG,
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
pub extern "C" fn c_node_image_has_image_data(raw_node: *mut ThalamusNode) -> c_char {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let image = deref_plugin_impl(c_node).data.and_then(|data| data.image());
  if image.is_some() { 1 } else { 0 }
}

pub extern "C" fn c_node_mocap_segments(
  output: *mut ThalamusMocapSegmentSpan,
  raw_node: *mut ThalamusNode,
) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let mocap = deref_plugin_impl(c_node).data.unwrap().mocap().unwrap();
  let result = mocap.segments();
  unsafe {
    (&mut *output).data = result.as_ptr();
    (&mut *output).size = result.len() as u64;
  }
}

pub extern "C" fn c_node_mocap_pose_name(
  output: *mut ThalamusCharSpan,
  raw_node: *mut ThalamusNode,
) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let mocap = deref_plugin_impl(c_node).data.unwrap().mocap().unwrap();
  let result = mocap.pose_name();
  unsafe {
    (&mut *output).data = result.as_ptr() as *const c_char;
    (&mut *output).size = result.len() as u64;
  }
}

#[allow(non_snake_case)]
pub extern "C" fn c_node_mocap_has_motion_data(raw_node: *mut ThalamusNode) -> c_char {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let mocap = deref_plugin_impl(c_node).data.and_then(|data| data.mocap());
  if mocap.is_some() { 1 } else { 0 }
}

pub extern "C" fn c_node_text_text(output: *mut ThalamusCharSpan, raw_node: *mut ThalamusNode) {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let text = deref_plugin_impl(c_node).data.unwrap().text().unwrap();
  let result = text.text();
  unsafe {
    (&mut *output).data = result.as_ptr() as *const c_char;
    (&mut *output).size = result.len() as u64;
  }
}

#[allow(non_snake_case)]
pub extern "C" fn c_node_text_has_text_data(raw_node: *mut ThalamusNode) -> c_char {
  let c_node = unsafe { &*(raw_node as *const ThalamusNode) };
  let text = deref_plugin_impl(c_node).data.and_then(|data| data.text());
  if text.is_some() { 1 } else { 0 }
}

fn wrap_analog(c_node: &mut ThalamusNode) {
  c_node.analog = Box::into_raw(Box::new(ThalamusAnalogNode::default()));
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
  c_node.image = Box::into_raw(Box::new(ThalamusImageNode::default()));
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
  c_node.mocap = Box::into_raw(Box::new(ThalamusMocapNode::default()));
  unsafe {
    (*c_node.mocap).segments = Some(c_node_mocap_segments);
    (*c_node.mocap).pose_name = Some(c_node_mocap_pose_name);
    (*c_node.mocap).has_motion_data = Some(c_node_mocap_has_motion_data);
  }
}

fn wrap_text(c_node: &mut ThalamusNode) {
  c_node.text = Box::into_raw(Box::new(ThalamusTextNode::default()));
  unsafe {
    (*c_node.text).text = Some(c_node_text_text);
    (*c_node.text).has_text_data = Some(c_node_text_has_text_data);
  }
}

extern "C" fn create_node_template<T: crate::api::Node + crate::api::NodeConsts + 'static>(
  factory: *mut ThalamusNodeFactory,
  state: *mut ThalamusState,
  io_context: *mut ThalamusIoContext,
  graph: *mut ThalamusNodeGraph,
) -> *mut ThalamusNode {
  create2_node_template::<T>(factory, state, io_context, graph, ptr::null_mut())
}

extern "C" fn create2_node_template<T: crate::api::Node + crate::api::NodeConsts + 'static>(
  factory: *mut ThalamusNodeFactory,
  state: *mut ThalamusState,
  _io_context: *mut ThalamusIoContext,
  _graph: *mut ThalamusNodeGraph,
  c_impl: *mut ::std::os::raw::c_void,
) -> *mut ThalamusNode {
  println!("create_node_template");
  let api_raw = unsafe { (*factory).plugin_impl as *mut ThalamusAPIRaw };
  let c_node = Box::into_raw(Box::new(ThalamusNode {
    impl_: c_impl,
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
  let c_node_ref = unsafe { &mut *c_node };
  let api = ThalamusAPI { raw: api_raw };
  let node_token = crate::api::NodeToken::new(c_node);

  let token = unsafe { crate::api::MainThreadToken::new_in_main_thread_callback() };
  let ctor = T::new(
    api,
    node_token.clone(),
    crate::api::State::new(api, state),
    token,
  );
  let modalities = T::MODALITIES;
  c_node_ref.signals_offmain = if T::SIGNALS_OFFMAIN { 1 } else { 0 };

  let result = Box::new(PluginImpl {
    api,
    node_token,
    node: ctor.into_node_handle(),
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

unsafe extern "C" fn destroy_node_template(
  _factory: *mut ThalamusNodeFactory,
  node_raw: *mut ThalamusNode,
) {
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

extern "C" fn prepare_node_template<T: crate::api::Node>(
  _factory: *mut ThalamusNodeFactory,
) -> ::std::os::raw::c_char {
  if T::prepare() { 1 } else { 0 }
}

extern "C" fn cleanup_node_template<T: crate::api::Node>(_factory: *mut ThalamusNodeFactory) {
  T::cleanup();
}

impl ThalamusNodeFactory {
  pub fn new<T: crate::api::Node + crate::api::NodeConsts + 'static>(
    name: &'static str,
    api: *mut ThalamusAPIRaw,
  ) -> *mut ThalamusNodeFactory {
    println!("ThalamusNodeFactory::new {}", name);
    let result = Box::into_raw(Box::new(ThalamusNodeFactory {
      type_: ThalamusCharSpan {
        data: name.as_ptr() as *const c_char,
        size: name.len() as u64,
        owns_data: 0,
      },
      create: Some(create_node_template::<T>),
      destroy: Some(destroy_node_template),
      prepare: Some(prepare_node_template::<T>),
      cleanup: Some(cleanup_node_template::<T>),
      plugin_impl: api as *mut ::std::os::raw::c_void,
      create2: Some(create2_node_template::<T>),
    }));
    result as *mut ThalamusNodeFactory
  }
}

#[unsafe(no_mangle)]
pub extern "C" fn thalamus_get_node_factory_version() -> i32 {
  println!("thalamus_get_node_factory_version");
  return 1;
}
