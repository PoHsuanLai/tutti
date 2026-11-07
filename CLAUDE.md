# CLAUDE.md

Guidance for Claude Code working in the tutti audio engine. This file is rules
and commands. The reasoning and history behind them is in
[`docs/engineering-notes.md`](docs/engineering-notes.md): read the matching
section there before you relax or work around a rule.

## Direction: the graph is `tutti-graph`

**fundsp is gone; the audio graph is `tutti-graph`**, per
[`docs/design/013-native-graph.md`](docs/design/013-native-graph.md) (the
filename keeps the design's old "native graph" name; it is just the graph): a
`Topology` value, a pure compiler producing an immutable plan, units stored
once, and events as ports. `Engine` renders only the graph
(`Engine::new(&transport, &mut editor, executor)`), it is `bevy-tutti`'s
only runtime (`AudioGraphRes` wraps a `tutti_session::Session`, whose graph
holds the `Editor`; PDC is the compiler's, export forks the live graph with
`Editor::fork`), and every node is a `tutti_graph::Node`. fundsp's `Net`,
the vendored fork, `tutti-node` and its `AudioUnit` trait, `Setting` and
the numeric tower are deleted; the engine and `tutti-session` key a node by
`NodeKey` (`AudioNode` is `bevy-tutti`'s component, wrapping one), and
`tutti_types::latency` is a pure pass that is the compiler's own solve. A
block's tasks can run on a worker pool (below); a cost model decides when
that pays, and the compiler packs tasks too cheap to dispatch alone.

- Write nodes against `tutti_graph::Node` (`shape`, `prepare`, `process`,
  `reset`: declared latency and tail in the shape). A node with params is a
  `Node` + `ParamNode`, inserted through `param_parts`; a plain one is
  inserted `ForkByClone` or `Unforkable`. An internal filter that is never a
  graph node (the sampler's `stretch::Unit`) has plain methods, not a trait.
- Do not compare a render against another reference render in a new test:
  pin an analytic figure, an invariant of the render, or (for samples that
  call libm) a golden digest asserted only on the target it was recorded on.
- Musical delay is not latency. Report only processing latency to PDC.
- Transport commands meant for playback take an `At`
  (`MotionFsm::schedule`); the untimed `try_send` means `At::NextBlock`. A
  graph node that needs the transport at a frame reads `Env::transport_at`
  (the block's own time), or, behind a latent path, `Cx::transport_at` (the
  transport its input was rendered at): a start or seek inside a block is in
  `Env`, never a split block.
- Copying audio out of the graph is a **tap**, read through one `TapReader`
  (`TapIn` adapts it to `AudioIn`), and there are two kinds:
  - **The plan tap** (`Graph::open_tap`, `open_output_tap`): `Topology.taps`,
    one `Op::Tap` per tap in the plan, any port and width, aligned by the
    compiler or raw, a `ringbuf` ring per lane written together (the lanes
    cannot tear), the writer riding the commit queue. A closed tap compiles to
    no op; a fork carries none. It sees the graph's signal **before the
    engine's declick fader** and the fold to the device width, so a
    transport-gated source steps at a stop.
  - **The heard tap** (`Session::open_heard_tap`, the default for recording
    what the output sounds like): `Engine` writes each chunk of its output
    buffer after the fader, every device channel, from `process` and
    `process_segment` (a device callback and an offline `render`, silence for
    a refused block); an export forks without the engine and never writes to
    it. One at a time; the writer reaches the audio thread over `ringbuf`
    queues and the one it replaces goes back to the control side
    (`tutti-engine/src/hearing.rs`).
  Both count what a slow reader loses (`TapReader::take_lost`, and
  `AudioIn::take_lost` through `Recorder::lost`).
- Doc 013 records the defects found and the types that address them. Update
  it when the code it describes or a decision changes.
- Plugin hosts (VST2, VST3, CLAP, AU) move to a pure `step` core, the
  `Loaded` typestate trait whose `activate` splits a plugin into a realtime
  `Processor` and a main-thread `Controller`, and `Lifecycle<L>` per
  [`docs/design/016-plugin-hosts.md`](docs/design/016-plugin-hosts.md).

## The parallel executor (doc 013)

A block's tasks may run on several threads (`Executor::set_pool`,
`tutti_graph::WorkerPool`), and the render must stay bit-identical to the
serial one.

- **A cheap node declares its cost** (`Shape::with_cost`: `Cost::Light`
  or `Moderate`); one that declares nothing is `Heavy`, worth a thread.
- **Nodes communicate only through ports.** Two nodes sharing mutable state
  outside the graph (one `Arc<Mutex<_>>`, a shared cursor) are unordered in
  parallel. Share through an edge, or make it one node.
- **Op code is written once**, generic over `AudioSlots`/`EventSlots`
  (`tutti-graph/src/slots.rs`). Do not add an op path that reaches the arena,
  the event slots, a unit, a ring or an output except through them or a
  claim: that is what keeps `tutti-graph` `#![forbid(unsafe_code)]`.
- **New lock-free or `unsafe` runtime structure goes in `tutti-rt`**, with
  a loom model beside `pos_ring_loom.rs` that runs the shipped code, and a
  mutation record in it. The one exception is the publish cell, which is its
  own crate, `crates/arc-lend` (`ArcLend`/`Loan`; its loom model is
  `arc-lend/tests/loom.rs`). It names nothing of tutti's; keep it that way.
- **Store-load pairs use `SeqCst` fences**, not `SeqCst` accesses (loom
  models fences exactly; see `tutti-rt/src/job_gate.rs`).
- **Nodes run inside `with_no_denormals`**: never make memory safety depend
  on an exact float result (its `# Soundness` docs say why).
- A parallel test compares against the **serial render, bit for bit**
  (`tests/parallel.rs`, the differential suite's `modes`), and a gate on the
  parallel path runs every participant inside `assert_no_alloc`, helpers
  included (`TestPool::no_alloc`, the `GatedPool` in `tutti-engine`'s `rt_no_alloc_engine`, `tutti-nodes`'s
  `rt_no_alloc_modulation` and `tutti-clap-host`'s `clap_thread_pool`).

## Commands

`just` wraps these; `just --list` shows them. `just ci` runs what CI runs.

```bash
cargo build
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt
scripts/check-canonical-paths.sh   # the import-path gate (also searches *.md)
scripts/check-submodule-pins.sh    # VST3 SDK pins, asserted by SHA
scripts/check-bevy-free.sh         # tutti-session reaches no Bevy and has no bevy feature
scripts/check-adapter.sh           # bevy-tutti composes nothing, holds no policy (clock reads, usize/Duration consts); tutti-session re-exports no crate
scripts/check-doc-features.sh      # rustdoc per feature for bevy-tutti, tutti-session, tutti
scripts/check-guards.sh            # every host callback runs under the panic guard
scripts/check-plugin-ffi.sh        # no CLAP/VST3 unsafe outside the -ffi crates; the forbids
scripts/check-layout.sh            # flat crates/ and testing/, no crates->testing edges, layering
scripts/check-range-types.sh       # no plain derived Deserialize on a private tuple field
just deny                          # RustSec advisories, yanked and unmaintained crates (deny.toml)
```

`just deny` needs `cargo install cargo-deny --version 0.20.2 --locked` (CI pins
the same version) and fails with that command when it is missing.

`just doc` runs rustdoc as CI does (`--document-private-items`, per feature,
and the docs.rs build on a dated nightly); `just check-loom` runs clippy under
`--cfg loom`.

- **`unsafe`:** a library with none carries `#![forbid(unsafe_code)]`. Plugin FFI
  `unsafe` lives only in the per-format `-ffi` crates (`tutti-clap-ffi` and
  `tutti-vst3-ffi`); host crates forbid `unsafe`
  (`scripts/check-plugin-ffi.sh`, design 016 §2.9).
- **Toolchain:** `rust-toolchain.toml` pins the stable every gate runs on,
  and `Cargo.lock` is committed (`--locked` everywhere). To bump, edit
  `rust-toolchain.toml`, run `just ci`, and fix the new lints in the same PR.

### Testing

**Use `cargo nextest run`, not `cargo test`.** The audio tests touch device
and global state; nextest isolates each test in its own process.

```bash
cargo nextest run --workspace
cargo nextest run -p tutti-engine -E 'test(my_test_name)'
cargo test --doc --workspace       # nextest does NOT run doctests, silently
just test-all                      # both
```

- **A test finds a repo file through `tutti_fixtures`, never a relative `..`
  path:** `asset("audio/stereo.flac")` and `repo_root()` for the repo's files
  (`scripts/check-layout.sh` greps for depth-dependent paths), and `cdylib`
  and `plugin_server` for the reference plugins and `plugin-server`, which it
  finds in the test binary's own profile directory (no build script or env
  var, so it holds under `CARGO_TARGET_DIR` and `--target`). It is a
  dev-dependency and absence is a failure, never a skip. A crate-local file
  (`tutti-rustysynth`'s `samples/`) is `CARGO_MANIFEST_DIR.join(..)` without
  `..`.
- **Build `plugin-server` first:** `cargo build -p tutti-plugin-server`.
  Without it, `tutti-plugin`'s `clap_pdc_alignment`, `clap_crash_recovery` and
  `real_stall_tests`, `tutti-session`'s `plugin` suite, `tutti`'s §5.2
  example (`plugin_midi_export`, `--features clap,export`), and `bevy-tutti`'s
  `plugin_capture`, `mod_plugin` and `export_fork` plugin tests and
  `engine::restart::tests::a_restart_to_another_callback_size_re_declares_a_plugins_latency`
  (`--features full,plugin`) fail with that exact command, and
  `tutti-plugin-contract`'s `server` suite (`--features server`) needs
  `--no-default-features --features clap,vst2,vst3`. They must fail
  rather than skip, and they cannot use a dev-dependency because that would
  be a cycle.
- **Feature-gated suites** run only where a feature is on. A workspace run
  enables just what some member's dependency edge turns on (for example
  `tutti-nodes/convolution`), not a crate's own optional features. The rest
  (plugin formats, `midi-hardware`, `clap-extras`, ...) are named in CI's
  `dark features` job and `just test-features`. Name the feature when testing
  one crate, and add a new gated suite to both lists, or it never runs.
  **`tutti-midi-hardware --features sequencer-tests` is the exception:** it runs
  `Devices` against the real ALSA sequencer, which needs the kernel's `snd-seq`
  module and a readable `/dev/snd/seq`. CI's runner kernel (Azure) ships
  neither the module nor a package for it, so CI only lints the feature and the
  suite runs locally (`just test-features`). It fails, never skips, where the
  sequencer is missing. Run it before changing `backend/alsa`.
- **Golden digests** go through `tutti_nodes::testing::assert_golden`: asserted
  on x86_64 with glibc 2.39 (`RECORDED_ARCH`, `RECORDED_GLIBC`, Ubuntu 24.04),
  skipped with a printed note on any other libm or architecture (nextest shows
  the note only under `--no-capture`). CI's Linux jobs set
  `TUTTI_GOLDEN=require`, so a runner image that moves glibc fails instead of
  skipping; an unknown value panics, and `record` is refused when `CI` is set. To re-record, bump
  `RECORDED_GLIBC` and the runner pin, run
  `TUTTI_GOLDEN=record cargo nextest run --workspace --no-capture`, and copy the
  `GOLDEN <what> <digest>` lines into the tests; only for an argued libm move.

### Setup

- **Submodules:** `git clone --recursive`, or
  `git submodule update --init --recursive`. The VST3 SDK (three Steinberg
  repos pinned at `v3.8.0_build_66` under `vendor/vst3-sdk/`) is
  otherwise empty. `git describe` reports `v3.7.3_build_20-NN`; that is
  expected, and the gate asserts SHAs.
- **Linux:** `tutti-cpal` needs ALSA headers (`libasound2-dev pkg-config` or
  `alsa-lib-devel pkgconf-pkg-config`). Without them the build fails in
  `alsa-sys`'s build script.
- **Bevy-free floor:** three engine crates have an optional, off-by-default
  `bevy` feature (`tutti-types`, `tutti-mod`, `tutti-sampler`); the
  rest have none, and `tutti-session`, `tutti-graph`, `tutti-ump`, `tutti-io`
  and `tutti-codec` must never grow one (`WaveAsset`, the Bevy wrapper of a
  decoded `Wave`, is `bevy-tutti`'s). `bevy-tutti` is the only crate that
  requires Bevy. Check a crate that has the feature with `cargo check -p <crate>` and
  `--features bevy`. `--no-default-features` is *not* this check.
- **Import paths:** import engine types from a crate's root or prelude, never
  through its modules. Only `check-canonical-paths.sh` enforces this.

### Check Windows before you push

No Windows machine is needed:

```bash
rustup target add x86_64-pc-windows-msvc
cargo clippy -p tutti-plugin --all-targets --target x86_64-pc-windows-msvc -- -D warnings
zig c++ -target x86_64-windows-gnu -std=c++17 -c -o /tmp/o.obj tu.cpp -I <sdk>
```

**`--all-targets` fails on Linux at criterion's `alloca` build script.**
cc picks gcc, and gcc cannot target MSVC; `--lib --tests` fails the same way,
because criterion is a dev-dependency. Give the build script clang and a
one-line stub for the one header it wants. Clippy never links, so the stub
only has to let the C compile, and every Rust target is still checked in full:

```bash
mkdir -p /tmp/winstub
printf '#include <stddef.h>\nvoid *_alloca(size_t);\n' > /tmp/winstub/malloc.h
CC_x86_64_pc_windows_msvc=clang AR_x86_64_pc_windows_msvc=llvm-ar \
  CFLAGS_x86_64_pc_windows_msvc=-I/tmp/winstub \
  cargo clippy -p tutti-plugin --all-targets --target x86_64-pc-windows-msvc -- -D warnings
```

`just check-windows` runs this over the workspace and over the plugin crates
with every format feature. It leaves out the members that cannot
cross-compile: `ogg_next_sys` (reached through `tutti-codec`'s `encode-vorbis`
feature, which `tutti-export` turns on) and `tutti-vst3-host`'s `conformance` probe need
real Windows C headers. `tutti-codec` itself is linted with its other formats
(`encode-wav,encode-aiff,encode-flac` and every `decode-*`). CI's `clippy (windows)` job
runs everything natively on a `windows-latest` runner. It only lints: no CI job
runs the tests on Windows.
**A cast from a vst3 SDK enum constant differs by platform.** The bindings
type those constants as `c_uint` on unix and `c_int` on Windows, so
`kFoo as i32` passes Linux clippy and fails `unnecessary_cast` on Windows.
Use `helpers::sdk_enum_i32`.

These catch compile errors only. `nm` the object to confirm the branch you
meant was taken. Windows behaviour that surprised us before (named-pipe
timeouts, the MSVC CRT environment, VST3 UID layouts, `USERPROFILE`, bundle
by-products) is in the engineering notes; read it before touching
`util::transport` or plugin loading.

## Workspace

One workspace at the repo root. The member list in `Cargo.toml` is the source
of truth.

```
crates/          everything shipped; directory name = package name; no nesting
  tutti          Bevy-free umbrella. Every item is a `pub use` (tests/no_logic.rs).
  bevy-tutti     Bevy umbrella + adapter; the only Bevy-mandatory member.
  tutti-session  Bevy-free API layer: the cross-crate composition both umbrellas use
                 (design 015: `Session`, `Graph`, `SessionBuilder`, `Device`).
  arc-lend       The publish cell (`ArcLend`/`Loan`): a general-purpose crate with
                 no dependencies, `publish = false`, re-exported by tutti-rt.
  tutti-rt       Real-time-safe shared-memory primitives for the audio thread:
                 `AudioThreadCell`, `RtVec`, the position ring, the sequence lock,
                 the parallel executor's claims and job gate, `with_no_denormals`,
                 and the `ArcLend`/`Loan`/`Reader` re-export. Holds the lock-free
                 and `unsafe` runtime structure, with every loom model; depends
                 on `arc-lend` alone, `publish = false`
  tutti-ump      the Universal MIDI Packet: `Ump`, `MidiGroup`, `MidiChannel`,
                 `CCNumber`, and `Message` with `decode` and `From<Message> for Ump`,
                 which hold every message's bit positions (`Ump::words` and
                 `is_note_off` read the type, status and velocity bits a queue
                 needs), and the MIDI 1.0 wire: `Voice1::bytes`, `System::bytes`,
                 `frame` and `Midi1Parser`, the status bytes the hosts use
                 (`tutti-midi-types`' `wire.rs` and `Midi1Splitter` hold their own
                 until VST2 and AU move); no dependencies but `bytemuck` (and
                 optional `serde`)
  tutti-midi-protocol  MIDI 1.0 to MIDI 2.0 translation (design 017 §4 and §6): the
                 stateless `widen`, `narrow` and `midi1` and the M2-115 scaling;
                 depends on `tutti-ump` alone
  tutti-midi-device  MIDI device detection core (design 017 §5): `DeviceId`, `OsChange`,
                 the pure `DeviceTable`; no OS, no thread, and no dependency but optional `serde`
  tutti-types    vocabulary only: units, channel layouts, io edge traits,
                 Topology; no FFT and no new algorithms. `#![forbid(unsafe_code)]`.
                 Downmix and sample conversion stay until a follow-up moves them
  tutti-codec    the audio file codecs. Out: `Format` (`Wav(Pcm)`, `Aiff(Pcm)`,
                 `Flac { depth, level }`, `Vorbis { quality }`), `Spec`, and `Encoder`
                 over a `Write + Seek` sink; the `Pcm` widths and their one
                 quantization. In: `Wave` (the resident sample buffer), `Wave::read`,
                 `Wave::read_metadata`, `FileIn` (the streaming `AudioIn`),
                 `DecodeError`, `can_decode`. Depends on `tutti-types` alone
  tutti-graph    doc 013: the `Node` contract, Topology->Plan compiler, serial
                 executor + reference interpreter, the `WorkerPool` and the
                 `PrerollTable`, `MAX_ROOT_CHANNELS`; `Engine` renders it
  tutti-dsp      pure algorithms over slices: the real FFT, analyzers, filters,
                 dynamics, convolver, delay lines, waveshapers
  tutti-mod      modulation compiler
  tutti-transport `Transport`, the motion state machine, timed commands, the
                 offline timeline, `ProjectMeter`
  tutti-metering `MasterMeter`, `LoadMeter`
  tutti-tempo    `TempoMap`: step and ramp tempo segments, beat <-> seconds <-> frame
                 conversion in `f64`, and constant-tempo cells for a ramp
  tutti-engine   `Engine`, the per-block graph render, over a transport and
                 the meters (the metronome node, `ClickNode`, is `tutti-nodes`')
  tutti-io       live I/O edge: mic monitor, `TapIn`, `WavOut`, `Recorder`
  tutti-cpal     device
  tutti-export   offline render
  tutti-nodes, tutti-spatial (vbap, hrtf), tutti-sampler, tutti-polysynth,
  tutti-soundfont, tutti-rustysynth (our fork of rustysynth)
  tutti-midi-{types,runtime,hardware,file}, tutti-alsa-ump-sys
                 (raw ALSA UMP FFI that alsa-sys lacks; Linux only)
  tutti-plugin{,-types,-db,-server}, tutti-{vst2,vst3,clap,au}-host,
  tutti-clap-ffi (all CLAP unsafe), tutti-vst3-ffi (all VST3 unsafe),
  vst-tutti (our fork of the vst crate; its split is a later lane)
testing/         never shipped
  tutti-clap-test-plugin, tutti-vst2-test-plugin, tutti-fixtures,
  tutti-shared-plugin (the test plugin: a format-free core, a shell per
  format), tutti-plugin-contract (the cases every format's host runs against
  it), tutti-shm-model (loom), arc-lend-compare
vendor/          third-party code we never edit
  vst3-sdk/      the three Steinberg submodules
```

**Layering.** Dependencies between workspace crates point strictly down this
list (normal and build edges; dev-dependencies may go anywhere).
`scripts/check-layout.sh` holds the table and fails on an unplaced crate:

0. `arc-lend`, `vst-tutti`, `tutti-rustysynth`, `tutti-alsa-ump-sys`, `tutti-ump`
1. `tutti-types`, `tutti-rt`, `tutti-midi-protocol`, `tutti-midi-device`
2. `tutti-graph`, `tutti-dsp`, `tutti-mod`, `tutti-midi-types`, `tutti-metering`,
   `tutti-codec`, `tutti-tempo`
3. `tutti-transport`, `tutti-plugin-types`
4. `tutti-engine`, `tutti-io`, `tutti-nodes`, `tutti-midi-runtime`, `tutti-midi-file`,
   `tutti-clap-ffi`, `tutti-vst3-ffi`, `tutti-au-host`, `tutti-vst2-host`,
   `tutti-plugin-db`
5. `tutti-cpal`, `tutti-export`, `tutti-spatial`, `tutti-sampler`,
   `tutti-polysynth`, `tutti-soundfont`, `tutti-midi-hardware`,
   `tutti-clap-host`, `tutti-vst3-host`
6. `tutti-plugin`
7. `tutti-plugin-server`, `tutti-session`
8. `tutti`, `bevy-tutti`

- A member is directly `crates/<package-name>` or `testing/<dir>`; a nested
  directory is refused. Nothing under `crates/` has a normal or build
  dependency on `testing/`; a test reaches `testing/` through a
  dev-dependency.
- `vendor/` holds third-party code we never edit and no workspace member.
  Our own forks (`tutti-rustysynth`, `vst-tutti`) are crates, not vendored.
- Only `tutti` and `bevy-tutti` depend on `tutti-session`; nothing depends on
  them.

- `tutti-ump` depends on no workspace crate, keeps every module private
  (`tutti_ump::Ump`; CI runs the per-module path gate on it), is
  `#![forbid(unsafe_code)]` and has no `bevy` feature. A consumer names
  `tutti_ump::{MidiGroup, MidiChannel, CCNumber}`, never `tutti_types::`. The
  `tutti` and `bevy-tutti` preludes export all three, and `tutti-session`
  exports `MidiChannel` behind `midi`. A consumer that persists one with serde
  depends on `tutti-ump` with its `serde` feature; no umbrella forwards it.
  `MidiGroup::new`, `MidiChannel::new` and `CCNumber::new` return `Option` and
  refuse out of range, like the field types. `of` is for a literal or a
  constant (it panics out of range), and `masked` is the one constructor that
  masks, for bits the wire has just framed.
- `tutti-midi-protocol` depends on `tutti-ump` alone, keeps every module
  private (`tutti_midi_protocol::widen`; CI runs the per-module path gate on
  it) and is `#![forbid(unsafe_code)]`. It holds no state and allocates
  nothing. A consumer's call to `widen` is transitional: the stage 3 translator
  calls it at the input edge and consumers drop theirs.
- `tutti-midi-device` depends on no workspace crate, keeps every module private
  (`tutti_midi_device::DeviceTable`; CI runs the per-module path gate on it) and
  is `#![forbid(unsafe_code)]`. It holds no OS handle, thread or clock: a backend
  feeds `DeviceTable::apply` an `OsChange`, and `tutti-midi-hardware` owns the
  threads and the OS. A consumer that persists a `DeviceInfo` depends on it with
  its `serde` feature (no umbrella forwards it; renaming an `Identity` variant
  breaks saved files) and restores it with `Devices::resolve`. Only `Exact`
  is safe to bind unasked: an ALSA identity is a name, so a lone device with
  the saved name is `SameName` (bind or ask), and an ordinal above 0 or several
  alike devices are `Ambiguous`. Comparing a saved id with a listed one by hand
  can bind another keyboard of the same model.
- `tutti-transport`, `tutti-metering` and `tutti-engine` keep every module
  private (`tutti_transport::Transport`, never `tutti_transport::handle::Transport`;
  CI runs the per-module path gate on each), are `publish = false`,
  `#![forbid(unsafe_code)]` and have no `bevy` feature. `tutti-metering`
  depends on `tutti-types` alone; `tutti-transport` on `tutti-types`,
  `tutti-rt` and `tutti-graph`; `tutti-engine` on those and the two crates
  above, with a dev edge to `tutti-nodes` for the `ClickNode` tests.
  `TransportAudio` (`Transport::audio`, taken once) is the audio side the
  engine drives across the crate edge; the clock, control inputs and pending
  list behind it are crate-private.
- `tutti-tempo` depends on `tutti-types` alone (and `libm`, so a ramp's figures
  do not depend on the platform's libm), keeps every module private
  (`tutti_tempo::TempoMap`; CI runs the per-module path gate on it), is
  `#![forbid(unsafe_code)]` and has no `bevy` feature; `serde` is optional and
  only on `CellGrid`. A `TempoMap` is an immutable value built on the control
  side: its constructors refuse a tempo that is not finite and positive
  themselves, whatever `Bpm` accepts, and no query allocates. Seconds are `f64`
  (`Seconds` is `f32`). A map reaches a constant-tempo reader as `Cell`s on one
  fixed `CellGrid`, also cut at every point's frame (`TempoMap::next_change`);
  `chord_cell` is a pure function of the absolute frame, a cell inside a step
  has that step's tempo to the bit, and the crate derives no grid from a block.
- `tutti-types` has no workspace dependency, is `#![forbid(unsafe_code)]`, and
  nothing audio-specific belongs in it beyond its vocabulary.
- `tutti-codec` depends on `tutti-types` alone, keeps every module private
  (`tutti_codec::Encoder`; CI runs the per-module path gate on it), is
  `#![forbid(unsafe_code)]`, is `publish = false` and has no `bevy` feature.
  Each format is a cargo feature, none on by default: `encode-wav`,
  `encode-aiff`, `encode-flac`, `encode-vorbis` for writing and `decode-wav`,
  `decode-flac`, `decode-mp3`, `decode-ogg` for reading. A decode feature pulls
  exactly one `symphonia` container and no encoder library, an encode feature
  pulls no decoder, and the C Vorbis library is `encode-vorbis`'s alone, so a
  build that only reads never needs it (`scripts/check-layout.sh` holds both).
  A `Format` variant exists only with its `encode-*` feature: no `cfg(not)`
  arm refuses a format. An `Encoder` takes blocks already at the
  `Spec`'s rate and width; it does not pull, resample or dither, and nothing in
  the crate stops a caller from handing it blocks that were not. Pulling,
  resampling and dithering are the pipeline's, and `tutti-export` has one path
  through its own (`pump_blocks`, private). `write` is fallible; the file
  `create_file` makes is removed unless `finish` moved it into place (and
  `finish` after a failed `write` removes it and returns the first error). The
  sink is `Write + Seek + Send + 'static` on `Encoder` whatever the features,
  because bounds that followed the features would be worse; a `FormatWriter`
  asks only for what its own format needs. The
  WAV writer is one: `tutti-export` and `tutti-io`'s `WavOut` both write
  through it. Each format's writer implements `FormatWriter` (its boundary: it writes
  blocks of interleaved samples into a sink as one file of its format), with
  the sink bounds on the `impl`, not the trait; `Encoder` is the one type over
  them. A new format is a `Format` variant, a writer type, a `contract!` line in
  `src/contract.rs` and a row in `tests/bytes.rs`.
  Decoding has no trait: it is plain functions and types of our own over
  `symphonia`, all in `src/decode.rs` and `src/decode/`, so the library is
  swapped by editing that module. No `symphonia` type is in a public signature,
  field or error (`DecodeError` is ours), and no file outside that module names
  it (`scripts/check-layout.sh`). `Wave` is here because decoding produces it and
  `tutti-sampler`, `tutti-export` and `bevy-tutti` consume it; it needs no
  feature. `FileIn` is here because it is the decoder's streaming state.
- `tutti-rt` names no tutti crate. Its one workspace-member dependency is
  `arc-lend`, which depends on nothing at all. It holds the engine's lock-free and
  `unsafe` runtime structure (apart from `arc-lend` and the `-ffi` crates). Keep
  every module private (`tutti_rt::RtVec`, never `tutti_rt::vec::RtVec`; CI runs
  the per-module path gate on it).
- `tutti-dsp` depends on no tutti crate but `tutti-types` (its outside deps are
  rustfft, ebur128, libm and thiserror; `tutti-graph` is a dev-dependency only), keeps every
  module private (`tutti_dsp::RealFft`, never `tutti_dsp::fft::RealFft`; CI runs
  the per-module path gate on it), is `#![forbid(unsafe_code)]` and has no
  graph or runtime, no `Arc`-published state and no atomics for control: every
  control arrives per call as a plain typed value. It holds the real FFT, the
  analyzers, the SVF, ladder and Linkwitz-Riley filters (and their `Real`
  float), the dynamics curves, followers and kernels, the partitioned
  convolver, the delay line and swept-delay kernel, the cubic Hermite kernel, the phaser's all-pass chain, the
  block ramps, the export resampler, the
  ring buffers and the waveshaping curves. `tutti-nodes` holds the `tutti_graph::Node` adapters over
  them (shape, prepare, process, params, fork), and re-exports only the
  algorithm types that sit in a node's signature (`SvfType`, `LadderType`,
  `Real`, `CrossoverError`, `InterpolationMode`, `ShapeKind`).
- **A pure algorithm over slices goes in `tutti-dsp`; a node file holds only
  the `Node` adapter.** Where an algorithm needs a `Param<U>` read, an atomic or
  a non-finite guard (`LastGood`), the node reads and guards it and hands the
  algorithm plain values (`CompressorKernel::gain` takes a `CompressorBlock`).
  A loop that indexes the graph's `Inputs`/`Outputs`, or reads a `Param` cell,
  stays in the node.
- `tutti-graph` depends on no tutti crate but `tutti-types`, `tutti-rt` and `tutti-ump` (its outside
  deps are `bytemuck` and `ringbuf`, the editor→executor queue), keeps every
  module private (`tutti_graph::Plan`, never `tutti_graph::plan::Plan`; CI runs
  the per-module path gate on it) and is `#![forbid(unsafe_code)]`.
- `tutti-session` keeps every module private too (`tutti_session::Graph`;
  CI runs the per-module path gate on it), has no `bevy` feature and
  re-exports no crate whole (`scripts/check-adapter.sh`).
- `tutti-plugin-db` depends on `tutti-plugin-types` alone among workspace
  crates, keeps every module private (`tutti_plugin_db::Scanner`; CI runs
  the per-module path gate on it), is `#![forbid(unsafe_code)]` and has no
  `bevy` feature. It holds plugin discovery (the directory walk, the scanner,
  the pedal, and the file table `PluginDb` with its versioned file) and never
  loads a plugin: the probe is a parameter, and `tutti-plugin` supplies the
  out-of-process one. The table has one owning thread; a scan works on a
  snapshot and sends `Delta`s, and `PluginDb::apply` is the only code that
  changes a row. Consumers
  name its types from it, not through `tutti-plugin`.
- `tutti-spatial` depends on `tutti-nodes`, never the reverse.
- `tutti-midi-file` is OS-free.
- `tutti-rustysynth` (our fork) is excluded from clippy and rustdoc.
- A Bevy app depends on `bevy-tutti`; a headless consumer depends on `tutti`.
  `bevy-tutti` does not depend on `tutti`. Both use `tutti-session`. It
  holds only code that names two or more engine crates, or the order they
  run in, has no `bevy` feature (`scripts/check-bevy-free.sh`) and
  re-exports no crate whole. `bevy-tutti` reaches the engine's composition
  only through it, reads no clock but as a session phase's argument and holds
  no `usize`/`Duration` limit const (`scripts/check-adapter.sh`, whose short
  exception list names the lane that removes each), and does not re-export it.

## Unit types (MANDATORY)

Use the `tutti_types::value::units` newtype, never a bare float, for any
quantity one covers: fields, signatures, `Param<U>` and return types.

- **Operators are opt-in per type, and every omission is deliberate.** Read
  the omission ledgers in `value/units/`'s tests (one per family) before
  adding one; a unit's `unit!` ability list is its whole algebra. If it is
  listed there, write a named method instead.
- **An omission must ship with its replacement in the same change.**
- **Convert with the named converters** (`Db::to_amplitude`,
  `Seconds::to_samples{,_floor,_ceil}`, `Cents::to_semitones`), never by hand.
- **Measurements are not controls.** `Confidence`, `Correlation` and `Pan`
  are readings. Do not swap them for `Mix` or `Depth`.
- **Where the types stop:** C ABI, IPC and WIT boundaries, and precision the
  unit cannot carry (`Seconds` is f32, so SMPTE and hour-long durations stay
  f64). Say why in a comment.
- **Adding a unit:** only with a distinct range *or* a distinct algebra. Two
  names for the same behaviour is not a type.

## Range-checked values

A type that refuses some raws of its representation is hand-written to one
shape (`MidiChannel` is the reference). No macro.

| Item | Rule |
|---|---|
| `new(raw) -> Option<Self>` | Refuses what the type cannot hold; never clamps, wraps or rounds. `const fn` when the check is const. |
| `of(raw) -> Self` | `const fn` that panics, for a literal or a constant. Omit it when named constants cover the literals. |
| `get(self) -> Raw`, `From<Self> for Raw` | Always. |
| `TryFrom<Raw>` | Agrees with `new`; its error carries the refused raw. |
| serde | `serde(try_from = "Raw", into = "Raw")` or a hand impl through `TryFrom`; never a plain derive on the private field. |
| `clamped(raw)` / `nearest(raw)` | Saturation, named, only where a hot path or a lenient import needs it. A float maps NaN to a documented value. |
| `masked(raw)` | Only for bits the wire has just framed. |
| `Default` | Only if the default is in range. |

- No public field, no infallible `From<Raw>`, no `unsafe` constructor.
- A type whose range is the whole raw (`ChannelLayout`, the RPN word of
  `PitchBendSensitivity`) has no `new -> Option`, no `TryFrom` and no refusing
  serde; its plain derived serde is on the lint's exception list by name.
- A type with two raws (whole units and wire bits) says in its docs which raw
  `new`, `of` and serde each take (`PitchBendSensitivity`: `new` and `of` take
  whole semitones, serde the bits) and gives the other a named constructor
  (`from_rpn_bits`).
- `BeatsPerBar` and `NoteValue` keep the shape: `new` refuses, `clamped`
  (`BeatsPerBar`) and `nearest` (`NoteValue`, which rounds up to the next power
  of two, not to the closer one) saturate, and serde refuses.
- **A new type is checked by `tutti_fixtures::check_range_contract`** in a
  test (valid and invalid raws through `new`, `TryFrom`, `of` and serde), and
  `scripts/check-range-types.sh` fails a private tuple field with a derived
  `Deserialize` that is not `serde(try_from)` (`transparent` and `from` are
  flagged too). Its exception list names what removes each entry.
- `tutti-ump` does not dev-depend on `tutti-fixtures` (`check-layout.sh`: it
  depends on no workspace crate, of any kind), so its types keep their own
  tests.

## Publishing to the audio thread (MANDATORY)

Non-scalar state handed to the audio thread goes through
`tutti_rt::{ArcLend, Reader, Loan}` (the `arc-lend` crate, re-exported).
Scalars use `Param<U>`.

- **The audio thread never holds an owning handle to published state, and
  never frees it.** A reading thread lends through a `Reader`;
  `Reader::lend` returns a `Loan`, a `!Send` borrow, and is wait-free.
  `publish()` is control-thread only and frees retired values there. This is
  structural (one slot per registered reader, and a retirement list only the
  publisher touches), and the loom model `arc-lend/tests/loom.rs` checks it.
- **Register on the control side, where the reader is built**
  (`ArcLend::reader`, from an `Arc` of the cell: it takes the publisher's
  lock and may allocate), and store the `Reader` in the node or structure
  that reads. A fork builds over a new cell and registers there.
  `Reader::clone` registers a new reader on the same cell.
- Lend once per block, never per sample. Never park a `Loan` across blocks
  (it pins the value it holds). One `Reader` per nesting level: `lend` takes
  `&mut self`.
- **The design's cost is memory, not waiting:** a publish never waits on a
  reader, and a value a block holds when it is replaced (or one a reader
  preempted mid-lend might load) stays retired until the next scan after
  the block. **Every cell's publisher calls `reclaim()` on a control-side
  tick it already has**, so a cell that goes quiet still gets that scan:
  the session's `Phase::Reclaim` (which `bevy-tutti`'s `reclaim_graph`
  runs, and nothing more), the butler's cycle (its rings' maps). The
  session's sweep is decided in `tutti-session` alone: a node whose
  controls publish returns a `Reclaimer` from `SessionNode::reclaimer`,
  which the graph keeps by node from `add`/`crossfade` until the node goes;
  the graph sweeps its hosted plugins' meters; the session sweeps its own
  cells (project meter, modulation lanes, hardware ports). A
  new cell joins its owner's sweep in the same change. `reclaim()` is a
  lock and a length check when nothing is retired; it is control-thread
  only, like `publish`.
- Never `publish` from the audio thread.
- Do not try to prove the race with a no-alloc test. The loom model and miri
  cover it; a single-threaded gate can only pin the reader's code path.
- Nullable hot-swap slots go through `ArcLend` too, as
  `ArcLend<Option<Arc<_>>>` (the plugin nodes' `PluginMeter`). Nothing the
  audio thread reads is an `ArcSwap`: a store settles a held guard's debt
  into a strong count, and the guard's drop can then free on the audio thread.
  Control-thread-only cells stay plain. (The sampler's `SharedReader` is a
  plain `Arc` of a position-indexed ring of atomics; nothing swaps it.)

## Buffers: edges vs graph nodes

- **`AudioIn` / `AudioOut`** (`tutti-types::io`) are engine **edges** (mic,
  WAV, `TapIn`, `FileIn`, the sampler's butler ring). They take flat
  interleaved `&[S]` with a runtime `ChannelLayout`. **Every count on this
  boundary is in frames**, typed as `Samples` (the frame count). Cross to a
  slice length only with `Samples::interleaved_len` /
  `Samples::from_interleaved_len`.
- **`tutti_graph::Node::process`** is for anything that is a node in the
  graph (including sampler voices): planar `f32` through `Io`, any block
  length up to `Prepare::max_block`, the transport from `Env`.
- **Width is runtime.** Do not reintroduce a const-generic channel count. The
  two runtime width checks that replaced it (`pump`'s `debug_assert` and
  `Recorder::start`'s error) are mandatory.
- **Every `AudioIn` impl must state `ON_EMPTY` correctly** (live "not yet" vs
  finite "never again"). Getting it wrong ends a recording with no error.
- **A new trait must state its boundary in one sentence without "and".**

## Wiring is declared, not called

`bevy-tutti` runs on `tutti-graph` only (doc 013, "Flip the adapter").

- `spawn_audio_node` adds an *unwired* node. `PortSources` on a sink and the
  `MasterSources` resource declare what feeds each port. The rebuild builds a
  `Topology` value from them (`LiveGraph` holds the last one), writes the
  declared ports that differ into the session graph's spec, and the frame's
  commit compiles it iff the graph was edited (`Graph::is_edited`); PDC is
  the compiler's, and nothing is spliced into the graph.
- A headless `Session` declares through its own spec, keyed by
  `NodeKey`, and `Session::update()` commits iff the graph was edited.
- A declaration may be partial: a port a short `PortSources` leaves out, and
  every output channel while `MasterSources` is empty, belong to whoever
  wires them through `AudioGraphRes`. The rebuild's debug check
  (`topology::disagreements`) compares only declared ports; never compare a
  whole-graph fold (a latency plan) against the value.
- A declaration names an *entity*, the graph keys a `NodeKey`, which `bevy-tutti`'s `AudioNode` component holds; re-binding
  an entity to a new node re-derives its wires (`Changed<AudioNode>`).
- Keys are sink ports, so fan-in cannot be represented. Summing is a node's
  job.
- `GraphReconcileSystems`: `Spawn → Params → Despawn → Compensate → Commit`.
  The session's phases run in `SessionPhaseSystems(Phase)`, chained from
  `PHASES` (`Collect` before `Spawn`, `Commit` in `Commit`); schedule a
  system that runs a phase in its set. Node removal is an
  `On<Remove, AudioNode>` observer.
- Every spawn path takes a `SessionNode`: `spawn_audio_node`,
  `insert_audio_node`, `crossfade_audio_node` (and `Graph::insert` /
  `replace`, reached through `AudioGraphRes::graph`, take its `IntoNode`). A host's plain node goes in as
  `ForkByClone(node)` or `Unforkable(node)`; its own `ParamNode` is
  registered in one line with `bevy_tutti::param_graph_node!(MyNode)`.
- Params are `AudioParam<U, const P: u16>`, registered with
  `App::add_audio_param::<U, P>()`. `P` is a packed `UnitAddr` (a
  `UnitParam` kind in the low byte, an index among the node's params of
  that kind in the high byte; index 0 is the plain `UnitParam as u16`, so
  `{ UnitParam::Cutoff as u16 }` still names a node's only cutoff). They reach
  a node through its `ParamSet` (`SessionNode::params`): a node without the
  param takes nothing. A node with several params of a kind adds each with
  `ParamSetBuilder::param` and declares each modulatable with
  `Shape::with_param_addrs`; a route names one with `ParamRef::unit_at`.
  Modulation is declared too (`Mods`, `ModRoute`, `ModRange` naming a
  `ParamRef`): each modulated param gets a control lane on its port, whose
  knob is the node's own cell (a plugin parameter's lane, its own knob on
  the plugin's event input). One writer per knob: `write_param` for a
  native one, the plugin's GUI or `AudioGraphRes::modulation().set_knob`
  for a plugin's.
  `ModulationSystems::Apply` runs between `GraphReconcileSystems::Spawn` and
  `Params`; `ModulationSystems::AfterCommit` runs after `Commit`.

## Testing policy

- **Never simplify, skip or modify tests to make them pass.** Fix the bug.
- **Report failing tests and their root causes.** Never silently remove one.
- **Mutation-test new tests:** break the property, watch the test fail,
  restore it, and write the mutation in a comment. If a property cannot be
  covered here, say so in a comment rather than asserting something that
  always passes.

## The plugin contract

`testing/tutti-plugin-contract` runs the same cases against each format's host
through `Loaded`, `Processor`, `Controller` and `Control`, with the one test
plugin `testing/tutti-shared-plugin` (a format-free `core` and a shell per
format: CLAP, VST2 and VST3, each in `tutti-shared-plugin/src/<format>.rs`).
A format is a `Format` impl and `contract!(Fmt, gaps = N)`. The C++ plugin
`testing/tutti-shared-plugin-vst3-cpp` is the same spec on the Steinberg SDK
classes, a separate component and controller, and runs the same cases as the
`Vst3Cpp` format: the Rust VST3 shell shares the `vst3` crate with the host, so
only the C++ one checks the bindings. `ParamTiming` and `Gap` are a format's
only ways out of a case.

- **The expected figures live in the contract's `spec`**, never imported from
  the plugin: a case that asked the plugin could not fail when it is wrong.
- **The core records what the host sent before it orders or repairs
  anything.** A core that repairs first hides the host bug it should show.
- **The plugin is configured and read through its state** (a mode word in the
  blob, the transcript behind it): no environment variable, symbol or process
  global, so the same calls work through the plugin-server.
- **A case a format cannot pass is a `Gap { why, fails_with }`**: the case
  runs and must fail with that text, so a stale gap fails. `gaps = N` in the
  test file is the ratchet; raising it is a reviewed edit. Never `#[ignore]`.
- **A new case is mutation-tested**: break the property in the host or the
  plugin, watch it fail, restore, and write the mutation on the case.
- A plugin that cannot share the core (the C++ VST3 plugin, an in-process AU
  unit) follows the spec in `core`'s docs (and `core.hpp` mirrors it) and runs
  the same cases. A change to the spec changes both cores.
- The VST2 shell uses `vst-tutti`'s authoring side as a dependency; when that
  half moves out of `vst-tutti`, the shell's one dependency line changes.

## Check the constraint before designing around it

Before working around an obstacle, name it in one sentence and verify it:
read the `Cargo.toml`, grep for the type, run the probe.

- An existing dependency edge is not evidence it points the right way.
- An indirection whose only job is to avoid a change is the finding, not the
  design. Make the change.
- When the constraint is real, derive at the point of use rather than mirror
  and reconcile.

## Rules that came from bugs

- **One bundle walk:** `tutti_plugin_types::bundle` is the only code that
  knows the `Contents/<arch>/` layout.
  - `native_module_in_bundle` is for hosts.
  - `any_module_in_bundle` is for corpus scans.
  - Never write another copy.
- **VST3 UIDs:** build them with `inline_uid(l1, l2, l3, l4)`, never a
  hardcoded `[u8; 16]`.
- **Neither umbrella holds engine logic.** A piece with one owner goes to
  that crate; a piece composing several goes to `tutti-session`.
- **The engine must not reference `dawai`.** The provenance note in
  `bevy-tutti/Cargo.toml` is the one exception. The `docs/` audits are dated
  records: fix only what claims to be current, and never "update" a reference
  to a new app-side path.
- **Dead crate names:** `tutti-units` → `tutti-nodes`, `tutti-synth` →
  `tutti-polysynth` + `tutti-soundfont`, `tutti-midi` → the four `midi/`
  crates, `tutti-analysis` → `tutti-dsp`, `tutti-core` → `tutti-transport`
  (`Transport`, `ProjectMeter`, the clocks), `tutti-metering` (`MasterMeter`,
  `LoadMeter`) and `tutti-engine` (`Engine`; `MAX_ROOT_CHANNELS`
  is `tutti-graph`'s), `tutti-node` and `fundsp-tutti` →
  deleted (the node contract is `tutti_graph::Node`; the filters' `Real` is
  `tutti-nodes`'), and tutti-mod's runtime (`ModMatrix`, `ModParams`, `ParamFrom`,
  `modulation::legacy`) → deleted (doc 014 §10: `tutti_mod::compile` and
  a `ControlLaneNode` per param). A comment or doc that disagrees with the
  code is the bug.
- **MSVC render gate:** `render_is_bit_identical_to_the_audionode_era` is
  gated off MSVC, because `sin` differs in the last ulp between CRTs.

## Consumers: two pins that must stay in step

Consumers pin this repo as a git dependency. Keep these in step with them;
both are in `[workspace.dependencies]`:

- **`audio-automation`'s rev.** A mismatch surfaces as
  `expected Curve, found Curve`.
- **The bevy 0.19 line**, for anything that touches `bevy-tutti`.
