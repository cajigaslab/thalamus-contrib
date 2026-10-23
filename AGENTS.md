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
    (`media_converter_node.rs`). `frame_pool.rs` is the frame pool both
    converters share.
  - `src/node_copy.rs`: `AnalogDataCopy`, `ImageDataCopy` and `NodeDataCopy`,
    owned copies of node data for using it after a callback returns.
  - `src/shaders/*.comp`: GLSL compute shaders, compiled by `build.rs`.
  - `include/thalamus/`: `plugin.h` and `modalities.h` vendored from Thalamus.
  - `proto/thalamus.proto`: vendored from Thalamus's `proto/`; `build.rs`
    generates prost messages from it (parsed with protox, so no `protoc`).
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
  says which). The source build enables every native FFmpeg codec plus three
  external encoders: libopenh264 (H.264), libvpx-vp9 (VP9) and libopus
  (Opus); no libx264, libfdk_aac, libmp3lame. Decoding uses FFmpeg's own
  H.264, VP9 and Opus decoders. AAC and FLAC use FFmpeg's own encoders and
  decoders.
- libopus (`cmake/opus.cmake`, its own CMake build) and libvpx
  (`cmake/vpx.cmake`, configure/make; on Windows its `x86_64-win64-gcc`
  target built with clang and the dynamic CRT, `libvpx.a` copied to
  `vpx.lib`; VP9 encoder only; nasm required on x86) are static release
  builds installed into `build/<config>/codec-deps`, whose `pkgconfig` FFmpeg's
  configure searches. Rust gets their link flags from libavcodec's
  pkg-config, except on Windows, where `build.rs` links `vpx` and `opus` from
  `CODEC_DEPS_LIB_DIR` (written by `hatch_build.py`).
- OpenH264 itself isn't built or linked: Cisco's binary is only covered by
  Cisco's H.264 patent license when users download it separately and can
  turn it off. Thalamus downloads it into `~/.thalamus` at startup
  (`thalamus/openh264.py`, SHA-256 pinned) unless the user opted out in
  Preferences > H264, which creates `~/.thalamus/.no_openh264`; the binary
  is then deleted at the next startup.
  - `cmake/openh264.cmake` builds FFmpeg's wrapper (`--enable-libopenh264
    --disable-decoder=libopenh264`) against the vendored OpenH264 2.6.0 API
    headers (`cmake/openh264/include`, BSD) and a stub library
    (`cmake/openh264/stub.c`) that only FFmpeg's configure check and tools
    link.
  - The plugin defines the encoder functions FFmpeg calls,
    `WelsCreateSVCEncoder`/`WelsDestroySVCEncoder` (`rust/src/openh264.rs`),
    and forwards them to Cisco's binary loaded with libloading. It's used
    whenever the file is present; creating an encoder fails when it's
    missing or isn't 2.6.0 (its structs must match the headers), and opening
    FFmpeg's encoder fails.
  - Upgrading OpenH264 means updating the headers, `VERSION` in
    `openh264.rs`, and the names and hashes in `thalamus/openh264.py`
    together.
- On Windows FFmpeg's configure runs under MSYS2 (`sh`, `make`) and needs
  MSYS2's `pkgconf` (`pacman -S make pkgconf`), not another pkg-config on
  `PATH` (pkg-config-lite, Strawberry Perl's). MSYS2's splits
  `PKG_CONFIG_PATH` on `:`, so drive-letter paths don't work; `ffmpeg.cmake`
  passes OpenH264's `.pc` directory relative to FFmpeg's build directory. It
  warns when `sh` finds no `pkg-config` or one through a drive mount
  (`/c/...`), i.e. not MSYS2's.
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
  `src/thalamus/` copies, and `proto/thalamus.proto` to its
  `proto/thalamus.proto`; copy them over whenever Thalamus changes them.
- Structs in `plugin.h` are append-only ABIs and every appended field is
  versioned. Never read a field the other side may not have:
  - `ThalamusAPI` is copied with `copy_from_host`, which only copies the
    function slots the host reports in `version`; functions the running
    Thalamus lacks are `None`.
  - `ExtNode::request(&request, callback)` sends a JSON request to a node
    from `ThalamusAPI::get_node` (slot 143). Keep the ExtNode to send more
    later. The request is a `serde_json::Value`, converted to a host `Json`
    through its string form, and the response comes back the same way. The
    callback gets `Result<serde_json::Value, NodeRequestError>` exactly once,
    later on the main thread; on a Thalamus without the slot it gets
    `Err(Unsupported)`, also posted.
  - `ThalamusAPI::subscribe_events`/`subscribe_log` (slots 144-146) call back
    with every event (`EventMessage`: payload and time) or log message
    (`LogMessage`: text, time and remote time) Thalamus receives, on any
    thread; the host passes them as `ThalamusEvent`/`ThalamusLog`. They return
    None on an older Thalamus. Dropping the returned `OnDrop` disconnects; the
    host guarantees the callback isn't running once that returns.
  - `ThalamusAPI`/`ThalamusAPIThreadSafe::thalamus_build_type`,
    `thalamus_version` and `thalamus_commit` (slots 150-152, any thread;
    the host sets a static `ThalamusCharSpan`) return what STORAGE2 records,
    or None on an older Thalamus.
  - `NodeData::blob()` reads the blob modality (`BlobData`) of host nodes. It
    is gated on API version 143, since every Thalamus with node_request has
    `ThalamusNode::blob`.
  - Fields of structs Thalamus provides (e.g. `ThalamusAnalogNode` of a C++
    node) are gated on `ThalamusAPI::analog_node_version()`,
    `image_node_version()`, `text_node_version()` and `mocap_node_version()`.
  - This plugin reports its own struct versions through the
    `thalamus_get_node_factory_version`, `thalamus_get_analog_node_version`,
    `thalamus_get_image_node_version`, `thalamus_get_text_node_version`,
    `thalamus_get_mocap_node_version` (and,
    when blob nodes are supported, `thalamus_get_node_version`) exports in
    `ffi.rs`; bump them when filling new fields.
  - `AnalogData`/`ImageData`/`TextData`/`MocapData::remote_time()` (default
    zero) is the message's time on the Thalamus that first produced it, if it
    came from another one (analog node version 6, image, text and mocap node
    version 1).
    Wrappers that forward another node's data (`node_copy`, the media
    converter's stats) forward it too; converted frames don't carry it.
- Enum values from Thalamus may be newer than this build: match them with a
  fallback arm instead of assuming they're known.

## Writing nodes

- Implement `Node` + `NodeConsts` (`MODALITIES`, `SIGNALS_OFFMAIN`), then add
  the node to `export_nodes!` and its UI to `src/thalamus/contrib/__init__.py`.
- `State` (and other main-thread values) may only be touched, including
  dropped, on the main thread: dropping a `State` updates host structures
  that aren't thread safe. Move them to other threads in `MainThreadOnly`
  (`new(value, api, token)`); one dropped off the main thread without being
  taken posts its value to the main thread to be dropped there.
- `ThalamusAPI` (not `Send`, only made on the main thread) is proof of being
  on the main thread, so its `post_to_main` takes closures that aren't
  `Send`: posting from the main thread can capture a `State` directly. Use
  `MainThreadOnly` only when the value really goes through another thread
  (and `ThalamusAPIThreadSafe::post_to_main` from there).
- `deferred!(expr)` posts `expr` to the main thread with
  `ThalamusAPI::global()` (the API table `setup()` stores; panics off the main
  thread), e.g. `deferred!(state.set("Running", false))` to change state
  from a state callback without re-entering it. It moves what `expr` uses,
  like a `move` closure.
- Node objects live on the main thread (`MainThreadToken`, `MainThreadOnly`).
  Data produced on other threads goes out through an `OffMainSignaler`
  (`ready`), whose `predrop`/`block` must be called before the node goes
  away.
- Work that must happen before a node is destroyed goes in a handler
  installed with `NodeToken::set_predrop` in `new()`, which must eventually
  call `PredropToken::ready` (e.g. `OffMainSignaler::predrop(token)` or once a
  stop completes). Without a handler the node is ready to drop at once. The
  handler should hold a `Weak` to the node's state, not an `Rc`/`Arc`.
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
- Tracing: `ThalamusAPIThreadSafe::trace_event(name)` and
  `trace_event_static(c"name")` return a guard that ends the event (category
  "plugin") when dropped. It isn't Send: Perfetto ties begin and end to the
  thread, so don't hold it across an await. Static names are interned by
  address (written once per thread, then referenced by id); prefer them for
  frequent events. Run Thalamus with `-t` to record
  `thalamus_<time>.perfetto-trace`.

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
- `AnalogData::channels_changed()` is true on the first message whose
  channels (count, names, formats or sample intervals) differ from the
  previous message's. Consumers check it on every message and treat the
  first message they see as changed; AudioConverter resets on it.
- Upstream data (`ExtNodeData`) reports `image()`/`analog()` per message via
  `has_image_data`/`has_analog_data`, not just per node type, because nodes
  like MEDIA_CONVERTER emit image-only and audio-only messages.
- MIC emits whatever format `cpal` captures (f32 converted to f64), one
  message per audio callback; buffer sizes vary and aren't fixed frames. It
  signals from inside cpal's audio callback, so multithreaded subscribers
  (e.g. MEDIA_CONVERTER's push) run on the audio thread and must be quick.
  The node owns the `cpal::Stream` on the main thread; dropping it stops the
  callbacks.
- Sample intervals are whole nanoseconds, so 44.1 kHz arrives as 22675 or
  22676 ns. Convert intervals to rates with `interval_to_rate` (matches the
  output and common rates within 10 ns) and sample counts to durations with
  `samples_to_duration` (integer math); never add a rounded interval once per
  sample.

## Image and audio conversion

- `ImageConverter` converts images (FFmpeg scaling, MPEG4 encode/decode) and
  `AudioConverter` analog data. Keep them separate; `MediaConverter`
  combines them for MEDIA_CONVERTER, and its single `pull()` (audio first,
  then images) is how the node gets output.
- Both converters are `Sync` and follow the same pattern:
  - `push()` runs on the source's thread (the node's subscription callback).
    Raw input is converted there (images always go through `sws_scale`,
    which copies when the formats match); encoded input is only appended to
    the parser's buffer.
  - `pull()` runs in the node's tokio task. It parses one packet at a time
    under the input lock, copies it out (`av_packet_ref`) and decodes and
    encodes outside that lock, so pull never holds up push.
  - State is split into `CodecState` (pull's decoder/encoder), `InputState`
    (push's parser, scaler, timestamps) and a `FramePool`, always locked in
    that order. A reset (new parameters, a new input format/size, or
    `channels_changed`) replaces `InputState` and the pool with a new
    generation; pull rebuilds `CodecState` when it sees the generation
    change, and frames converted for an older generation are dropped.
  - For encoded input the output format, frame pool and scaler are chosen
    from the first decoded frame, since they depend on what the decoder
    produces. For raw input they're chosen in push from the first message.
  - Encoded output is limited to what its encoder supports (`choose_output`):
    the encoder's pixel/sample formats and rates, sizes rounded down to
    whole chroma blocks and capped at the codec's maximum, and quality
    clamped to its qscale range.
  - Outputs borrow the converter: encoded outputs hold the `CodecState`
    lock while read, raw image outputs own a frame reference.
  - Both converters see every message and pass through what needs no
    conversion; the node doesn't forward anything itself.
- MEDIA_CONVERTER parameters:
  - Formats: `Video Format` and `Audio Format`, compared case
    insensitively. `Passthrough` keeps the input format, so encoded input
    is decoded and re-encoded. `Decoded` passes raw input through and
    outputs encoded input as the decoder produces it: the decoder's pixel
    format, or for AAC (decoded as floats) doubles, since Thalamus has no
    float format.
  - `MPEG4 Quality` (MPEG4 quantizer scale, 1-31) and `H264 Quality` (H264
    QP, 1-51; libopenh264 has no quantizer-scale mode, so it's set as
    qmin = qmax); lower is better, 0 uses the codec default (5, 18).
  - `MPEG1 Quality`: MPEG-1 quantizer scale, 1-31, lower is better (0 uses
    the default, 5).
  - `MJPEG Quality`: MJPEG quantizer scale, 1-31, lower is better (0 uses
    the default, 3).
  - `VP9 Quality`: VP9 CRF, 0-63, lower is better (default 24). VP9 is
    encoded for live use: constant quality (no bit rate), `deadline`
    realtime, `cpu-used` 8, `lag-in-frames` 0 (one packet per frame, no
    superframes), `row-mt`.
  - `Video Width` and `Video Height`.
  - Encoded input is assumed to arrive one whole frame per message (see
    below).
  - `MPEG1` (FFmpeg's mpeg1video encoder and decoder): YUV420P, sizes up to
    4095, and only MPEG-1's frame rates (the nearest is used). Its decoder
    can't tell whether a stream has B-frames, so it holds every I and P
    frame back a frame. That delay is accepted: MPEG1 isn't meant for
    latency sensitive use.
  - `MJPEG` (FFmpeg's encoder and decoder): one JPEG per message, every
    frame a key frame. Output is full range YUV (YUVJ420P/422P/444P, chosen
    by `pix_format_for_codec`): FFmpeg's encoder also lists limited range
    YUV but won't open with it without non-standard JPEG, so swscale
    converts the range. MJPEG input's nominal pixel format is YUVJ420P;
    the decoder's (often YUVJ422P from webcams) decides the real one.
  - Encoded video formats: `MPEG4` (FFmpeg's encoder) and `H264` (FFmpeg's
    libopenh264 encoder on Cisco's binary, see above; decoded by FFmpeg's
    own h264 decoder). H264 is YUV420P only, frames up to 36864 macroblocks
    (e.g. 4096x2304). While the OpenH264 binary isn't downloaded, H264
    frames are dropped (logged once) and the encoder is opened as soon as
    it appears.
  - `Audio Bit Rate` (kbit/s for the whole stream, 0 = 256) and `Audio Sample Rate` (0 = source
    rate).
  - `Audio Index`: the input channel to start at, forwards from 0, 1, ... or
    backwards from -1 (last), -2, .... It takes the run of channels with
    that channel's format and interval (`select_input_channels`).
  - The node's state keys and the widget names in `__init__.py` must match
    exactly.
- Every output is wrapped in `WithStats`: two f64 channels, `Latency (ms)`
  (arrival to output) and `Output Bytes`, in front of any audio channels so
  the audio stays the trailing run a downstream converter selects by default.
- AAC output is ADTS framed (FFmpeg's encoder emits raw frames; the header is
  added in `receive_packets`), so the AAC parser and decoder work without
  extradata. Every frame pushed to the encoder gives one output, empty until
  the FIFO fills a 1024-sample frame, with `encoded_count` = samples pushed.
  FFmpeg's AAC encoder supports 1-6 and 8 channels with standard layouts.
- Opus output (libopus, default 128 kbit/s) frames each packet with an
  MPEG-TS Opus control header (`0x7FE0`, then the length in 255-byte steps),
  which FFmpeg's Opus parser splits on, as ADTS does for AAC. It's mono or
  stereo only: more channels need an Opus header (extradata) a packet
  stream can't carry, so such inputs are rejected. The Opus decoder is
  opened with the input's channel count at 48 kHz.
- FLAC output (FFmpeg's encoder) is the encoder's packets as is: FLAC frames
  carry their own headers, and each one states the sample rate and bits per
  sample, so the decoder needs no STREAMINFO. Rates are rounded to ones a
  frame header can state (`flac_sample_rate`). i16 and double input are
  encoded as 16-bit, i32 input as 32-bit (`strict_std_compliance`
  experimental; without it FFmpeg keeps only the top 24 bits). Frames are
  4608 samples at 44.1/48 kHz (~100 ms), and a message can hold several.
  FFmpeg's FLAC parser only ends a frame once it has validated several
  headers after it, so FLAC input is parsed with
  `PARSER_FLAG_COMPLETE_FRAMES`: each message goes to the decoder as one
  packet, which decodes every frame in it. Frames after the first come out
  with no pts and are timed by the packet's seq (`CodecState::packet_seq`).
- `ThalamusAnalogEncoding_FLAC` (3) needs a Thalamus that knows it: older
  builds abort on encodings they don't know.
- FFmpeg pitfalls hit here: call `swr_init` after `swr_alloc_set_opts2`;
  allocate buffers (`av_frame_get_buffer`) for frames you write into, and
  never call it on a frame that already has buffers (it leaks them); drain
  `avcodec_receive_packet`/`_frame` after every send or the next send returns
  EAGAIN; `av_parser_parse2` can return a packet pointing into your input
  buffer, so send it before shifting the buffer; a decoded frame can be
  handed out as `av_frame_clone` when no conversion is needed.
- MPEG-4 part 2 quirks: a fresh decoder needs the VOL header, and the parser
  never sets `key_frame`, so key frames are detected with `pict_type` and
  packets before the first one are skipped. Recordings can start mid-GOP,
  before the first VOL header.
- The MPEG4 parser only ends a frame when it sees the next frame's start
  code, which delays decoding by one frame (~33 ms at 30 fps).
  The converter always sets `PARSER_FLAG_COMPLETE_FRAMES` to remove the lag,
  so each parse call must be exactly one frame and the converter parses one
  queued message at a time.
- Encoded image streams carry their nominal frame rate (the encoder's time
  base, from the source's frame interval), which webcams don't keep; use
  message times, not the stream's rate, for timing.

## STORAGE3

- `storage3_node.rs` records like Thalamus's STORAGE2 (events, log, and the
  Sources' analog, image, motion, text and blob data, plus its metrics as
  analog data), without file copying, Simple Copy or compression. Records
  are the prost-generated `storage_record::proto` messages
  (`Image.Format` values differ from `ThalamusImageFormat`'s), encoded and
  framed with an 8-byte big-endian length in `storage_record.rs`.
- "Outputs" is a list of `{"URL": ..., "Main": bool}`. The node keeps
  exactly one Main (`normalize_main`, called from the state callback on any
  change in the list; its sets re-enter the callback, which converges): one just set Main, else the existing one, else the first. Plain paths and file:// URLs are
  files; http(s):// URLs get one streamed (chunked) POST lasting the whole
  recording, as Thoth's POST /files/PATH expects (no checksum is posted).
  If the response comes before the body ends (e.g. an error), the body
  stream is cancelled so the output stops at once. The root config goes to
  the output's name plus `.json`, as with STORAGE2.
- Names render like STORAGE2's: `{{rec}}`, strftime fields (left as they
  are if strftime can't parse the template), then `.YYYYMMDD.N`, with N the
  first recording number the Main output doesn't have yet (checked with HEAD
  for HTTP), used for every output.
- The metadata record gets the Metadata table, then Thalamus's "Build Type",
  "Version" and "Commit" (STORAGE2's keys; left out on a Thalamus without
  API slots 150-152) and this plugin's "Contrib Build Type", "Contrib
  Version" and "Contrib Commit" (`build_info.rs`: version from
  `__about__.py`, commit from git at build time in `build.rs`).
- The writer thread sorts each sweep's records by time and sends one shared
  buffer (`bytes::Bytes`) to each output over a tokio unbounded channel:
  files are written by their own thread (`blocking_recv`), HTTP outputs by a
  reqwest task on the plugin's tokio runtime, whose body streams from the
  channel. A failed output logs and drops its receiver; the writer then
  stops sending to it, so the others aren't affected. The writer isn't a
  runtime thread, so it uses `Handle::block_on` for the HEAD probes and to
  wait for HTTP tasks when recording stops.

## Image viewer and imgui windows

- `ImageSink::update` may be called from any thread. It never waits on the
  render loop or GPU: it drops the frame if the pool is busy or no texture is
  free, copies raw planes into a host-visible buffer and submits a compute
  shader (`shaders/convert.comp`) that converts to RGBA. Formats: Gray, RGB,
  BGR, YUYV422, YUV420P, YUVJ420P, NV12, Gray16, RGB16 (native byte order,
  full 16-bit range).
- Encoded frames (MJPEG, MPEG1, MPEG4, H264, VP9; `image_converter::is_compressed`)
  are decoded synchronously in `update`, on the caller's thread, by an
  `ImageConverter` in `Decoded` mode that the sink holds while a viewer is
  open (created in `ViewerWindow::new`, dropped first in `ImageViewer`'s
  drop). Every encoded frame is decoded, since inter-frame codecs need them
  all; only the decoded frames go through the skip-if-replaced check. The
  decoder has its own lock, so decoding never holds the pool lock the render
  loop takes, but callers on two threads wait for each other.
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
- The audio converter tests run real FFmpeg AAC and FLAC encode/decode round trips;
  image converter tests can run a whole `ImageConverter` with `test_api()`
  (a zeroed host API with only a clock);
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
  the UI plots use; `ThalamusStub.image` for image format and size).
  MEDIA_CONVERTER's `Latency (ms)` and `Output Bytes` channels are a quick
  check of a conversion path.
- A Rust panic in a callback aborts Thalamus with a message on stderr; a
  native crash only leaves a minidump (see Thalamus's AGENTS.md for reading
  them).
