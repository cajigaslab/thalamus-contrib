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
    FFmpeg-based conversion used by MEDIA_CONVERTER
    (`media_converter_node.rs`).
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
- A message can mix formats across channels (MEDIA_CONVERTER puts f64 stats
  channels in front of i16 audio). Then the message-wide `is_*` functions are
  false, so consumers must read each channel by `analog_format(channel)`;
  Thalamus's `visit_node` readers don't (see its AGENTS.md).
- Upstream data (`ExtNodeData`) reports `image()`/`analog()` per message via
  `has_image_data`/`has_analog_data`, not just per node type, because nodes
  like MEDIA_CONVERTER emit image-only and audio-only messages.
- MIC emits whatever format `cpal` captures (f32 converted to f64), one
  message per audio callback; buffer sizes vary and aren't fixed frames.
- Sample intervals are whole nanoseconds, so 44.1 kHz arrives as 22675 or
  22676 ns. Convert intervals to rates with `interval_to_rate` (matches the
  output and common rates within 10 ns) and sample counts to durations with
  `samples_to_duration` (integer math); never add a rounded interval once per
  sample.

## Image and audio conversion

- `image_converter::Converter` converts images (FFmpeg scaling, MPEG4
  encode/decode). Keep audio out of it; `AudioConverter` handles analog data
  and `MediaConverter` combines the two for MEDIA_CONVERTER.
- `push()` only copies input; conversion happens in `pull()`, which the node
  runs on the tokio pool. Both converters see every message and pass through
  what needs no conversion; the node doesn't forward anything itself.
- MEDIA_CONVERTER parameters: `Video Format`/`Video Quality`/`Video Width`/
  `Video Height`, `Audio Format` (`PASSTHROUGH` keeps the input format, so
  AAC input is decoded and re-encoded), `Audio Bit Rate` (kbit/s, 0 = 64),
  `Audio Sample Rate` (0 = source rate) and `Audio Index`: the input channel
  to start at, forwards from 0, 1, ... or backwards from -1 (last), -2, ...;
  it takes the run of channels with that channel's format and interval
  (`select_input_channels`). The node's state keys and the widget names in
  `__init__.py` must match exactly.
- Every output is wrapped in `WithStats`: two f64 channels, `Latency (ms)`
  (arrival to output) and `Output Bytes`, in front of any audio channels so
  the audio stays the trailing run a downstream converter selects by default.
- AAC output is ADTS framed (FFmpeg's encoder emits raw frames; the header is
  added in `receive_packets`), so the AAC parser and decoder work without
  extradata. Every frame pushed to the encoder gives one output, empty until
  the FIFO fills a 1024-sample frame, with `encoded_count` = samples pushed.
  FFmpeg's AAC encoder supports 1-6 and 8 channels with standard layouts.
- FFmpeg pitfalls hit here: call `swr_init` after `swr_alloc_set_opts2`;
  allocate buffers (`av_frame_get_buffer`) for frames you write into, and
  never call it on a frame that already has buffers (it leaks them); drain
  `avcodec_receive_packet`/`_frame` after every send or the next send returns
  EAGAIN; `av_parser_parse2` can return a packet pointing into your input
  buffer, so send it before shifting the buffer; a decoded frame can be
  handed out as `av_frame_clone` when no conversion is needed.
- MPEG-4 part 2 quirks: a fresh decoder needs the VOL header, and the parser
  never sets `key_frame`, so key frames are detected with `pict_type`.
  Recordings can start mid-GOP, before the first VOL header.

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

## Python widgets

- Widgets are built from the config as soon as the UI loads it, before
  Thalamus has created their nodes. Address node requests with
  `selector=NodeSelector(name=config['name'], type=config['type'])`: the
  server waits for a matching node (the unary call answers NOT_FOUND after
  5 s) instead of answering immediately.
- An exception escaping a `create_task_with_exc_handling` task takes down the
  whole Python UI, which cancels every request it has in flight (and has
  exposed use-after-return bugs in Thalamus's gRPC handlers). Don't let
  widget code raise, e.g. on `json.loads` of an empty response.

## Testing

- `cd rust; cargo test` after a `hatch build` (from PowerShell on Windows).
- Tests needing hardware are `#[ignore]`d, e.g.
  `cargo test capture_from_default_device -- --ignored --nocapture`.
- The audio converter tests run real FFmpeg AAC encode/decode round trips;
  prefer that style (synthetic input through the real codec) for conversion
  code. Keep logic like channel selection or rate math in pure functions so
  it can be unit tested without a Thalamus API.
- Thalamus loads `src/thalamus/contrib/thalamus_contrib.dll`, which
  `hatch build` copies from `rust/target/`. The copy fails if a running
  `native.exe` has the DLL open; if a change seems to have no effect, check
  that file's timestamp (`copy2` keeps the build's).
- End to end: from a Thalamus checkout, with this repo's `src` first on
  `PYTHONPATH`, run `python -m thalamus.pipeline --contrib -c config.json` in
  the background. The pipeline resets every `Running` to false on load, so
  start nodes with `python -m thalamus.registry -p '$.nodes[0].Running' -s
  true` (`-p` alone prints a value). Read node output with a small gRPC client
  (`ThalamusStub.analog`, with or without `native_formats`, or `graph`, which
  the UI plots use).
- A Rust panic in a callback aborts Thalamus with a message on stderr; a
  native crash only leaves a minidump (see Thalamus's AGENTS.md for reading
  them).
