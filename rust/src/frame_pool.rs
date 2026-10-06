//! A pool of FFmpeg frames shared by a converter's push and pull sides. Both
//! sides lock it only briefly: to take a frame to write into, to queue a
//! converted frame, or to take one for output. Frames come back for reuse
//! once every reference handed out has been dropped.

use std::collections::VecDeque;

use ffmpeg_sys_next as ffi;

fn av_error_string(ret: i32) -> String {
  let mut buf = [0i8; ffi::AV_ERROR_MAX_STRING_SIZE];
  let rc = unsafe { ffi::av_strerror(ret, buf.as_mut_ptr(), buf.len()) };
  if rc == 0 {
    unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
      .to_string_lossy()
      .to_string()
  } else {
    format!("error code {}", ret)
  }
}

/// How many samples per channel an audio frame's buffers can hold.
unsafe fn frame_capacity(frame: *const ffi::AVFrame) -> i32 {
  unsafe {
    if (*frame).buf[0].is_null() {
      return 0;
    }
    let format: ffi::AVSampleFormat = std::mem::transmute((*frame).format);
    let bps = ffi::av_get_bytes_per_sample(format);
    let bytes_per_sample = if ffi::av_sample_fmt_is_planar(format) != 0 {
      bps
    } else {
      bps * (*frame).ch_layout.nb_channels
    };
    if bytes_per_sample <= 0 {
      0
    } else {
      (*frame).linesize[0] / bytes_per_sample
    }
  }
}

/// What the pool's frames hold. Fixed for a pool: a converter replaces the
/// pool when its output changes.
#[derive(Clone, Copy)]
pub(crate) enum FramePoolParams {
  Audio{layout: ffi::AVChannelLayout, format: ffi::AVSampleFormat, samplerate: i32},
  Video{format: ffi::AVPixelFormat, width: i32, height: i32},
}

impl FramePoolParams {
  pub(crate) fn empty() -> FramePoolParams {
    FramePoolParams::Audio {
      layout: unsafe { std::mem::zeroed() },
      format: ffi::AVSampleFormat::AV_SAMPLE_FMT_NONE,
      samplerate: 0,
    }
  }
}

/// Converted frames waiting for pull, and frames to reuse. A reset replaces
/// it with a pool of the new generation, and frames converted for an older
/// generation are dropped rather than queued.
pub(crate) struct FramePool {
  writable: VecDeque<*mut ffi::AVFrame>,
  pending: VecDeque<*mut ffi::AVFrame>,
  used: VecDeque<*mut ffi::AVFrame>,
  params: FramePoolParams,
  pub(crate) generation: u64,
}

// SAFETY: the frames are only touched under the pool's mutex, apart from the
// clones handed out, which own their own references.
unsafe impl Send for FramePool {}

impl FramePool {
  pub(crate) fn new(params: FramePoolParams, generation: u64) -> FramePool {
    FramePool {
      params,
      generation,
      writable: VecDeque::new(),
      pending: VecDeque::new(),
      used: VecDeque::new(),
    }
  }

  /// A frame to write into. `nb_samples` is the audio frame size; it's
  /// ignored for video, whose frames all have the pool's size.
  pub(crate) fn get_writable(&mut self, nb_samples: i32) -> *mut ffi::AVFrame {
    unsafe {
      self.used.retain(|f| {
        let writable = ffi::av_frame_is_writable(*f) != 0;
        if writable {
          self.writable.push_back(*f);
        }
        !writable
      });

      // A recycled frame keeps its buffers, so reuse it when they're big enough
      // and replace it otherwise (av_frame_get_buffer on a frame that has
      // buffers leaks them).
      if let Some(mut frame) = self.writable.pop_front() {
        let fits = match self.params {
          FramePoolParams::Audio { .. } => frame_capacity(frame) >= nb_samples,
          FramePoolParams::Video { .. } => !(*frame).buf[0].is_null(),
        };
        if fits {
          if let FramePoolParams::Audio { .. } = self.params {
            (*frame).nb_samples = nb_samples;
          }
          return frame;
        }
        ffi::av_frame_free(&mut frame);
      }

      let frame = ffi::av_frame_alloc();
      assert!(!frame.is_null(), "av_frame_alloc failed");
      match self.params {
        FramePoolParams::Audio { layout, format, samplerate } => {
          ffi::av_channel_layout_copy(&mut (*frame).ch_layout, &layout);
          (*frame).format = format as i32;
          (*frame).sample_rate = samplerate;
          (*frame).nb_samples = nb_samples;
        }
        FramePoolParams::Video { format, width, height } => {
          (*frame).format = format as i32;
          (*frame).width = width;
          (*frame).height = height;
        }
      };

      let ret = ffi::av_frame_get_buffer(frame, 0);
      assert!(ret >= 0, "ffi::av_frame_get_buffer: {}", av_error_string(ret));
      frame
    }
  }

  /// Queues `frame` for pull, or frees it if it was converted for an older
  /// generation. Returns whether it was queued.
  pub(crate) fn push_pending(&mut self, mut frame: *mut ffi::AVFrame, generation: u64) -> bool {
    if generation != self.generation {
      unsafe { ffi::av_frame_free(&mut frame) };
      return false;
    }
    self.pending.push_back(frame);
    true
  }

  /// A new reference to the next queued frame of `generation`.
  pub(crate) fn get_pending(&mut self, generation: u64) -> Option<*mut ffi::AVFrame> {
    if generation != self.generation {
      return None;
    }
    let result = self.pending.pop_front();
    result.map(|r| {
      let new_ref = unsafe { ffi::av_frame_clone(r) };
      self.used.push_back(r);
      new_ref
    })
  }
}

impl Drop for FramePool {
  fn drop(&mut self) {
    unsafe {
      for f in self.writable.iter_mut() {
        ffi::av_frame_free(f);
      }
      for f in self.pending.iter_mut() {
        ffi::av_frame_free(f);
      }
      for f in self.used.iter_mut() {
        ffi::av_frame_free(f);
      }
    }
  }
}
