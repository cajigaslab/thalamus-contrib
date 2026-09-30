# AGENTS.md

Notes for coding agents working on thalamus-contrib, the Rust plugin that adds
nodes to Thalamus (https://github.com/cajigaslab/Thalamus; active work is on
its `devel` branch, releases on `main`). CLAUDE.md has the short version.

## Layout

- `rust/`: the plugin, a `cdylib` loaded by Thalamus at runtime.
  - `src/lib.rs`: module list and the `export_nodes!` table; a node must be
    listed there to be loaded.
  - `src/api.rs`: the safe Rust API over the Thalamus C API (`State`,
    `NodeData`/`AnalogData`/`ImageData`, `OffMainSignaler`, `run_task`, ...).
  - `src/ffi.rs`: the C glue: node factories, the `c_node_*` callbacks that
    expose Rust nodes to Thalamus, and the `thalamus_get_*` exports.
  - `src/*_node.rs`: nodes. Keep them free of `unsafe`.
  - `src/image_viewer.rs`, `imgui_window.rs`, `imgui_platform.rs`: the Vulkan +
    imgui preview window nodes open with "View".
  - `src/image_converter.rs`, `audio_converter.rs`, `media_converter.rs`:
    FFmpeg-based conversion used by IMAGE_CONVERTER.
  - `src/shaders/*.comp`: GLSL compute shaders, compiled by `build.rs`.
  - `include/thalamus/`: `plugin.h` and `modalities.h` vendored from Thalamus.
- `src/thalamus/contrib/`: the Python package; `__init__.py` declares each
  node's UI (`Factory`, `UserData`), plus custom widgets (`*_widget.py`).
- `cmake/`: native dependencies (FFmpeg, OpenCV, ...), driven by
  `hatch_build.py`.

## Building

- Build with `hatch build` from the repo root, from PowerShell on Windows (Git
  Bash's `link.exe` shadows MSVC's). Don't run `cargo build` first: the hatch
  hook builds the native dependencies and writes `FFMPEG_DIR` and friends to
  `.cargo/config.toml`. After one `hatch build`, `cd rust; cargo test` works.
- `cmake/ffmpeg.cmake` uses a system FFmpeg 7.x if pkg-config finds one,
  otherwise builds FFmpeg from source (`build/<config>/ffmpeg_provider.txt`
  says which). The source build enables every native FFmpeg codec but no
  external libraries (no libx264, libopus, libfdk_aac, libmp3lame).
- Windows uses the dynamic CRT everywhere (Rust and skia's prebuilt library
  do); new native dependencies must too.
- If FFmpeg rebuilds unexpectedly, run `ninja -n -d explain`: two different
  ninja versions on PATH hash command lines differently, which marks every
  command as changed.
- Linux builds of `cpal` (MIC node) need the ALSA development package
  (`libasound2-dev`).
- `build.rs` generates the C bindings from `include/thalamus/plugin.h`
  (bindgen, `-x c`; Thalamus enums become newtypes with their full prefixed
  names, e.g. `ThalamusImageFormat::ThalamusImageFormat_Gray`) and compiles
  `src/shaders/*.comp` to SPIR-V with naga (no shader toolchain needed).

## The C API and versioning

- Keep `include/thalamus/plugin.h` and `modalities.h` identical to Thalamus's
  `src/thalamus/` copies; copy them over whenever Thalamus changes them.
- Structs in `plugin.h` are append-only ABIs and every appended field is
  versioned. Never read a field the other side may not have:
  - `ThalamusAPI` is copied with `copy_from_host`, which only copies the
    function slots the host reports in `version`; functions the running
    Thalamus lacks are `None`.
  - Fields of structs Thalamus provides (e.g. `ThalamusAnalogNode` of a C++
    node) are gated on `ThalamusAPI::analog_node_version()`.
  - This plugin reports its own struct versions through the
    `thalamus_get_node_factory_version`, `thalamus_get_analog_node_version`
    (and, when blob nodes are supported, `thalamus_get_node_version`) exports
    in `ffi.rs`; bump them when filling new fields.
- Enum values from Thalamus may be newer than this build: match them with a
  fallback arm instead of assuming they're known.

## Writing nodes

- Implement `Node` + `NodeConsts` (`MODALITIES`, `SIGNALS_OFFMAIN`), then add
  the node to `export_nodes!` and its UI to `src/thalamus/contrib/__init__.py`.
- Node objects live on the main thread (`MainThreadToken`, `MainThreadOnly`).
  Data produced on other threads goes out through an `OffMainSignaler`
  (`ready`), whose `predrop`/`block` must be called before the node goes
  away.
- State callbacks are synchronous and re-entrant: `state.set(...)` inside a
  callback calls the callback again. Borrow the node inside each match arm
  rather than for the whole callback, and release the borrow before calling
  anything that may set state (e.g. `ImageViewer::new` writes
  `view_geometry`). To change state from inside a callback, `post_to_main`.
- `State::connect` is recursive: it also fires for keys in nested
  collections. `State` equality is pointer equality on interned wrappers, so
  `source == node_state` filters to the node's own keys.
- Dropping a `TaskScope` from inside its own task deadlocks; post the work
  that drops it to the main thread instead.

## Analog data

- `AnalogData` sample types: `data` (f64), `short_data` (i16), `int_data`
  (i32), `ulong_data` (u64), selected by the `is_*` functions, or per channel
  with `analog_format(channel)`. `AnalogFormat::Encoded` channels carry no
  samples; their samples are in `buffer()` (`encoding()`, e.g. AAC, ADTS
  framed) with `encoded_count()` samples per channel.
- A message has one sample type per channel, so e.g. f64 stats channels can't
  ride along with i16 audio.
- MIC emits whatever format `cpal` captures (f32 converted to f64), one
  message per audio callback; buffer sizes vary and aren't fixed frames.

## Image and audio conversion

- `image_converter::Converter` converts images (FFmpeg scaling, MPEG4
  encode/decode). Keep audio out of it; `AudioConverter` handles analog data
  and `MediaConverter` combines the two for IMAGE_CONVERTER.
- `push()` only copies input; conversion happens in `pull()`, which the node
  runs on the tokio pool.
- AAC output: one output per encoded input, with `encoded_count` equal to the
  samples that input contributed (at the AAC rate), even when the encoder
  hasn't produced a frame yet. FFmpeg's AAC encoder supports 1-6 and 8
  channels with standard layouts; extra channels are dropped. Rates are
  snapped to AAC rates within 0.1%, otherwise resampled.
- MPEG-4 part 2 quirks: a fresh decoder needs the VOL header, and the parser
  never sets `key_frame`, so key frames are detected with `pict_type`.

## Image viewer and imgui windows

- `ImageSink::update` may be called from any thread. It never waits: it
  drops the frame if the pool is busy or no texture is free, copies raw
  planes into a host-visible buffer and submits a compute shader
  (`shaders/convert.comp`) that converts to RGBA. Formats: Gray, RGB, BGR,
  YUYV422, YUV420P, YUVJ420P, NV12, Gray16, RGB16 (native byte order, full
  16-bit range). MJPEG/MPEG1/MPEG4 are dropped.
- Never touch an `ImguiWindow`'s in-flight fences from another thread: the
  render loop resets and submits them. Texture reuse is tracked with frame
  numbers instead.
- `ImguiWindow::render_frame` never blocks: if the frame slot's fence isn't
  signaled or no swapchain image is free it only pumps events and returns.
  Each window's imgui context stays suspended except while it renders, so
  several windows can coexist on the main thread.
- Shaders run on Thalamus's shared graphics queue, which Thalamus picks with
  compute support; SPIR-V must stay valid for Vulkan 1.0 +
  `SPV_KHR_storage_buffer_storage_class`. Avoid arrays in push constant
  blocks (naga lays them out with a 16-byte stride).

## Testing

- `cd rust; cargo test` after a `hatch build` (from PowerShell on Windows).
- Tests needing hardware are `#[ignore]`d, e.g.
  `cargo test capture_from_default_device -- --ignored --nocapture`.
- The audio converter tests run real FFmpeg AAC encode/decode round trips;
  prefer that style (synthetic input through the real codec) for conversion
  code.
