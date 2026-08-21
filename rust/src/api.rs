
use core::slice;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::pin::Pin;
use std::ptr::null;
use std::rc::Rc;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::{os::raw::c_void, sync::OnceLock};
use std::time::Duration;

use futures::future::FusedFuture;
use ash::vk::Handle;

pub use crate::ffi::{
  *
};
use crate::wakers::{self, RcWake};

/// Zero-sized token proving the current call is on the main (io_context) thread.
/// !Send so it cannot cross thread boundaries. Copy so it can be passed by value
/// without lifetime annotations.
#[derive(Copy, Clone)]
pub struct MainThreadToken {
  _not_send: PhantomData<*const ()>,
}

impl MainThreadToken {
  /// For use in C entry points that are always invoked on the main thread
  /// (e.g. node factory callbacks, post_callback). Not pub so callers outside
  /// this crate cannot manufacture a token.
  pub(crate) unsafe fn new_in_main_thread_callback() -> Self {
    MAIN_THREAD_ID.get_or_init(|| std::thread::current().id());
    MainThreadToken { _not_send: PhantomData }
  }
}

/// Thread ID of the main (io_context) thread, learned the first time a
/// MainThreadToken is manufactured, which only ever happens on that thread.
static MAIN_THREAD_ID: OnceLock<std::thread::ThreadId> = OnceLock::new();

/// True if the calling thread is the main (io_context) thread.
pub fn is_main_thread() -> bool {
  MAIN_THREAD_ID.get() == Some(&std::thread::current().id())
}

/// Wraps a !Send value so it can travel inside a Send closure, while ensuring
/// it can only be accessed with a MainThreadToken (i.e., on the main thread).
pub struct MainThreadOnly<T> {
  value: T,
}

unsafe impl<T> Send for MainThreadOnly<T> {}

impl<T> MainThreadOnly<T> {
  pub fn new(value: T, _token: MainThreadToken) -> Self { Self { value } }
  pub fn get(&self, _token: MainThreadToken) -> &T { &self.value }
  pub fn take(self, _token: MainThreadToken) -> T { self.value }
}

struct PostArgs<T> {
  call: T
}

struct GetNodeArgs<T> {
  api: ThalamusAPI,
  callback: T
}

struct NodeReadyArgs<T> {
  api: ThalamusAPI,
  callback: T
}

unsafe extern "C" fn post_callback<T: FnOnce(MainThreadToken)>(data: *mut ::std::os::raw::c_void) {
  let args = unsafe {
    let raw_args = data as *mut PostArgs<T>;
    Box::from_raw(raw_args)
  };
  let token = unsafe { MainThreadToken::new_in_main_thread_callback() };
  let PostArgs { call } = *args;
  call(token);
}

unsafe extern "C" fn threadpool_callback<T: FnOnce()>(data: *mut ::std::os::raw::c_void) {
  let args = unsafe {
    let raw_args = data as *mut PostArgs<T>;
    Box::from_raw(raw_args)
  };
  let PostArgs { call } = *args;
  call();
}

pub struct ExtNode {
  api: ThalamusAPI,
  node: *mut ThalamusNode
}

impl ExtNode {
  fn new(api: ThalamusAPI, node: *mut ThalamusNode) -> ExtNode {
    unsafe {
      ((*api.raw).node_inc_ref.unwrap())(node)
    };
    ExtNode {
      api, node
    }
  }
  pub fn subscribe<T: FnMut(ExtNode) + 'static>(&self, callback: T) -> OnDrop {
    let call_ptr = Box::into_raw(Box::new(NodeReadyArgs {
      api: self.api,
      callback
    }));
    let void_ptr = call_ptr as *mut std::os::raw::c_void;

    let connection = unsafe {
      ((&*self.api.raw).node_ready_connect.unwrap())(self.node, Some(node_ready_callback::<T>), void_ptr)
    };
    let api = self.api;
    let cleanup = move || {
      unsafe {
        ((&*api.raw).node_ready_disconnect.unwrap())(connection);
        drop(Box::from_raw(call_ptr));
      }
    };
    OnDrop { action: Box::new(cleanup) }
  }

  pub fn subscribe_multithreaded<T: FnMut(ExtNode) + Send + 'static>(&self, callback: T) -> OnDrop {
    let call_ptr = Box::into_raw(Box::new(NodeReadyArgs {
      api: self.api,
      callback
    }));
    let void_ptr = call_ptr as *mut std::os::raw::c_void;

    let connection = unsafe {
      ((&*self.api.raw).node_ready_multithreaded_connect.unwrap())(self.node, Some(node_ready_callback::<T>), void_ptr)
    };
    let api = self.api;
    let cleanup = move || {
      unsafe {
        ((&*api.raw).node_ready_disconnect.unwrap())(connection);
        drop(Box::from_raw(call_ptr));
      }
    };
    OnDrop { action: Box::new(cleanup) }
  }

  pub fn analog<'a>(&'a self) -> Option<ExtAnalogNode<'a>> {
    let analog = unsafe {
      (*self.node).analog
    };
    if analog.is_null() {
      None
    } else {
      Some(ExtAnalogNode { node: self })
    }
  }

  pub fn data(&self) -> ExtNodeData<'_> {
    ExtNodeData { node: self }
  }

  pub fn state(&self) -> State {
    let raw = unsafe {
      ((*self.api.raw).node_get_state.unwrap())(self.node)
    };
    // node_get_state already increments the ref count, so construct State directly
    State { api: self.api, state: raw }
  }
}

impl Drop for ExtNode {
  fn drop(&mut self) {
    unsafe {
      ((*self.api.raw).node_dec_ref.unwrap())(self.node)
    }
  }
}

pub struct ExtAnalogNode<'a> {
  node: &'a ExtNode,
}

impl<'a> ExtAnalogNode<'a> {
  pub fn subscribe_analog_channels_changed<T: FnMut(ExtNode) + 'static>(&self, callback: T) -> OnDrop {
    let call_ptr = Box::into_raw(Box::new(NodeReadyArgs {
      api: self.node.api,
      callback
    }));
    let void_ptr = call_ptr as *mut std::os::raw::c_void;

    let connection = unsafe {
      ((&*self.node.api.raw).node_channels_changed_connect.unwrap())(self.node.node, Some(node_ready_callback::<T>), void_ptr)
    };
    let api = self.node.api;
    let cleanup = move || {
      unsafe {
        ((&*api.raw).node_channels_changed_disconnect.unwrap())(connection);
        drop(Box::from_raw(call_ptr));
      }
    };
    OnDrop { action: Box::new(cleanup) }
  }
}

pub struct ExtNodeData<'a> {
  node: &'a ExtNode,
}

impl<'a> NodeData for ExtNodeData<'a> {
  fn time(&self) -> Duration {
    let ns = unsafe {
      ((*self.node.node).time_ns).unwrap()(self.node.node)
    };
    Duration::from_nanos(ns)
  }

  fn analog(&self) -> Option<&dyn AnalogData> {
    let analog = unsafe { (*self.node.node).analog };
    if analog.is_null() { None } else { Some(self) }
  }

  fn image(&self) -> Option<&dyn ImageData> {
    let image = unsafe { (*self.node.node).image };
    if image.is_null() { None } else { Some(self) }
  }

  fn mocap(&self) -> Option<&dyn MocapData> {
    let mocap = unsafe { (*self.node.node).mocap };
    if mocap.is_null() { None } else { Some(self) }
  }
}

impl<'a> AnalogData for ExtNodeData<'a> {
  fn data(&self, channel: i32) -> &[f64] {
    let mut span = ThalamusDoubleSpan { data : null(), size: 0 };
    unsafe {
      let analog = (*self.node.node).analog;
      let data = (*analog).data.unwrap();
      data(&mut span as *mut ThalamusDoubleSpan, self.node.node, channel);
      std::slice::from_raw_parts(span.data, span.size as usize)
    }
  }

  fn short_data(&self, channel: i32) -> &[i16] {
    let mut span = ThalamusShortSpan { data : null(), size: 0 };
    unsafe {
      let analog = (*self.node.node).analog;
      let data = (*analog).short_data.unwrap();
      data(&mut span as *mut ThalamusShortSpan, self.node.node, channel);
      std::slice::from_raw_parts(span.data, span.size as usize)
    }
  }

  fn int_data(&self, channel: i32) -> &[i32] {
    let mut span = ThalamusIntSpan { data : null(), size: 0 };
    unsafe {
      let analog = (*self.node.node).analog;
      let data = (*analog).int_data.unwrap();
      data(&mut span as *mut ThalamusIntSpan, self.node.node, channel);
      std::slice::from_raw_parts(span.data, span.size as usize)
    }
  }

  fn ulong_data(&self, channel: i32) -> &[u64] {
    let mut span = ThalamusULongSpan { data : null(), size: 0 };
    unsafe {
      let analog = (*self.node.node).analog;
      let data = (*analog).ulong_data.unwrap();
      data(&mut span as *mut ThalamusULongSpan, self.node.node, channel);
      std::slice::from_raw_parts(span.data, span.size as usize)
    }
  }

  fn num_channels(&self) -> i32 {
    unsafe {
      let analog = (*self.node.node).analog;
      let num_channels = (*analog).num_channels.unwrap();
      num_channels(self.node.node)
    }
  }
  fn sample_interval(&self, channel: i32) -> Duration {
    unsafe {
      let analog = (*self.node.node).analog;
      let sample_interval_ns = (*analog).sample_interval_ns.unwrap();
      Duration::from_nanos(sample_interval_ns(self.node.node, channel))
    }
  }
  fn name(&self, channel: i32) -> &str {
    let mut span = ThalamusByteSpan { data : null(), size: 0 };
    unsafe {
      let analog = (*self.node.node).analog;
      let name_func = (*analog).name.unwrap();
      name_func(&mut span as *mut ThalamusByteSpan, self.node.node, channel);
      let slice = std::slice::from_raw_parts(span.data, span.size as usize);
      std::str::from_utf8(slice).unwrap()
    }
  }
  fn is_short_data(&self) -> bool {
    unsafe {
      let analog = (*self.node.node).analog;
      ((*analog).is_short_data.unwrap())(self.node.node) != 0
    }
  }
  fn is_int_data(&self) -> bool {
    unsafe {
      let analog = (*self.node.node).analog;
      ((*analog).is_int_data.unwrap())(self.node.node) != 0
    }
  }
  fn is_ulong_data(&self) -> bool {
    unsafe {
      let analog = (*self.node.node).analog;
      ((*analog).is_ulong_data.unwrap())(self.node.node) != 0
    }
  }
  fn is_transformed(&self) -> bool {
    unsafe {
      let analog = (*self.node.node).analog;
      ((*analog).is_transformed.unwrap())(self.node.node) != 0
    }
  }
  fn scale(&self, channel: i32) -> f64 {
    unsafe {
      let analog = (*self.node.node).analog;
      ((*analog).scale.unwrap())(self.node.node, channel)
    }
  }
  fn offset(&self, channel: i32) -> f64 {
    unsafe {
      let analog = (*self.node.node).analog;
      ((*analog).offset.unwrap())(self.node.node, channel)
    }
  }
}

impl<'a> ImageData for ExtNodeData<'a> {
  fn plane(&self, channel: i32) -> &[u8] {
    let mut span = ThalamusByteSpan { data : null(), size: 0 };
    unsafe {
      let image = (*self.node.node).image;
      let plane = (*image).plane.unwrap();
      plane(&mut span as *mut ThalamusByteSpan, self.node.node, channel);
      std::slice::from_raw_parts(span.data, span.size as usize)
    }
  }

  fn num_planes(&self) -> u64 {
    unsafe {
      let image = (*self.node.node).image;
      let num_planes = (*image).num_planes.unwrap();
      num_planes(self.node.node)
    }
  }

  fn format(&self) -> ImageFormat {
    unsafe {
      let image = (*self.node.node).image;
      let format = (*image).format.unwrap();
      match format(self.node.node) {
        ThalamusImageFormat::Gray => ImageFormat::Gray,
        ThalamusImageFormat::RGB => ImageFormat::RGB,
        ThalamusImageFormat::YUYV422 => ImageFormat::YUYV422,
        ThalamusImageFormat::YUV420P => ImageFormat::YUV420P,
        ThalamusImageFormat::YUVJ420P => ImageFormat::YUVJ420P,
      }
    }
  }

  fn width(&self) -> u64 {
    unsafe {
      let image = (*self.node.node).image;
      let width = (*image).width.unwrap();
      width(self.node.node)
    }
  }

  fn height(&self) -> u64 {
    unsafe {
      let image = (*self.node.node).image;
      let height = (*image).height.unwrap();
      height(self.node.node)
    }
  }

  fn frame_interval(&self) -> Duration {
    unsafe {
      let image = (*self.node.node).image;
      let frame_interval_ns = (*image).frame_interval_ns.unwrap();
      Duration::from_nanos(frame_interval_ns(self.node.node))
    }
  }
}

impl<'a> MocapData for ExtNodeData<'a> {
  fn segments(&self) -> &[ThalamusMocapSegment] {
    let mut span = ThalamusMocapSegmentSpan { data : null(), size: 0 };
    unsafe {
      let mocap = (*self.node.node).mocap;
      let segments = (*mocap).segments.unwrap();
      segments(&mut span as *mut ThalamusMocapSegmentSpan, self.node.node);
      std::slice::from_raw_parts(span.data, span.size as usize)
    }
  }

  fn pose_name(&self) -> &str {
    let mut span = ThalamusCharSpan { data : null(), size: 0, owns_data: 0 };
    unsafe {
      let mocap = (*self.node.node).mocap;
      let pose_name = (*mocap).pose_name.unwrap();
      pose_name(&mut span as *mut ThalamusCharSpan, self.node.node);
      let slice = std::slice::from_raw_parts(span.data as *const u8, span.size as usize);
      std::str::from_utf8(slice).unwrap()
    }
  }
}

unsafe extern "C" fn node_ready_callback<T: FnMut(ExtNode)>(node: *mut ThalamusNode, data: *mut ::std::os::raw::c_void) {
  let args = unsafe {
    &mut*(data as *mut NodeReadyArgs<T>)
  };

  let ext_node = ExtNode::new(args.api, node);
  (args.callback)(ext_node);
}

unsafe extern "C" fn get_node_callback<T: FnMut(ExtNode)>(node: *mut ThalamusNode, data: *mut ::std::os::raw::c_void) {
  let args = unsafe {
    &mut*(data as *mut GetNodeArgs<T>)
  };

  let ext_node = ExtNode::new(args.api, node);
  (args.callback)(ext_node);
}

/// Error returned when a ThalamusAPI/ThalamusAPIThreadSafe call is made
/// after the C++ side has already destroyed the underlying node (e.g. from a
/// callback queued via post_to_main/post_to_threadpool that outlived the node).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeDestroyed;

impl std::fmt::Display for NodeDestroyed {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "node has already been destroyed")
  }
}

impl std::error::Error for NodeDestroyed {}

/// Shared handle to a node's C++ pointer. destroy_node_template nulls this out
/// when the C++ side destroys the node, so any clone of the token still held by
/// a callback queued via post_to_main/post_to_threadpool (and thus possibly
/// running after the node is gone) can detect that and bail out instead of
/// dereferencing freed memory.
#[derive(Debug, Clone)]
pub struct NodeToken {
  node: Arc<Mutex<*mut ThalamusNode>>
}
unsafe impl Send for NodeToken {}

impl NodeToken {
  pub fn new(node: *mut ThalamusNode) -> NodeToken {
    NodeToken { node: Arc::new(Mutex::new(node)) }
  }

  /// Called by destroy_node_template once the node is gone.
  pub(crate) fn destroy(&self) {
    *self.node.lock().unwrap() = std::ptr::null_mut();
  }

  fn with<R>(&self, f: impl FnOnce(*mut ThalamusNode) -> R) -> Result<R, NodeDestroyed> {
    let guard = self.node.lock().unwrap();
    if guard.is_null() {
      return Err(NodeDestroyed);
    }
    Ok(f(*guard))
  }
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub struct ThalamusAPI {
  pub raw: *mut ThalamusAPIRaw,
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub struct ThalamusAPIThreadSafe {
  pub raw: *mut ThalamusAPIRaw,
}
unsafe impl Send for ThalamusAPIThreadSafe {}

pub enum NodeSelector {
  Name(String),
  Type(String)
}

impl ThalamusAPIThreadSafe {
  pub fn thread_unsafe(&self, _token: MainThreadToken) -> ThalamusAPI {
    ThalamusAPI{raw: self.raw}
  }

  pub fn time(&self) -> Duration {
    unsafe {
      let time_ns = (&*self.raw).time_ns.unwrap();
      return Duration::from_nanos(time_ns());
    }
  }

  pub fn ready_offmain(&self, data: &dyn NodeData, token: &NodeToken) -> Result<(), NodeDestroyed> {
    token.with(|node| unsafe {
      let node_ready_offmain = (&*self.raw).node_ready_offmain.unwrap();
      let plugin_impl = plugin_impl_ptr(node);
      // SAFETY: data is only read back synchronously inside node_ready_offmain,
      // which returns before this function does, so the erased lifetime never
      // outlives the real borrow.
      let data: &'static dyn NodeData = std::mem::transmute(data);

      (*plugin_impl).data = Some(data);
      node_ready_offmain(node);
      (*plugin_impl).data = None;
    })
  }

  /// Calls `ready` if invoked on the main thread, otherwise `ready_offmain`.
  pub fn ready_this_thread(&self, data: &dyn NodeData, token: &NodeToken) -> Result<(), NodeDestroyed> {
    if is_main_thread() {
      let main_token = unsafe { MainThreadToken::new_in_main_thread_callback() };
      self.thread_unsafe(main_token).ready(data, token)
    } else {
      self.ready_offmain(data, token)
    }
  }

  pub fn post_to_main<T: FnOnce(MainThreadToken) + Send + 'static>(&self, call: T) {
    unsafe {
      let api = &*self.raw;
      let call_ptr = Box::into_raw(Box::new(PostArgs {
        call
      }));
      let void_ptr = call_ptr as *mut std::os::raw::c_void;
      (api.io_context_post.unwrap())(Some(post_callback::<T>), void_ptr);

      //let call_ref =  &*call_ptr;
      //let mut done = call_ref.mutex.lock().unwrap();
      //while !*done {
      //  done = call_ref.cond.wait(done).unwrap();
      //}
      //
      //drop(Box::from_raw(call_ptr));
    }
  }

  pub fn post_to_threadpool<T: FnOnce() + Send + 'static>(&self, call: T) {
    unsafe {
      let api = &*self.raw;
      let call_ptr = Box::into_raw(Box::new(PostArgs {
        call
      }));
      let void_ptr = call_ptr as *mut std::os::raw::c_void;
      (api.threadpool_post.unwrap())(Some(threadpool_callback::<T>), void_ptr);

      //let call_ref =  &*call_ptr;
      //let mut done = call_ref.mutex.lock().unwrap();
      //while !*done {
      //  done = call_ref.cond.wait(done).unwrap();
      //}
      //
      //drop(Box::from_raw(call_ptr));
    }
  }
}

impl ThalamusAPI {
  pub fn thread_safe(&self) -> ThalamusAPIThreadSafe {
    ThalamusAPIThreadSafe{raw: self.raw}
  }

  pub fn tokio(&self) -> std::sync::MutexGuard<'static, Option<tokio::runtime::Runtime>> {
    tokio_runtime()
  }

  pub fn time(&self) -> Duration {
    unsafe {
      let time_ns = (&*self.raw).time_ns.unwrap();
      return Duration::from_nanos(time_ns());
    }
  }

  pub fn ready(&self, data: &dyn NodeData, token: &NodeToken) -> Result<(), NodeDestroyed> {
    token.with(|node| unsafe {
      let node_ready = (&*self.raw).node_ready.unwrap();
      let plugin_impl = plugin_impl_ptr(node);
      // SAFETY: data is only read back synchronously inside node_ready, which
      // returns before this function does, so the erased lifetime never
      // outlives the real borrow.
      let data: &'static dyn NodeData = std::mem::transmute(data);

      (*plugin_impl).data = Some(data);
      node_ready(node);
      (*plugin_impl).data = None;
    })
  }

  pub fn channels_changed(&self, token: &NodeToken) -> Result<(), NodeDestroyed> {
    token.with(|node| unsafe {
      let node_channels_changed = (&*self.raw).node_channels_changed.unwrap();
      node_channels_changed(node);
    })
  }

  pub fn post_to_main<T: FnOnce(MainThreadToken) + Send + 'static>(&self, call: T) {
    unsafe {
      let api = &*self.raw;
      let call_ptr = Box::into_raw(Box::new(PostArgs {
        call
      }));
      let void_ptr = call_ptr as *mut std::os::raw::c_void;
      (api.io_context_post.unwrap())(Some(post_callback::<T>), void_ptr);

      //let call_ref =  &*call_ptr;
      //let mut done = call_ref.mutex.lock().unwrap();
      //while !*done {
      //  done = call_ref.cond.wait(done).unwrap();
      //}
      //
      //drop(Box::from_raw(call_ptr));
    }
  }

  pub fn create_serial_port(&self) -> SerialPort {
    unsafe {
      let api = &*self.raw;
      let port = (api.serial_port_create.unwrap())();
      SerialPort {
        api: *self, port
      }
    }
  }

  pub fn create_streambuf(&self) -> StreamBuf {
    unsafe {
      let api = &*self.raw;
      let buffer = (api.streambuf_create.unwrap())();
      StreamBuf {
        api: *self, buffer
      }
    }
  }


  pub fn create_timer(&self) -> Timer {
    unsafe {
      let timer = ((&(*self.raw)).timer_create.unwrap())();
      println!("new timer {:?} {:?}", self, timer);
      Timer {
        api: *self, timer
      }
    }
  }

  pub fn get_node<T: FnMut(ExtNode) + 'static>(&self, selector: NodeSelector, callback: T) -> OnDrop {
    let mut c_selector = ThalamusNodeSelector {
      name: ThalamusCharSpan { data: null(), size: 0, owns_data: 0 },
      _type: ThalamusCharSpan { data: null(), size: 0, owns_data: 0 }
    };

    match selector {
      NodeSelector::Name(val) => {
        c_selector.name.data = val.as_ptr() as *const i8;
        c_selector.name.size = val.len() as u64;
      },
      NodeSelector::Type(val) => {
        c_selector._type.data = val.as_ptr() as *const i8;
        c_selector._type.size = val.len() as u64;
      }
    };

    let call_ptr = Box::into_raw(Box::new(GetNodeArgs {
        api: *self,
        callback
    }));
    let void_ptr = call_ptr as *mut std::os::raw::c_void;

    let connection = unsafe {
      ((&*self.raw).node_get_node.unwrap())(&mut c_selector as *mut ThalamusNodeSelector, Some(get_node_callback::<T>), void_ptr)
    };

    let api = self.raw;
    let cleanup = move || {
      unsafe {
        ((&*api).node_get_node_disconnect.unwrap())(connection);
        drop(Box::from_raw(call_ptr));
      };
    };
    OnDrop { action: Box::new(cleanup) }
  }

  pub fn post_to_threadpool<T: FnOnce() + Send + 'static>(&self, call: T) {
    unsafe {
      let api = &*self.raw;
      let call_ptr = Box::into_raw(Box::new(PostArgs {
        call
      }));
      let void_ptr = call_ptr as *mut std::os::raw::c_void;
      (api.threadpool_post.unwrap())(Some(threadpool_callback::<T>), void_ptr);

      //let call_ref =  &*call_ptr;
      //let mut done = call_ref.mutex.lock().unwrap();
      //while !*done {
      //  done = call_ref.cond.wait(done).unwrap();
      //}
      //
      //drop(Box::from_raw(call_ptr));
    }
  }

  /// If `handle` is `Some` and not yet finished, joins it on a threadpool
  /// thread (so the caller is never blocked waiting for it) and hops back to
  /// the main thread to invoke `callback`. If `handle` is `None` or already
  /// finished, the threadpool hop is skipped and `callback` runs immediately.
  /// Useful when a background thread's completion handler needs to touch
  /// non-Send, main-thread-only state (e.g. an `Rc<RefCell<..>>`).
  ///
  /// `token` proves this call itself is happening on the main thread; it's
  /// what makes it sound to smuggle the non-Send `callback` through the
  /// Send-bound post_to_* calls below.
  ///
  /// Returns a flag that starts out `true` and is set to `false` once
  /// `callback` has finished running, so callers can poll completion.
  pub fn join_then<T: Send + 'static, F: FnOnce() + 'static>(
    &self,
    handle: Option<std::thread::JoinHandle<T>>,
    token: MainThreadToken,
    callback: F,
  ) -> Rc<RefCell<bool>> {
    let running = Rc::new(RefCell::new(true));

    match handle {
      Some(h) if !h.is_finished() => {
        let mt_api = self.thread_safe();
        let wrapped = MainThreadOnly::new((callback, Rc::clone(&running)), token);
        mt_api.post_to_threadpool(move || {
          let _ = h.join();
          mt_api.post_to_main(move |token| {
            let (callback, running) = wrapped.take(token);
            callback();
            *running.borrow_mut() = false;
          });
        });
      }
      Some(h) => {
        // Already finished: no need to bounce through the threadpool.
        let _ = h.join();
        callback();
        *running.borrow_mut() = false;
      }
      None => {
        callback();
        *running.borrow_mut() = false;
      }
    }

    running
  }

  /// Raw handle to the VkInstance Thalamus already created. Thalamus owns it —
  /// never destroy it from a plugin.
  pub fn vulkan_instance_handle(&self) -> ash::vk::Instance {
    unsafe {
      let raw = ((&*self.raw).get_vulkan_instance.unwrap())();
      ash::vk::Instance::from_raw(raw as u64)
    }
  }

  pub fn vulkan_physical_device(&self) -> ash::vk::PhysicalDevice {
    unsafe {
      let raw = ((&*self.raw).get_vulkan_physical_device.unwrap())();
      ash::vk::PhysicalDevice::from_raw(raw as u64)
    }
  }

  /// Raw handle to the VkDevice Thalamus already created. Thalamus owns it —
  /// never destroy it from a plugin.
  pub fn vulkan_device_handle(&self) -> ash::vk::Device {
    unsafe {
      let raw = ((&*self.raw).get_vulkan_device.unwrap())();
      ash::vk::Device::from_raw(raw as u64)
    }
  }

  /// Loads instance-level Vulkan entry points for the VkInstance Thalamus
  /// already created, so a plugin can call into it through `ash` without
  /// creating its own instance. `entry` only needs to resolve
  /// `vkGetInstanceProcAddr` (e.g. `unsafe { ash::Entry::load() }`).
  ///
  /// # Safety
  /// The returned `ash::Instance` must not outlive the Thalamus process, and
  /// callers must never call `destroy_instance` on it -- Thalamus owns the
  /// underlying VkInstance.
  pub unsafe fn load_vulkan_instance(&self, entry: &ash::Entry) -> ash::Instance {
    unsafe { ash::Instance::load(entry.static_fn(), self.vulkan_instance_handle()) }
  }

  /// Loads device-level Vulkan entry points for the VkDevice Thalamus already
  /// created.
  ///
  /// # Safety
  /// The returned `ash::Device` must not outlive the Thalamus process, and
  /// callers must never call `destroy_device` on it -- Thalamus owns the
  /// underlying VkDevice.
  pub unsafe fn load_vulkan_device(&self, instance: &ash::Instance) -> ash::Device {
    unsafe { ash::Device::load(instance.fp_v1_0(), self.vulkan_device_handle()) }
  }

  /// Creates a new VkCommandPool on Thalamus's Vulkan device. Ownership
  /// transfers to the caller, which is responsible for destroying it (e.g.
  /// via `device.destroy_command_pool(pool, None)`) before the device goes
  /// away.
  pub fn create_vulkan_command_pool(&self) -> ash::vk::CommandPool {
    unsafe {
      let raw = ((&*self.raw).create_vulkan_command_pool.unwrap())();
      ash::vk::CommandPool::from_raw(raw as u64)
    }
  }

  /// Locks Thalamus's shared VkQueue for the duration of the returned guard.
  /// Thalamus serializes all access to this queue -- it uses the same queue
  /// for its own rendering -- so any `vkQueueSubmit`/`vkQueuePresentKHR`/etc.
  /// must happen while holding this lock.
  pub fn lock_vulkan_queue(&self) -> VulkanQueueGuard {
    unsafe {
      let raw = &*self.raw;
      let lock = (raw.lock_vulkan_queue.unwrap())();
      let queue = ash::vk::Queue::from_raw((raw.get_vulkan_queue.unwrap())() as u64);
      VulkanQueueGuard { api: *self, lock, queue }
    }
  }
}

/// RAII guard holding Thalamus's shared VkQueue lock. Access `queue()` only
/// while this guard is alive; dropping it releases the lock via
/// `unlock_vulkan_queue`.
pub struct VulkanQueueGuard {
  api: ThalamusAPI,
  lock: *mut ThalamusVkQueueLock,
  queue: ash::vk::Queue,
}
unsafe impl Send for VulkanQueueGuard {}

impl VulkanQueueGuard {
  pub fn queue(&self) -> ash::vk::Queue {
    self.queue
  }
}

impl Drop for VulkanQueueGuard {
  fn drop(&mut self) {
    unsafe {
      ((&*self.api.raw).unlock_vulkan_queue.unwrap())(self.lock);
    }
  }
}

pub struct Json {
  api: ThalamusAPI,
  handle:*mut ThalamusJson,
}

impl Json {
  pub fn new(api: ThalamusAPI, handle:*mut ThalamusJson) -> Json {
    unsafe {
      ((&*api.raw).json_inc_ref.unwrap())(handle);
    }
    Json{api, handle}
  }

  pub fn to_string(&self) -> String {
    unsafe {
      let mut span = ThalamusCharSpan { data: null(), size: 0, owns_data: 0};
      ((&*self.api.raw).json_to_string.unwrap())(&mut span as *mut ThalamusCharSpan, self.handle);

      let slice = slice::from_raw_parts(span.data as *mut u8, span.size as usize);
      let text = str::from_utf8(slice).unwrap();
      let result = text.to_string();

      ((&*self.api.raw).charspan_release.unwrap())(&mut span as *mut ThalamusCharSpan);

      result
    }
  }

  pub fn from_string(api: ThalamusAPI, text: &str) -> Json {
      let mut span = ThalamusCharSpan { data: text.as_ptr() as *const i8, size: text.len() as u64, owns_data: 0};


      let handle = unsafe { ((&*api.raw).json_from_string.unwrap())(&mut span as *mut ThalamusCharSpan) };
      Json {
        api, handle
      }
  }
}

impl Clone for Json {
  fn clone(&self) -> Self {
      return Json::new(self.api, self.handle)
  }
}

impl Drop for Json {
  fn drop(&mut self) {
    unsafe {
      ((&*self.api.raw).json_dec_ref.unwrap())(self.handle);
    }
  }
}

pub struct Request {
  pub api: ThalamusAPI,
  pub handle:*mut ThalamusRequestHandle,
}

impl Request {
  pub fn respond(self, response: &Json) {
    unsafe {
      ((&*self.api.raw).request_respond.unwrap())(self.handle, response.handle)
    }
  }
}

struct SleeperState {
  wakes: i32,
  futures: VecDeque<Arc<Mutex<SleeperFutureState>>>
}

pub struct Sleeper {
  api: ThalamusAPI,
  state: Arc<Mutex<SleeperState>>
}

pub struct SleeperWaker {
  api: ThalamusAPI,
  state: Arc<Mutex<SleeperState>>
}

struct SleeperFutureState {
  state: Arc<Mutex<SleeperState>>,
  waker: Option<Waker>
}

pub struct SleeperFuture {
  state: Arc<Mutex<SleeperFutureState>>
}

impl Future for SleeperFuture {
  type Output = bool;

  fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
    let mut future_state = self.state.lock().unwrap();
    let mut state = future_state.state.lock().unwrap();
    if state.wakes > 0 {
      state.wakes -= 1;
      std::task::Poll::Ready(true)
    } else if state.wakes < 0 {
      std::task::Poll::Ready(false)
    } else {
      drop(state);
      match &mut future_state.waker {
        Some(waker) => {
          if !waker.will_wake(cx.waker()) {
            *waker = cx.waker().clone();
          }
        },
        None => {
          future_state.waker = Some(cx.waker().clone());
        }
      }
      std::task::Poll::Pending
    }
  }
}

impl SleeperWaker {
  pub fn wake_impl(&self, immediate: bool) {
    let state = self.state.clone();
    let closure = move |_token: MainThreadToken| {
      let future = {
        let mut lock = state.lock().unwrap();
        if lock.wakes >= 0 {
          lock.wakes += 1;
        }
        lock.futures.pop_front()
      };
      future.map(|f| {
        let waker = f.lock().unwrap().waker.take();
        waker.as_ref().map(|w| w.wake_by_ref());
      });
    };
    if immediate {
      let token = unsafe { MainThreadToken::new_in_main_thread_callback() };
      closure(token);
    } else {
      self.api.post_to_main(closure);
    }
  }

  pub fn wake(&self) {
    self.wake_impl(false);
  }
}

impl Sleeper {
  pub fn new(api: ThalamusAPI) -> Sleeper {
    Sleeper { api, state: Arc::new(Mutex::new( SleeperState { wakes: 0, futures: VecDeque::<Arc<Mutex<SleeperFutureState>>>::new() }))}
  }

  pub fn waker(&self) -> SleeperWaker {
    SleeperWaker { api: self.api, state: self.state.clone() }
  }

  pub fn wait(&self) -> SleeperFuture {
    let mut lock = self.state.lock().unwrap();

    let result = SleeperFuture { state: Arc::new(Mutex::new(SleeperFutureState {state: self.state.clone(), waker: None })) };
    lock.futures.push_back(result.state.clone());
    result
  }
}

impl Drop for Sleeper {
  fn drop(&mut self) {
    let mut state = self.state.lock().unwrap();
    state.wakes = -1;
    while !state.futures.is_empty() {
      let waker = self.waker();
      drop(state);
      waker.wake_impl(true);
      state = self.state.lock().unwrap();
    }
  }
}

struct IOArgs<T> {
  api: ThalamusAPI,
  callback: T
}

unsafe extern "C" fn io_callback<T: FnMut(ErrorCode, u64)>(error: *mut ThalamusErrorCode, length: u64, data: *mut ::std::os::raw::c_void) {
  let mut args = unsafe {
    let raw_args = &mut*(data as *mut IOArgs<T>);
    Box::from_raw(raw_args)
  };
  let error_code = ErrorCode::new(args.api, error);
  
  (args.callback)(error_code, length);
}

pub struct StreamBuf {
  api: ThalamusAPI,
  buffer: *mut ThalamusStreamBuf
}

impl Drop for StreamBuf {
  fn drop(&mut self) {
    unsafe {
      let api = &*self.api.raw;
      (api.streambuf_destroy.unwrap())(self.buffer);
    }
  }
}

impl StreamBuf {
  pub fn to_string(&self) -> String {
    unsafe {
      let api = &*self.api.raw;
      let mut span = ThalamusCharSpan { data: null(), size: 0, owns_data: 0};
      (api.streambuf_to_span.unwrap())(&mut span as *mut ThalamusCharSpan, self.buffer);
      let slice = slice::from_raw_parts(span.data as *mut u8, span.size as usize);
      let text = str::from_utf8(slice).unwrap();
      let result = text.to_string();
      (api.charspan_release.unwrap())(&mut span as *mut ThalamusCharSpan);
      result
    }
  }

  pub fn consume(&self, count: u64) {
    unsafe {
      let api = &*self.api.raw;
      (api.streambuf_consume.unwrap())(self.buffer, count);
    }
  }

  pub fn size(&self) -> u64 {
    unsafe {
      let api = &*self.api.raw;
      (api.streambuf_size.unwrap())(self.buffer)
    }
  }
}

pub struct SerialPort {
  api: ThalamusAPI,
  port: *mut ThalamusSerialPort
}

impl SerialPort {
  pub fn error(&self) -> Option<ErrorCode> {
    unsafe {
      let api = &*self.api.raw;
      let error = (api.serial_port_error.unwrap())(self.port);
      let error_code = ErrorCode::new(self.api, error);
      if error_code.code == 0 {
        None
      } else {
        Some(error_code)
      }
    }
  }
  pub fn open(&self, name: &str) -> Result<(), ErrorCode> {
    unsafe {
      let api = &*self.api.raw;
      let name_span = ThalamusCharSpan { data: name.as_ptr() as *const i8, size: name.len() as u64, owns_data: 0 };
      (api.serial_port_open.unwrap())(self.port, &name_span as *const ThalamusCharSpan);
      match self.error() {
        None => Ok(()),
        Some(error) => Err(error)
      }
    }
  }
  pub fn set_baud_rate(&self, rate: u32) -> Result<(), ErrorCode> {
    unsafe {
      let api = &*self.api.raw;
      (api.serial_set_baud_rate.unwrap())(self.port, rate);
      match self.error() {
        None => Ok(()),
        Some(error) => Err(error)
      }
    }
  }

  pub fn write_callback<T: FnMut(ErrorCode, u64)>(&self, data: &[u8], callback: T) {
    unsafe {
      let api = &*self.api.raw;
      let boxed = Box::new(IOArgs {
        api: self.api,
        callback
      });
      let args = Box::into_raw(boxed)  as *mut std::os::raw::c_void;
      let mut span = ThalamusByteSpan { data: data.as_ptr(), size: data.len() as u64 };
      (api.serial_port_write.unwrap())(self.port, &mut span as *mut ThalamusByteSpan, Some(io_callback::<T>), args);
    }
  }
  pub fn write(&self, data: &[u8]) -> SimpleFuture<u64, ErrorCode> {
    let future = SimpleFuture::new();
    let state = future.state.clone();
    self.write_callback(data, move |error, size| {
      resolve_simple_future(&state, size, error);
    });
    future
  }

  pub fn read_callback<T: FnMut(ErrorCode, u64)>(&self, data: &mut [u8], callback: T) {
    unsafe {
      let api = &*self.api.raw;
      let boxed = Box::new(IOArgs {
        api: self.api,
        callback
      });
      let args = Box::into_raw(boxed)  as *mut std::os::raw::c_void;
      let mut span = ThalamusMutableByteSpan { data: data.as_mut_ptr(), size: data.len() as u64 };
      (api.serial_port_read.unwrap())(self.port, &mut span as *mut ThalamusMutableByteSpan, Some(io_callback::<T>), args);
    }
  }
  pub fn read(&self, data: &mut [u8]) -> SimpleFuture<u64, ErrorCode> {
    let future = SimpleFuture::new();
    let state = future.state.clone();
    self.read_callback(data, move |error, size| {
      resolve_simple_future(&state, size, error);
    });
    future
  }

  pub fn read_some_callback<T: FnMut(ErrorCode, u64)>(&self, data: &mut [u8], callback: T) {
    unsafe {
      let api = &*self.api.raw;
      let boxed = Box::new(IOArgs {
        api: self.api,
        callback
      });
      let args = Box::into_raw(boxed)  as *mut std::os::raw::c_void;
      let mut span = ThalamusMutableByteSpan { data: data.as_mut_ptr(), size: data.len() as u64 };
      (api.serial_port_read_some.unwrap())(self.port, &mut span as *mut ThalamusMutableByteSpan, Some(io_callback::<T>), args);
    }
  }
  pub fn read_some(&self, data: &mut [u8]) -> SimpleFuture<u64, ErrorCode> {
    let future = SimpleFuture::new();
    let state = future.state.clone();
    self.read_some_callback(data, move |error, size| {
      resolve_simple_future(&state, size, error);
    });
    future
  }

  pub fn read_until_callback<T: FnMut(ErrorCode, u64)>(&self, buffer: &StreamBuf, delimiter: &str, callback: T) {
    unsafe {
      let api = &*self.api.raw;
      let boxed = Box::new(IOArgs {
        api: self.api,
        callback
      });
      let args = Box::into_raw(boxed)  as *mut std::os::raw::c_void;
      let delimiter_span = ThalamusCharSpan { data: delimiter.as_ptr() as *const i8, size: delimiter.len() as u64, owns_data: 0 };
      (api.serial_port_read_until.unwrap())(self.port, buffer.buffer, &delimiter_span as *const ThalamusCharSpan, Some(io_callback::<T>), args);
    }
  }
  pub fn read_until(&self, buffer: &StreamBuf, delimiter: &str) -> SimpleFuture<u64, ErrorCode> {
    println!("Read Until");
    let future = SimpleFuture::new();
    let state = future.state.clone();
    self.read_until_callback(buffer, delimiter, move |error, size| {
      resolve_simple_future(&state, size, error);
    });
    future
  }
}

impl Drop for SerialPort {
  fn drop(&mut self) {
    unsafe {
      let api = &*self.api.raw;
      (api.serial_port_destroy.unwrap())(self.port);
    }
  }
}

struct TaskState {
  future: Option<Pin<Box<dyn Future<Output = ()>>>>,
  poll: Poll<()>
}

impl TaskState {
  fn poll(&mut self, cx: &mut Context<'_>) {
    match self.future.as_mut() {
      None => {},
      Some(pin_future) => {
        let future = pin_future.as_mut();
        if self.poll.is_pending() {
          self.poll = future.poll(cx);
        }
      }
    }
  }
}

struct Task {
  state: Mutex<TaskState>
}

impl Task {
  fn poll(self: &Rc<Self>) {
    let mut state = self.state.lock().unwrap();
    if let None = state.future {
      return;
    }
    let waker = wakers::waker(self.clone());
    let mut cx = Context::from_waker(&waker);
    state.poll(&mut cx);
  }
}

impl RcWake for Task {
  fn wake_by_ref(arc_self: &Rc<Self>) {
    arc_self.poll();
  }
}

pub struct TaskScope {
  task: Rc<Task>
}

impl Drop for TaskScope {
  fn drop(&mut self) {
    println!("TaskScope.drop");
    let mut state = self.task.state.lock().unwrap();
    state.future = None;
  }
}

pub fn run_task<F: Future<Output = ()> + 'static>(future: F) -> TaskScope
{
  let task = Rc::new(Task {
    state: Mutex::new(TaskState {
      future: Some(Box::pin(future)), poll: Poll::Pending
    })
  });

  task.poll();
  TaskScope{ task: task.clone() }
}

pub fn time(api: *const ThalamusAPIRaw) -> Duration {
  unsafe {
    return Duration::from_nanos(((&*api).time_ns.unwrap())());
  }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum StateAction {
  Set,
  Delete,
}

#[derive(Debug)]
pub struct State {
  state: *mut ThalamusState,
  api: ThalamusAPI
}

impl Hash for State {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.state.hash(state);
    }
}

impl PartialEq for State {
  fn eq(&self, other: &Self) -> bool {
    self.state == other.state
  }
}
impl Eq for State {}

#[derive(Debug,PartialEq)]
pub enum StateValue {
  Bool(bool),
  Dict(State),
  Float(f64),
  Int(i64),
  List(State),
  String(String),
  Null
}

impl TryFrom<StateValue> for f64 {
  type Error = StateValue;

  fn try_from(value: StateValue) -> Result<Self, Self::Error> {
    match value {
      StateValue::Float(f) => Ok(f),
      StateValue::Int(i) => Ok(i as f64),
      other => Err(other),
    }
  }
}

impl TryFrom<StateValue> for i64 {
  type Error = StateValue;

  fn try_from(value: StateValue) -> Result<Self, Self::Error> {
    match value {
      StateValue::Int(i) => Ok(i),
      other => Err(other),
    }
  }
}

impl TryFrom<StateValue> for i32 {
  type Error = StateValue;

  fn try_from(value: StateValue) -> Result<Self, Self::Error> {
    match value {
      StateValue::Int(i) => Ok(i as i32),
      other => Err(other),
    }
  }
}

impl TryFrom<StateValue> for u64 {
  type Error = StateValue;

  fn try_from(value: StateValue) -> Result<Self, Self::Error> {
    match value {
      StateValue::Int(i) => Ok(i as u64),
      other => Err(other),
    }
  }
}

impl TryFrom<StateValue> for u32 {
  type Error = StateValue;

  fn try_from(value: StateValue) -> Result<Self, Self::Error> {
    match value {
      StateValue::Int(i) => Ok(i as u32),
      other => Err(other),
    }
  }
}

impl TryFrom<StateValue> for String {
  type Error = StateValue;

  fn try_from(value: StateValue) -> Result<Self, Self::Error> {
    match value {
      StateValue::String(s) => Ok(s),
      other => Err(other),
    }
  }
}

#[derive(Debug, PartialEq)]
pub enum StateKey {
  Int(i64),
  String(String)
}

fn wrap_state(api:ThalamusAPI, arg: *mut ThalamusState) -> StateValue {
  unsafe {
    let raw = &*api.raw;
    if (raw.state_is_bool.unwrap())(arg) != 0 {
      StateValue::Bool((raw.state_get_bool.unwrap())(arg) != 0)
    } else if (raw.state_is_dict.unwrap())(arg) != 0 {
      StateValue::Dict(State::new(api, arg))
    } else if (raw.state_is_float.unwrap())(arg) != 0 {
      StateValue::Float((raw.state_get_float.unwrap())(arg))
    } else if (raw.state_is_int.unwrap())(arg) != 0 {
      StateValue::Int((raw.state_get_int.unwrap())(arg))
    } else if (raw.state_is_list.unwrap())(arg) != 0 {
      StateValue::List(State::new(api, arg))
    } else if (raw.state_is_string.unwrap())(arg) != 0 {
      let mut span = ThalamusCharSpan { data: null(), size: 0, owns_data: 0 };
      ((&*raw).state_get_string.unwrap())(&mut span, arg);
      let text = std::str::from_utf8(std::slice::from_raw_parts(span.data as *const u8, span.size as usize)).unwrap();
      StateValue::String(text.to_string())
    } else {
      StateValue::Null
    }
  }
}

fn wrap_state_key(api: ThalamusAPI, arg: *mut ThalamusState) -> StateKey {
  match wrap_state(api, arg) {
    StateValue::Int(k) => StateKey::Int(k),
    StateValue::String(k) => StateKey::String(k),
    _ => panic!("Unexpected state key")
  }
}

unsafe extern "C" fn state_on_change<T: FnMut(State, StateAction, StateValue, StateValue)>(source_raw: *mut ThalamusState, action: i32, key_raw: *mut ThalamusState, value_raw: *mut ThalamusState, data: *mut ::std::os::raw::c_void) {
  let args = unsafe { &mut *(data as *mut StateConnectionCallbackArgs<T>) };
  let action = if action == 0 { StateAction::Set } else { StateAction::Delete };

  let key = wrap_state(args.api, key_raw);
  let value = wrap_state(args.api, value_raw);
  let source = State::new(args.api, source_raw);

  (args.callback)(source, action, key, value);
}

pub struct SimpleFutureState<T, E> {
  waker: Option<Waker>,
  result: Option<Result<T, E>>,
  done: bool,
  //state: Arc<Mutex<SleeperFutureState>>
}

pub struct SimpleFuture<T, E> {
  state: Rc<RefCell<SimpleFutureState<T, E>>>
}

impl<T, E> SimpleFutureState<T, E> {
  pub fn set(cell: &RefCell<Self>, result: Result<T, E>) {
    let waker = {
      let mut this = cell.borrow_mut();
      this.result = Some(result);
      this.done = true;
      this.waker.take()
    };
    waker.map(|w| w.wake_by_ref());
  }
}

fn resolve_simple_future<T>(cell: &RefCell<SimpleFutureState<T, ErrorCode>>, value: T, error: ErrorCode) {
  let result = if error.code != 0 { Err(error) } else { Ok(value) };
  SimpleFutureState::<T, ErrorCode>::set(cell, result);
}

impl<T, E> Future for SimpleFuture<T, E> {
  type Output = Result<T, E>;

  fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
    let mut state = self.state.borrow_mut();
    match state.result.take() {
      Some(result) => {
        std::task::Poll::Ready(result)
      },
      None => {
        match &mut state.waker {
          Some(waker) => {
            if !waker.will_wake(cx.waker()) {
              *waker = cx.waker().clone();
            }
          },
          None => {
            state.waker = Some(cx.waker().clone());
          }
        }
        std::task::Poll::Pending
      }
    }
  }
}

impl<T, E> FusedFuture for SimpleFuture<T, E> {
  fn is_terminated(&self) -> bool {
    self.state.borrow().done
  }
}

impl<T, E> SimpleFuture<T, E> {
  pub fn new() -> SimpleFuture<T, E> {
    SimpleFuture::<T, E> {
      state: Rc::new(RefCell::new(SimpleFutureState::<T, E> {done: false, waker: None, result: None}))
    }
  }
}

unsafe extern "C" fn timer_on_timer<T: FnMut(ErrorCode)>(error: *mut ThalamusErrorCode, data: *mut ::std::os::raw::c_void) {
  let mut args = unsafe {
    let raw_args = &mut*(data as *mut TimerCallbackArgs<T>);
    Box::from_raw(raw_args)
  };
  let error_code = ErrorCode::new(args.api, error);
  
  (args.callback)(error_code);
}

struct TimerCallbackArgs<T> {
  pub api: ThalamusAPI,
  pub callback: T
}

#[derive(Debug)]
pub struct ErrorCode {
  pub code: i32,
  pub message: String
}

impl ErrorCode {
  pub fn aborted(&self) -> bool {
    self.code == *OPERATION_ABORTED.get().unwrap()
  }

  pub fn new(api: ThalamusAPI, error: *mut ThalamusErrorCode) -> ErrorCode {
    unsafe {
      let code = ((&(*api.raw)).error_code_value.unwrap())(error);
      let message = if code != 0 {
        let mut span = ThalamusCharSpan { data: null(), size: 0, owns_data: 0};
        ((&(*api.raw)).error_code_message.unwrap())(&mut span as *mut ThalamusCharSpan, error);

        let slice = slice::from_raw_parts(span.data as *mut u8, span.size as usize);
        let text = str::from_utf8(slice).unwrap().to_string();
        ((&(*api.raw)).charspan_release.unwrap())(&mut span as *mut ThalamusCharSpan);
        text
      } else {
        "".to_string()
      };

      ErrorCode { code, message }
    }
  }
}

struct StateConnectionCallbackArgs<T: FnMut(State, StateAction, StateValue, StateValue)> {
  pub callback: T,
  pub api: ThalamusAPI
}

pub trait DictSetter {
  fn set_dict_state(&self, key_raw: &str, value: &State);
  fn set_dict_str(&self, key_raw: &str, value_raw: &str);
  fn set_dict_int(&self, key_raw: &str, value_raw: i64);
  fn set_dict_float(&self, key_raw: &str, value_raw: f64);
  fn set_dict_null(&self, key_raw: &str);
  fn set_dict_bool(&self, key_raw: &str, value: bool);
}

pub trait ListSetter {
  fn set_list_state(&self, key_raw: i64, value: &State);
  fn set_list_str(&self, key_raw: i64, value_raw: &str);
  fn set_list_int(&self, key_raw: i64, value_raw: i64);
  fn set_list_float(&self, key_raw: i64, value_raw: f64);
  fn set_list_null(&self, key_raw: i64);
  fn set_list_bool(&self, key_raw: i64, value: bool);
}

pub struct StateIter {
  api: ThalamusAPI,
  raw: *mut ThalamusStateIter,
}

pub struct StateEntry {
  pub key: StateKey,
  pub val: StateValue,
}

impl Iterator for StateIter {
  type Item = StateEntry;

  fn next(&mut self) -> Option<Self::Item> {
    let readable = unsafe {
      ((&*self.api.raw).state_iter_next.unwrap())(self.raw)
    };

    if readable != 0 {
      let (raw_key, raw_val) = unsafe {
        (((&*self.api.raw).state_iter_key.unwrap())(self.raw), ((&*self.api.raw).state_iter_value.unwrap())(self.raw))
      };

      let key = wrap_state_key(self.api, raw_key);
      let val = wrap_state(self.api, raw_val);

      unsafe {
        ((&*self.api.raw).state_dec_ref.unwrap())(raw_key);
        ((&*self.api.raw).state_dec_ref.unwrap())(raw_val);
      };

      Some(StateEntry{key, val})
    } else {
      None
    }
  }
}

impl IntoIterator for &State {
  type Item = StateEntry;
  type IntoIter = StateIter;

  fn into_iter(self) -> Self::IntoIter {
    let raw = unsafe {
      ((&*self.api.raw).state_iter_create.unwrap())(self.state)
    };
    StateIter { api: self.api, raw }
  }
}

impl Drop for StateIter {
  fn drop(&mut self) {
    unsafe {
      ((&*self.api.raw).state_iter_destroy.unwrap())(self.raw);
    }
  }
}

impl State {
  pub fn new(api:ThalamusAPI, state:*mut ThalamusState) -> State {
    unsafe {
      ((&*api.raw).state_inc_ref.unwrap())(state);
    }
    State {
      state: state, api: api
    }
  }

  pub fn contains_key(&self, val: StateKey) -> bool {
    for entry in self {
      if entry.key == val {
        return true;
      }
    }
    false
  }

  pub fn contains_val(&self, val: StateValue) -> bool {
    for entry in self {
      if entry.val == val {
        return true;
      }
    }
    false
  }

  pub fn parent(&self) -> Option<State> {
    let state = unsafe {
      ((&*self.api.raw).state_parent.unwrap())(self.state)
    };
    if state.is_null() {
      None
    } else {
      Some(State { api: self.api, state })
    }
    //state_parent increments the reference count and we don't want to increment it again with
    //State::new
  }

  pub fn key_of(&self, val: &State) -> Option<StateKey> {
    let key = unsafe {
      ((&*self.api.raw).state_key_of.unwrap())(self.state, val.state)
    };

    if key.is_null() {
      None
    } else {
      let result = Some(wrap_state_key(self.api, key));
      unsafe {
        ((&*self.api.raw).state_dec_ref.unwrap())(key);
      };
      result
    }
    //state_parent increments the reference count and we don't want to increment it again with
    //State::new
  }

  pub fn get(&self, index: StateKey) -> Option<StateValue> {
    let result = match index {
      StateKey::Int(key) => {
        unsafe {
          ((&*self.api.raw).state_get_at_index.unwrap())(self.state, key as u64)
        }
      },
      StateKey::String(key_raw) => {
        let span = ThalamusCharSpan { data: key_raw.as_ptr() as *const i8, size: key_raw.len() as u64, owns_data: 0 };
        unsafe {
          ((&*self.api.raw).state_get_at_name.unwrap())(self.state, &span)
        }
      }
    };
    if result.is_null() {
      None
    } else {
      let raw = result;
      let wrapped = wrap_state(self.api, raw);
      unsafe {
        ((&*self.api.raw).state_dec_ref.unwrap())(raw);
      };
      Some(wrapped)
    }
  }

  pub fn set(&self, index: StateKey, raw_value: StateValue) {
    let api = unsafe { &*self.api.raw };
    match index {
      StateKey::Int(key) => {
        unsafe {
          match raw_value {
            StateValue::Bool(value) => {
              (api.state_set_at_index_bool.unwrap())(self.state, key, if value { 1 } else { 0 });
            },
            StateValue::Dict(value) => {
              (api.state_set_at_index_state.unwrap())(self.state, key, value.state);
            },
            StateValue::Float(value) => {
              (api.state_set_at_index_float.unwrap())(self.state, key, value);
            },
            StateValue::Int(value) => {
              (api.state_set_at_index_int.unwrap())(self.state, key, value);
            },
            StateValue::List(value) => {
              (api.state_set_at_index_state.unwrap())(self.state, key, value.state);
            },
            StateValue::String(rust_value) => {
              let value_span = ThalamusCharSpan { data: rust_value.as_ptr() as *const i8, size: rust_value.len() as u64, owns_data: 0 };
              (api.state_set_at_index_string.unwrap())(self.state, key, &value_span as *const ThalamusCharSpan);
            },
            StateValue::Null => {
              (api.state_set_at_index_null.unwrap())(self.state, key);
            },
          };
        }
      },
      StateKey::String(key_raw) => {
        let key_span = ThalamusCharSpan { data: key_raw.as_ptr() as *const i8, size: key_raw.len() as u64, owns_data: 0 };
        let key = &key_span as *const ThalamusCharSpan;
        unsafe {
          match raw_value {
            StateValue::Bool(value) => {
              (api.state_set_at_name_bool.unwrap())(self.state, key, if value { 1 } else { 0 });
            },
            StateValue::Dict(value) => {
              (api.state_set_at_name_state.unwrap())(self.state, key, value.state);
            },
            StateValue::Float(value) => {
              (api.state_set_at_name_float.unwrap())(self.state, key, value);
            },
            StateValue::Int(value) => {
              (api.state_set_at_name_int.unwrap())(self.state, key, value);
            },
            StateValue::List(value) => {
              (api.state_set_at_name_state.unwrap())(self.state, key, value.state);
            },
            StateValue::String(rust_value) => {
              let value_span = ThalamusCharSpan { data: rust_value.as_ptr() as *const i8, size: rust_value.len() as u64, owns_data: 0 };
              (api.state_set_at_name_string.unwrap())(self.state, key, &value_span as *const ThalamusCharSpan);
            },
            StateValue::Null => {
              (api.state_set_at_name_null.unwrap())(self.state, key);
            },
          };
        }
      }
    };
  }

  pub fn recap(&self) {
    unsafe {
      ((&*self.api.raw).state_recap.unwrap())(self.state);
    }
  }

  pub fn recap_with<T: FnMut(State, StateAction, StateValue, StateValue)>(&self, callback: T) {
    let callback_args = Box::new(StateConnectionCallbackArgs {
      callback,
      api: self.api
    });
    let raw = Box::into_raw(callback_args);

    unsafe {
      ((&*self.api.raw).state_recap_with.unwrap())(self.state, Some(state_on_change::<T>), raw as *mut c_void);
    }

    unsafe { drop(Box::from_raw(raw)); }
  }

  pub fn connect<'a, T: FnMut(State, StateAction, StateValue, StateValue) + 'static>(&self, callback: T) -> OnDrop
  {
    let callback_args = Box::new(StateConnectionCallbackArgs {
      callback,
      api: self.api
    });
    let raw = Box::into_raw(callback_args);
    let connection = unsafe {
      ((&*self.api.raw).state_recursive_change_connect.unwrap())(self.state, Some(state_on_change::<T>), raw as *mut c_void)
    };
    println!("connect {:?} {:?} {:?}", self.api, connection, raw);

    let cleanup_api = self.api;
    let cleanup = move || {
      println!("connect drop {:?} {:?}", cleanup_api, connection);
      unsafe {
        ((&*cleanup_api.raw).state_recursive_change_disconnect.unwrap())(connection);
        drop(Box::from_raw(raw));
      }
      println!("StateConnection::drop");
    };

    OnDrop { action: Box::new(cleanup) }
  }
}

impl Clone for State {
  fn clone(&self) -> Self {
    State::new(self.api, self.state)
  }
}

impl Drop for State {
  fn drop(&mut self) {
    unsafe {
      ((&*self.api.raw).state_dec_ref.unwrap())(self.state);
    }
  }
}

pub struct OnDrop {
  action: Box<dyn FnMut()>
}

impl OnDrop {
  pub fn noop() -> OnDrop {
    OnDrop { action: Box::new(|| {}) }
  }
}

impl Drop for OnDrop {
  fn drop(&mut self) {
    (self.action)();
  }
}

pub struct Timer {
  pub timer: *mut ThalamusTimer,
  pub api: ThalamusAPI
}

impl Timer {
  pub fn sleep_callback<T: FnMut(ErrorCode)>(&self, duration: Duration, callback: T) {
    unsafe {
      let api = &*self.api.raw;
      let boxed = Box::new(TimerCallbackArgs {
        api: self.api,
        callback
      });
      let args = Box::into_raw(boxed)  as *mut std::os::raw::c_void;
      let ns = duration.as_nanos() as u64;
      (api.timer_expire_after_ns.unwrap())(self.timer, ns);
      (api.timer_async_wait.unwrap())(self.timer, Some(timer_on_timer::<T>), args);
    }
  }
  pub fn sleep(&self, duration: Duration) -> SimpleFuture<(), ErrorCode> {
    let future = SimpleFuture::new();
    let state = future.state.clone();
    self.sleep_callback(duration, move |error| {
      resolve_simple_future(&state, (), error);
    });
    future
  }
}

impl Drop for Timer {
  fn drop(&mut self) {
    unsafe {
      println!("drop {:?} {:?}", self.api, self.timer);
      ((&(*self.api.raw)).timer_destroy.unwrap())(self.timer);
    }
  }
}

pub struct PredropToken {
  pub api: *mut ThalamusAPIRaw,
  pub node: *mut ThalamusNode,
}
unsafe impl Send for PredropToken {}

impl PredropToken {
  pub fn ready(&self) {
    unsafe {
      ((&(*self.api)).node_predrop_ready.unwrap())(self.node);
    }
  }
}

pub trait Node {
  fn process(&self, handle: Request, request: Json);
  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, token: MainThreadToken) -> Self where Self: Sized;
  fn prepare() -> bool where Self: Sized { true }
  fn cleanup() where Self: Sized {}
  fn predrop(&self, token: PredropToken) {
    token.ready()
  }
  fn modalities(&self) -> u32 { 0 }
}

//impl<'a, REF, VAL: ?Sized, FUNC: Fn(&Ref<'a, REF>) -> &'a VAL> RefCellGuard<'a, REF, VAL, FUNC> {
//  pub fn new(_ref: Ref<'a, REF>, val:&'a VAL) -> RefCellGuard<'a, REF, VAL> {
//    RefCellGuard::<'a> {
//      _ref, val
//    }
//  }
//}

pub trait NodeData {
  fn time(&self) -> Duration;
  fn analog(&self) -> Option<&dyn AnalogData> { None }
  fn image(&self) -> Option<&dyn ImageData> { None }
  fn mocap(&self) -> Option<&dyn MocapData> { None }
}

pub trait AnalogData {
  fn data(
          &self,
          channel: i32,
      ) -> &[f64] {
        panic!("Unimplemented")
      }

  fn short_data(
          &self,
          _channel: i32,
      ) -> &[i16] {
        panic!("Unimplemented")
      }

  fn int_data(
          &self,
          _channel: i32,
      ) -> &[i32] {
        panic!("Unimplemented")
      }
  fn ulong_data(
          &self,
          _channel: i32,
      ) -> &[u64] {
        panic!("Unimplemented")
      }

  fn num_channels(&self) -> i32;
  fn sample_interval(&self, channel: i32) -> Duration;
  fn name(
          &self,
          channel: ::std::os::raw::c_int,
      ) -> &str;
  fn is_short_data(&self) -> bool{
        false
      }
  fn is_int_data(&self) -> bool{
        false
      }
  fn is_ulong_data(&self) -> bool{
        false
      }
  fn is_transformed(&self) -> bool {
        false
      }
  fn scale(&self, _channel: i32) -> f64 {
        return 1.0
      }
  fn offset(&self, _channel: i32) -> f64 {
        return 0.0
      }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ImageFormat {
    Gray,
    RGB,
    YUYV422,
    YUV420P,
    YUVJ420P,
}

pub trait ImageData {
  fn plane(
          &self,
          channel: i32,
      ) -> &[u8];
  fn num_planes(&self) -> u64;
  fn format(&self) -> ImageFormat;
  fn width(&self) -> u64;
  fn height(&self) -> u64;
  fn frame_interval(&self) -> Duration;
}

pub trait MocapData {
  fn segments(
          &self,
      ) -> &[ThalamusMocapSegment];
  fn pose_name(
          &self,
      ) -> &str;
}

pub static OPERATION_ABORTED: OnceLock<i32> = OnceLock::<i32>::new();

static TOKIO_RUNTIME: Mutex<Option<tokio::runtime::Runtime>> = Mutex::new(None);

pub fn tokio_runtime() -> std::sync::MutexGuard<'static, Option<tokio::runtime::Runtime>> {
  TOKIO_RUNTIME.lock().unwrap()
}

pub fn setup(api_raw: *mut ThalamusAPIRaw) {
  unsafe {
    (*api_raw).sanitize();
    let api = &*api_raw;
    OPERATION_ABORTED.set((api.error_code_operation_aborted.unwrap())())
      .expect("Failed to initialize constant: OPERATION_ABORTED");
  }
  let mut runtime = TOKIO_RUNTIME.lock().unwrap();
  assert!(runtime.is_none(), "Tokio runtime already initialized");
  *runtime = Some(
    tokio::runtime::Builder::new_multi_thread()
      .enable_all()
      .build()
      .expect("Failed to build Tokio runtime")
  );
}

#[unsafe(no_mangle)]
pub extern "C" fn thalamus_teardown() {
  println!("thalamus_teardown");
  *TOKIO_RUNTIME.lock().unwrap() = None;
}

#[macro_export]
macro_rules! export_nodes {
    ( $(($name:literal, $type:ident)),* ) => {

#[allow(unused_imports)]
use $crate::api::{
  ThalamusNode,
  ThalamusAPIRaw,
  ThalamusNodeFactory,
};

#[unsafe(no_mangle)]
pub extern "C" fn thalamus_get_node_factories(api: *mut ThalamusAPIRaw) -> *const *const ThalamusNodeFactory {
  println!("thalamus_get_node_factories");
  $crate::api::setup(api);
  let mut vec = Vec::<*const ThalamusNodeFactory>::new();
  $(
    vec.push(ThalamusNodeFactory::new::<$type>($name, api));
  )*
  vec.push(ptr::null_mut());
  let ptr = vec.as_ptr();
  std::mem::forget(vec);
  ptr
}

}
}
