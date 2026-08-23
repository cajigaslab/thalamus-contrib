//! imgui-rs platform backend built entirely on ThalamusAPI -- no direct SDL3
//! references. Window creation/destruction lives on `crate::api::SDLWindow`;
//! this module only translates the events that window's creator receives
//! (via `ThalamusAPI::subscribe_sdl_events`) into `imgui::Io` updates, plus
//! cursor shape and clipboard plumbing.

use crate::api::{OnDrop, SDLCursor, ThalamusAPI};
use crate::ffi::*;
use imgui::{ClipboardBackend, Context, Key, MouseButton};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

struct ThalamusClipboard {
  api: ThalamusAPI,
}

impl ClipboardBackend for ThalamusClipboard {
  fn get(&mut self) -> Option<String> {
    let text = self.api.get_clipboard_text();
    if text.is_empty() { None } else { Some(text) }
  }

  fn set(&mut self, text: &str) {
    self.api.set_clipboard_text(text);
  }
}

pub struct ImguiPlatform {
  api: ThalamusAPI,
  window_id: u32,
  events: Rc<RefCell<VecDeque<ThalamusSDLEvent>>>,
  _subscription: OnDrop,
  cursors: HashMap<usize, SDLCursor>,
  last_cursor: Option<Option<imgui::MouseCursor>>,
  should_close: bool,
}

impl ImguiPlatform {
  /// `window_id` is the THALAMUS_SDL_WindowID of the window this platform
  /// backend should react to (from `SDLWindow::id()`); events for other
  /// windows (e.g. Thalamus's own preview windows) are ignored except for
  /// the process-wide QUIT event.
  pub fn new(api: ThalamusAPI, ctx: &mut Context, window_id: u32) -> ImguiPlatform {
    ctx.set_ini_filename(None);
    ctx.io_mut().backend_flags.insert(imgui::BackendFlags::HAS_MOUSE_CURSORS);
    ctx.set_clipboard_backend(ThalamusClipboard { api });

    let events: Rc<RefCell<VecDeque<ThalamusSDLEvent>>> = Rc::new(RefCell::new(VecDeque::new()));
    let events_for_cb = events.clone();
    let subscription = api.subscribe_sdl_events(move |event| {
      events_for_cb.borrow_mut().push_back(*event);
    });

    let mut cursors = HashMap::new();
    for id in [
      THALAMUS_SDL_SYSTEM_CURSOR_DEFAULT,
      THALAMUS_SDL_SYSTEM_CURSOR_TEXT,
      THALAMUS_SDL_SYSTEM_CURSOR_MOVE,
      THALAMUS_SDL_SYSTEM_CURSOR_NS_RESIZE,
      THALAMUS_SDL_SYSTEM_CURSOR_EW_RESIZE,
      THALAMUS_SDL_SYSTEM_CURSOR_NESW_RESIZE,
      THALAMUS_SDL_SYSTEM_CURSOR_NWSE_RESIZE,
      THALAMUS_SDL_SYSTEM_CURSOR_POINTER,
      THALAMUS_SDL_SYSTEM_CURSOR_NOT_ALLOWED,
    ] {
      if let Some(cursor) = api.create_system_cursor(id) {
        cursors.insert(id as usize, cursor);
      }
    }

    ImguiPlatform {
      api,
      window_id,
      events,
      _subscription: subscription,
      cursors,
      last_cursor: None,
      should_close: false,
    }
  }

  /// True once a quit/close-requested event for this window has been seen.
  pub fn should_close(&self) -> bool {
    self.should_close
  }

  /// Drains pending SDL events into `ctx`'s `Io` and syncs the OS cursor to
  /// whatever imgui wants this frame. Call once per frame, before `ctx.frame()`.
  pub fn new_frame(&mut self, ctx: &mut Context, delta_time: f32) {
    ctx.io_mut().delta_time = delta_time.max(1.0 / 1000.0);

    let pending: Vec<ThalamusSDLEvent> = self.events.borrow_mut().drain(..).collect();
    for event in &pending {
      self.apply_event(ctx, event);
    }

    self.sync_cursor(ctx);
  }

  fn sync_cursor(&mut self, ctx: &mut Context) {
    let cursor = if ctx.io().mouse_draw_cursor { None } else { ctx.mouse_cursor() };
    if self.last_cursor == Some(cursor) {
      return;
    }
    self.last_cursor = Some(cursor);
    match cursor {
      Some(mouse_cursor) => {
        if let Some(sdl_cursor) = self.cursors.get(&Self::cursor_id(mouse_cursor)) {
          self.api.set_cursor(Some(sdl_cursor));
        }
        self.api.show_cursor();
      }
      None => {
        self.api.hide_cursor();
      }
    }
  }

  fn cursor_id(cursor: imgui::MouseCursor) -> usize {
    (match cursor {
      imgui::MouseCursor::Arrow => THALAMUS_SDL_SYSTEM_CURSOR_DEFAULT,
      imgui::MouseCursor::TextInput => THALAMUS_SDL_SYSTEM_CURSOR_TEXT,
      imgui::MouseCursor::ResizeAll => THALAMUS_SDL_SYSTEM_CURSOR_MOVE,
      imgui::MouseCursor::ResizeNS => THALAMUS_SDL_SYSTEM_CURSOR_NS_RESIZE,
      imgui::MouseCursor::ResizeEW => THALAMUS_SDL_SYSTEM_CURSOR_EW_RESIZE,
      imgui::MouseCursor::ResizeNESW => THALAMUS_SDL_SYSTEM_CURSOR_NESW_RESIZE,
      imgui::MouseCursor::ResizeNWSE => THALAMUS_SDL_SYSTEM_CURSOR_NWSE_RESIZE,
      imgui::MouseCursor::Hand => THALAMUS_SDL_SYSTEM_CURSOR_POINTER,
      imgui::MouseCursor::NotAllowed => THALAMUS_SDL_SYSTEM_CURSOR_NOT_ALLOWED,
    }) as usize
  }

  fn apply_event(&mut self, ctx: &mut Context, event: &ThalamusSDLEvent) {
    match event.event_type() {
      THALAMUS_SDL_EVENT_QUIT => {
        self.should_close = true;
      }
      THALAMUS_SDL_EVENT_WINDOW_CLOSE_REQUESTED => {
        if unsafe { event.as_window() }.window_id == self.window_id {
          self.should_close = true;
        }
      }
      THALAMUS_SDL_EVENT_WINDOW_FOCUS_LOST if unsafe { event.as_window() }.window_id == self.window_id => {
        ctx.io_mut().app_focus_lost = true;
      }
      THALAMUS_SDL_EVENT_WINDOW_FOCUS_GAINED if unsafe { event.as_window() }.window_id == self.window_id => {
        ctx.io_mut().app_focus_lost = false;
      }
      THALAMUS_SDL_EVENT_MOUSE_MOTION => {
        let motion = unsafe { event.as_mouse_motion() };
        if motion.window_id == self.window_id {
          ctx.io_mut().add_mouse_pos_event([motion.x, motion.y]);
        }
      }
      t @ (THALAMUS_SDL_EVENT_MOUSE_BUTTON_DOWN | THALAMUS_SDL_EVENT_MOUSE_BUTTON_UP) => {
        let button_event = unsafe { event.as_mouse_button() };
        if button_event.window_id == self.window_id {
          if let Some(button) = Self::mouse_button(button_event.button) {
            ctx.io_mut().add_mouse_button_event(button, t == THALAMUS_SDL_EVENT_MOUSE_BUTTON_DOWN);
          }
        }
      }
      THALAMUS_SDL_EVENT_MOUSE_WHEEL => {
        let wheel = unsafe { event.as_mouse_wheel() };
        if wheel.window_id == self.window_id {
          ctx.io_mut().add_mouse_wheel_event([wheel.x, wheel.y]);
        }
      }
      t @ (THALAMUS_SDL_EVENT_KEY_DOWN | THALAMUS_SDL_EVENT_KEY_UP) => {
        let key_event = unsafe { event.as_key() };
        if key_event.window_id == self.window_id {
          if let Some(key) = Self::map_key(key_event.scancode) {
            ctx.io_mut().add_key_event(key, t == THALAMUS_SDL_EVENT_KEY_DOWN);
          }
        }
      }
      _ => {}
    }
  }

  fn mouse_button(button: u8) -> Option<MouseButton> {
    match button {
      1 => Some(MouseButton::Left),
      2 => Some(MouseButton::Middle),
      3 => Some(MouseButton::Right),
      _ => None,
    }
  }

  /// Scancode values match THALAMUS_SDL_Scancode in plugin_window_event.h
  /// (an exact mirror of SDL3's own scancodes). Only a representative subset
  /// is mapped here -- extend as needed.
  fn map_key(scancode: u32) -> Option<Key> {
    Some(match scancode {
      4 => Key::A, 5 => Key::B, 6 => Key::C, 7 => Key::D, 8 => Key::E,
      9 => Key::F, 10 => Key::G, 11 => Key::H, 12 => Key::I, 13 => Key::J,
      14 => Key::K, 15 => Key::L, 16 => Key::M, 17 => Key::N, 18 => Key::O,
      19 => Key::P, 20 => Key::Q, 21 => Key::R, 22 => Key::S, 23 => Key::T,
      24 => Key::U, 25 => Key::V, 26 => Key::W, 27 => Key::X, 28 => Key::Y, 29 => Key::Z,
      40 => Key::Enter, 41 => Key::Escape, 42 => Key::Backspace, 43 => Key::Tab, 44 => Key::Space,
      79 => Key::RightArrow, 80 => Key::LeftArrow, 81 => Key::DownArrow, 82 => Key::UpArrow,
      224 => Key::LeftCtrl, 225 => Key::LeftShift, 226 => Key::LeftAlt, 227 => Key::LeftSuper,
      228 => Key::RightCtrl, 229 => Key::RightShift, 230 => Key::RightAlt, 231 => Key::RightSuper,
      _ => return None,
    })
  }
}
