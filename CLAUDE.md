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
(parked, below) only runtime (`AudioGraphRes` wraps a `tutti_session::Session`, whose graph
holds the `Editor`; PDC is the compiler's, export forks the live graph with
`Editor::fork`), and every node is a `tutti_graph::Node`. fundsp's `Net`,
the vendored fork, `tutti-node` and its `AudioUnit` trait, `Setting` and
the numeric tower are deleted; the engine and `tutti-session` key a node by
`NodeKey` (`AudioNode` is `bevy-tutti`'s component, wrapping one), and
`Topology` and its passes live here (`tutti_graph::delays` is a pure pass and the
compiler's own solve, one copy; `latency_plan` and `graph_tail` fold the same value). A block's tasks can run on a worker pool (below); a cost model decides when
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
- `Shape::tail` is a node's **ceiling**. A node whose ring-out follows its
  params narrows it each block with `Io::report_tail` (silence skip only; the
  plan, PDC and export read the ceiling); the report must cover the ring-out
  and err long, measured from the last non-quiet input (not from the call);
  only a node without event inputs is parked on it (doc 013, "A tail excludes
  latency"; `contract::assert_honest_tail` checks it).
- Transport commands meant for playback take an `At`
  (`MotionFsm::schedule`); the untimed `try_send` means `At::NextBlock`. A
  graph node that needs the transport at a frame reads `Env::transport_at`
  (the block's own time), or, behind a latent path, `Cx::transport_at` (the
  transport its input was rendered at): a start or seek inside a block is in
  `Env`, never a split block.
- Copying audio out of the graph is a **tap**, read through one `TapReader`
  (it is a `Source` as it is: `impl Source for TapReader`), and there are two
  kinds:
  - **The plan tap** (`Graph::open_tap`, `open_output_tap`; opening refuses a
    source that does not exist then with `MissingSource`, and a lane whose port
    goes missing later reads silence, never failing a commit): `Topology.taps`,
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
  `Source::take_lost` through `Recorder::lost`).
- Copying audio into the graph is an **inlet**, the inverse of the tap:
  `InletNode` (`tutti-graph`) copies a standalone ring (`TapReader::standalone`)
  a thread of your own fills (a capture callback) into the graph, and
  `Graph::open_inlet` / `add_inlet` (`tutti-session`) refuse a producer whose
  rate is not the graph's with `InletError::RateMismatch`. An inlet does not
  resample and two clocks drift; what fills or empties the ring is counted,
  never blocked on. Gain is a node downstream, not the inlet's.
- Doc 013 records the defects found and the types that address them. Update
  it when the code it describes or a decision changes.
- **Main-thread entries take a token.** An editor, timer, run-loop, state or
  controller call a format requires on the UI thread takes
  `&tutti_plugin_types::MainThread` (`!Send`, from `MainThread::claim()`, once
  per process; `MainThread::current()` for a `Drop`). Holding one is the proof:
  no `debug_assert` and no marked thread (doc 016 §2.9a). Bevy holds it as the
  `NonSend` resource `PluginEditorMainThread`. AU also asks the OS.
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
- **Level metering is an op in the plan** (`Editor::open_meter`, doc 013
  "Meters"), not an engine pass: per channel, on the outputs or any node
  port, before the engine's fold and declick; the master is aligned to the
  outputs' time. Each lane is a `tutti_rt::level_cell` (two slots and a
  flip, with a loom model), so a block lands in one read, peak and energy
  together; a read takes what was held since the last read, so a reader is
  one consumer (`&mut self`, not `Clone`) and a second consumer opens a
  second meter. Opening refuses a source that does not exist then (`MissingSource`); a lane whose port goes missing later reads silence.
  Ballistics are the reader's.
  `LoadMeter` stays in `Engine`: it times the block and is not audio.
- **Nodes run inside `with_no_denormals`**: never make memory safety depend
  on an exact float result (its `# Soundness` docs say why).
- A parallel test compares against the **serial render, bit for bit**
  (`tests/parallel.rs`, the differential suite's `modes`), and a gate on the
  parallel path runs every participant inside `assert_no_alloc`, helpers
  included (`TestPool::no_alloc`, the `GatedPool` in `tutti-engine`'s `rt_no_alloc_engine`, `tutti-nodes`'s
  `rt_no_alloc_modulation` and `tutti-clap-host`'s `clap_thread_pool`).

## Commands

`cargo xtask <recipe>` wraps these (the `xtask` crate; `.cargo/config.toml` aliases it);
`cargo xtask help` lists them, and `cargo xtask --dry-run <recipe>` prints what a recipe would
run. `cargo xtask ci` runs what CI runs.

```bash
cargo build
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt
cargo xtask check-paths [<crate> <module>...]  # the import-path gate (also searches *.md)
cargo xtask check-submodule-pins   # VST3 SDK pins, asserted by SHA
scripts/check-bevy-free.sh         # tutti-session reaches no Bevy and has no bevy feature
scripts/check-adapter.sh           # tutti-session re-exports no crate whole (bevy-tutti's composition/policy scans are skipped while it is parked)
cargo xtask check-doc-features     # rustdoc per feature for tutti-session, tutti
scripts/check-guards.sh            # every host callback runs under the panic guard
scripts/check-plugin-ffi.sh        # no CLAP/VST3/AU unsafe outside the -ffi crates; the forbids; VST2 held to its pins
scripts/check-layout.sh            # flat crates/ and testing/, no crates->testing edges, layering
cargo xtask check-range-types      # no plain derived Deserialize on a private tuple field
scripts/check-hosting.sh           # every InProcess/OutOfProcess impl is in the contract's hostings
cargo xtask deny                   # RustSec advisories, yanked and unmaintained crates (deny.toml)
```

`check-paths`, `check-submodule-pins`, `check-doc-features` and `check-range-types` are
Rust gates in `xtask/src/gates/` (a pure function from the files, read through `Tree`,
to a `Report`, with tests over in-memory trees); the `scripts/` ones are still scripts.

`cargo xtask deny` needs `cargo install cargo-deny --version 0.20.2 --locked` (CI pins
the same version) and fails with that command when it is missing.

`cargo xtask doc` runs rustdoc as CI does (`--document-private-items`, per feature,
and the docs.rs build on a dated nightly); `cargo xtask check-loom` runs clippy under
`--cfg loom`.

- **`unsafe`:** a library with none carries `#![forbid(unsafe_code)]`. Plugin FFI
  `unsafe` lives only in the per-format `-ffi` crates (`tutti-clap-ffi`,
  `tutti-vst3-ffi`, `tutti-au-ffi` and `tutti-vst2-ffi`); host crates forbid
  `unsafe` (`scripts/check-plugin-ffi.sh`, design 016 §2.9). The in-process VST2
  backend's `WindowHandle` decode (the raw pointer of `HostEditor::open_editor`) is the
  one caller still pinned.
- **Toolchain:** `rust-toolchain.toml` pins the stable every gate runs on,
  and `Cargo.lock` is committed (`--locked` everywhere). To bump, edit
  `rust-toolchain.toml`, run `cargo xtask ci`, and fix the new lints in the same PR.

### Testing

**Use `cargo nextest run`, not `cargo test`.** The audio tests touch device
and global state; nextest isolates each test in its own process.

```bash
cargo nextest run --workspace
cargo nextest run -p tutti-engine -E 'test(my_test_name)'
cargo test --doc --workspace       # nextest does NOT run doctests, silently
cargo xtask test-all              # both
```

- **A test finds a repo file through `tutti_fixtures`, never a relative `..`
  path:** `asset("audio/stereo.flac")` and `repo_root()` for the repo's files
  (`scripts/check-layout.sh` greps for depth-dependent paths), and `cdylib`
  and `plugin_server` for the reference plugins and `plugin-server`, which it
  finds in the test binary's own profile directory (no build script or env
  var, so it holds under `CARGO_TARGET_DIR` and `--target`). It is a
  dev-dependency and absence is a failure, never a skip. A crate-local file
  (`tutti-sf2`'s `samples/`) is `CARGO_MANIFEST_DIR.join(..)` without
  `..`.
- **Build `plugin-server` first:** `cargo build -p tutti-plugin-server`.
  Without it, `tutti-plugin`'s `clap_pdc_alignment`, `clap_crash_recovery` and
  `real_stall_tests`, `tutti-plugin-ipc`'s `subprocess::launch` tests,
  `tutti-session`'s `plugin` suite, `tutti`'s §5.2
  example (`plugin_midi_export`, `--features clap,export`), and `tutti-session`'s
  `export_fork_plugin`, `plugin_latency` and `plugin_modulation` suites (ported
  from `bevy-tutti`; `--features clap,export,modulation,device`) fail with that
  exact command, and
  `tutti-plugin-contract`'s `server` suite (`--features server`) needs
  `--no-default-features --features clap,vst2,vst3`. They must fail
  rather than skip, and they cannot use a dev-dependency because that would
  be a cycle.
- **Build `plugin-probe` first** for a suite that scans: `cargo build -p
  tutti-plugin-probe` (`tutti_fixtures::plugin_probe()`, the same rules as
  `plugin_server`: a dev-dependency builds a library, not a binary, and a
  missing one is a failure, never a skip). `tutti-plugin-contract`'s `probe`
  suite (`--features server`) spawns it, and so does `tutti-plugin`'s opt-in
  `db_probe` test (`TUTTI_PROBE_PLUGIN`). `tutti-session`'s `plugin` suite loads
  through the server and does not scan, so it needs the server alone. A scan
  with no `plugin-probe` on hand degrades to filename metadata
  (`ProbeNotFound`), so a suite that must see a real probe uses the fixture.
- **Feature-gated suites** run only where a feature is on. A workspace run
  enables just what some member's dependency edge turns on (for example
  `tutti-nodes/convolution`), not a crate's own optional features. The rest
  (plugin formats, `midi-hardware`, `clap-extras`, ...) are named in CI's
  `dark features` job and `cargo xtask test-features`. Name the feature when testing
  one crate, and add a new gated suite to both lists, or it never runs.
  **`tutti-midi-hardware --features sequencer-tests` is the exception:** it runs
  `Devices` against the real ALSA sequencer, which needs the kernel's `snd-seq`
  module and a readable `/dev/snd/seq`. CI's runner kernel (Azure) ships
  neither the module nor a package for it, so CI only lints the feature and the
  suite runs locally (`cargo xtask test-features`). It fails, never skips, where the
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
  `bevy` feature (`tutti-types`, `tutti-modulation`, `tutti-sampler`); the
  rest have none, and `tutti-session`, `tutti-graph`, `tutti-ump`
  and `tutti-codec` must never grow one (`WaveAsset`, the Bevy wrapper of a
  decoded `Wave`, is `bevy-tutti`'s). `bevy-tutti` is the only crate that
  requires Bevy. Check a crate that has the feature with `cargo check -p <crate>` and
  `--features bevy`. `--no-default-features` is *not* this check.
- **Import paths:** import engine types from a crate's root or prelude, never
  through its modules. Only `cargo xtask check-paths` enforces this.

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

`cargo xtask check-windows` runs this over the workspace and over the plugin crates
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
`tutti-plugin-ipc`'s `transport` or plugin loading.

## Workspace

**`bevy-tutti` is parked** (2026-11, `docs/engineering-notes.md`, "bevy-tutti is
parked"). It is in `crates/bevy-tutti` but out of the workspace (`exclude`, and
its own `[workspace]` root): no workspace command builds Bevy, and CI does not
build it, except a weekly drift check (`.github/workflows/parked-bevy.yml`) that
measures and gates nothing. **Lanes do not update it**: an engine change that
breaks it is not a reason to touch it. Build it on demand with
`cargo check --manifest-path crates/bevy-tutti/Cargo.toml --features full,plugin
--all-targets` (no committed lock file; it resolves fresh). Un-parking is one
migration PR: put it back in `members`, drop the `exclude`, restore its
`workspace = true` inheritance, the CI/xtask lines and `ADAPTER_PARKED` in
`scripts/check-adapter.sh`. Its engine-path tests were ported to
Bevy-free suites in `tutti-session` first. The rules below that name it
describe the adapter as it was parked.

One workspace at the repo root. The member list in `Cargo.toml` is the source
of truth.

```
xtask/           the development commands (`cargo xtask help`): std only, no
                 dependency of any kind, `publish = false`, the one member outside
                 crates/ and testing/; no layer, nothing depends on it
crates/          everything shipped; directory name = package name; no nesting
  tutti          Bevy-free umbrella. Every item is a `pub use` (tests/no_logic.rs).
  bevy-tutti     PARKED (out of the workspace, see above): Bevy umbrella + adapter; the only Bevy-mandatory crate.
  tutti-session  Bevy-free API layer: the cross-crate composition both umbrellas use
                 (design 015: `Session`, `Graph`, `SessionBuilder`, `Device`), and
                 `Recorder` / `PumpLoop`, a source pumped into a sink (feature `record`).
  arc-lend       The publish cell (`ArcLend`/`Loan`): a general-purpose crate with
                 no dependencies, `publish = false`, re-exported by tutti-rt.
  tutti-rt       Real-time-safe shared-memory primitives for the audio thread:
                 `AudioThreadCell`, `RtVec`, the position ring, the sequence lock,
                 the parallel executor's claims and job gate, `with_no_denormals`,
                 and the `ArcLend`/`Loan`/`Reader` re-export. Holds the lock-free
                 and `unsafe` runtime structure, with every loom model, and the one
                 thread-priority call (`request_priority`, which reports what the OS
                 granted); depends on `arc-lend` alone (and `thread-priority`),
                 `publish = false`
  tutti-ump      the Universal MIDI Packet: `Ump`, `MidiGroup`, `MidiChannel`,
                 `CCNumber`, and `Message` with `decode` and `From<Message> for Ump`,
                 which hold every message's bit positions (`Ump::words` and
                 `is_note_off` read the type, status and velocity bits a queue
                 needs), and the MIDI 1.0 wire: `Voice1::bytes`, `System::bytes`,
                 `frame` and `Midi1Parser`, the status bytes the hosts use
                 (`tutti-midi-types`' `wire.rs` holds its own until the MIDI
                 lane deletes it), `NoteId` (`channel * 128 + note`), and
                 SysEx: `SysexMessage` (a checked `F0 .. F7`), `SysexAssembler`
                 (SysEx7 packets to whole messages over a caller's buffer) and its
                 inverse `sysex7_packets` (`SysexMessage::packets`,
                 `Sysex7Packets::checked` for a payload of plain bytes), with
                 `Sysex7Packet::data` and `SysexRef::new`; no
                 dependencies but `bytemuck` (and optional `serde`)
  tutti-midi-protocol  MIDI 1.0 to MIDI 2.0 translation (design 017 §4 and §6): the
                 stateless `widen`, `narrow` and `midi1`, the M2-115 scaling, and the
                 float converters (`u32_to_unit_f32` and kin); depends on `tutti-ump` alone
  tutti-voicing  what a note-driven instrument decides before it makes sound, as one
                 pure step: `Voicing::step(channel, message, patch, voices) -> &[Command]`
                 (`Start`, `Retarget`, `Release`, `Damp`, `Cut`, `Retune`) over channel state, the
                 note table and the note gate (`Holds`), with the differences between
                 instruments as values (`Rules`) and a `Patch`; no audio, no voice, no
                 allocation after `new`. Depends on `tutti-ump`, `tutti-midi-protocol` and
                 `tutti-types`; `publish = false`
  tutti-midi-device  MIDI device core (design 017 §5): `DeviceId`, `OsChange`, the pure
                 `DeviceTable`, and the MIDI-CI codec (`CiMessage`: Discovery, Invalidate
                 MUID, NAK, over SysEx payload bytes); no OS, no thread, and no
                 dependency but `bitflags` and optional `serde`
  tutti-types    vocabulary only: units, channel layouts, io edge traits;
                 no FFT and no new algorithms but `Fold` (topology to
                 topology, built on the control side). `#![forbid(unsafe_code)]`.
                 The width folds are gone (every caller builds a `Fold`); sample
                 conversion stays until the edges move off it
  tutti-codec    the audio file codecs. Out: `Format` (`Wav(Pcm)`, `Aiff(Pcm)`,
                 `Flac { depth, level }`, `Vorbis { quality }`), `Spec`, and `Encoder`
                 over a `Write + Seek` sink, and `FileOut` (the file sink: the
                 `Sink` over an `Encoder`, any `Format`); the `Pcm` widths and
                 their one quantization. In: `Wave` (the resident sample buffer), `Wave::read`,
                 `Wave::read_metadata`, `FileIn` (the streaming `Source`, `FileOut`'s mirror),
                 `DecodeError`, `can_decode`. Depends on `tutti-types` alone
  tutti-graph    doc 013: the `Node` contract, Topology->Plan compiler, serial
                 executor + reference interpreter, the `WorkerPool` and the
                 `PrerollTable`, `MAX_ROOT_CHANNELS`, the tap (`TapReader`, a
                 `Source`) and its inverse the inlet (`InletNode`); `Engine` renders it
  tutti-dsp      pure algorithms over slices: the real FFT, the cosine windows and `StftGeometry`, filters,
                 dynamics, convolver, delay lines, reverb, waveshapers, the export
                 resampler
  tutti-modulation modulation compiler
  tutti-measure  the analyzers, measurements over slices that read audio and return
                 numbers: loudness (EBU R128, windowed momentary and short-term), peaks
                 and waveform blocks, pitch (`yin`), onsets, spectra, spectral shape,
                 stereo correlation and ballistics, the vectorscope, silence regions,
                 cross-correlation and take alignment, tempo and beats, notes, chroma, key
                 and chords, and the STFT transforms; depends on `tutti-types`
                 and `tutti-dsp` (the FFT, the windows, `StftGeometry`)
  tutti-transport `Transport`, the motion state machine, timed commands, the
                 offline timeline, `ProjectMeter`
  tutti-metering `LoadMeter` (level meters are plan ops: `tutti-graph`'s `MeterReader`)
  tutti-tempo    `TempoMap`: step and ramp tempo segments, beat <-> seconds <-> frame
                 conversion in `f64`, and constant-tempo cells for a ramp
  tutti-engine   `Engine`, the per-block graph render, over a transport and
                 the load meter (the metronome node, `ClickNode`, is `tutti-nodes`')
  tutti-cpal     device
  tutti-export   offline render
  tutti-nodes, tutti-spatial (vbap, hrtf), tutti-sampler, tutti-polysynth,
  tutti-soundfont (the SoundFont player: the node, the voices, the patch that
                 turns a note into voices; it executes `tutti-voicing`'s
                 commands), tutti-sf2 (the SoundFont 2 file as data)
  tutti-midi-{types,runtime,hardware,file}, tutti-alsa-ump-sys
                 (raw ALSA UMP FFI that alsa-sys lacks; Linux only)
  tutti-plugin{,-types,-db,-server}, tutti-{vst2,vst3,clap,au}-host,
  tutti-plugin-ipc (the process edge: the wire, the shared-memory slab and
                 control socket, the plugin-server's launch, and the
                 client of a served plugin, `PluginBridge`; no plugin format,
                 design 021 §6.1),
  tutti-plugin-probe (the `plugin-probe` binary: reads a plugin's descriptor
                 into a result file, `tutti-plugin-types`' `probe_wire`; the format
                 hosts and no hosting; nothing depends on it, design 021 §2.5),
  tutti-clap-ffi (all CLAP unsafe), tutti-vst3-ffi (all VST3 unsafe),
  tutti-au-ffi (all AU unsafe; macOS only, empty elsewhere),
  tutti-vst2-ffi (all VST2 unsafe: the host half of our fork of the vst crate; the plugin
  is a main-thread `PluginInstance` that closes it and an audio `RenderHandle` that never does)
testing/         never shipped
  tutti-clap-test-plugin, tutti-vst2-test-plugin, tutti-fixtures,
  tutti-vst2-author (the plugin-authoring half of the vst fork: `plugin_main!`, the
  `AEffect` builder, the plugin-side dispatcher; only the VST2 test plugins use it),
  tutti-shared-plugin (the test plugin: a format-free core, a shell per
  format), tutti-plugin-contract (the cases every format's host runs against
  it), tutti-shm-model (loom), arc-lend-compare
vendor/          third-party code we never edit
  vst3-sdk/      the three Steinberg submodules
```

**Layering.** Dependencies between workspace crates point strictly down this
list (normal and build edges; dev-dependencies may go anywhere).
`scripts/check-layout.sh` holds the table and fails on an unplaced crate:

0. `arc-lend`, `tutti-alsa-ump-sys`, `tutti-ump`
1. `tutti-types`, `tutti-rt`, `tutti-midi-protocol`, `tutti-midi-device`
2. `tutti-graph`, `tutti-dsp`, `tutti-modulation`, `tutti-midi-types`, `tutti-metering`,
   `tutti-codec`, `tutti-tempo`, `tutti-voicing`, `tutti-sf2`
3. `tutti-transport`, `tutti-plugin-types`, `tutti-measure`
4. `tutti-engine`, `tutti-nodes`, `tutti-midi-runtime`, `tutti-midi-file`,
   `tutti-clap-ffi`, `tutti-vst3-ffi`, `tutti-au-ffi`, `tutti-vst2-ffi`,
   `tutti-plugin-db`, `tutti-plugin-ipc`
5. `tutti-cpal`, `tutti-export`, `tutti-spatial`, `tutti-sampler`,
   `tutti-polysynth`, `tutti-soundfont`, `tutti-midi-hardware`,
   `tutti-clap-host`, `tutti-vst3-host`, `tutti-au-host`, `tutti-vst2-host`
6. `tutti-plugin`, `tutti-plugin-probe`
7. `tutti-plugin-server`, `tutti-session`
8. `tutti` (and `bevy-tutti`, when it is unparked)

- A member is directly `crates/<package-name>` or `testing/<dir>`, except
  `xtask` (a named exception, the top-level directory); a nested directory is
  refused. Nothing under `crates/` has a normal or build
  dependency on `testing/`; a test reaches `testing/` through a
  dev-dependency.
- `xtask` is in no layer: it depends on no workspace crate (of any kind, and
  on no outside crate: std only), nothing depends on it, it is
  `#![forbid(unsafe_code)]` and `publish = false` (`scripts/check-layout.sh`).
  A recipe is its planning in `xtask/src/recipes.rs`; keep the commands there
  the commands CI runs.
- `vendor/` holds third-party code we never edit and no workspace member.
  Our own forks (`tutti-sf2`, `tutti-vst2-ffi`) are crates, not vendored.
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
  refuse out of range, like the field types (`U7`, `U14`, `NoteNumber`, ...),
  which have the same shape. `of` is for a literal or a constant (it panics out
  of range), and `masked` is the one constructor that masks, for bits the wire
  has just framed. A packet's Group and Channel are `Ump::group`,
  `Ump::address` and `Ump::channel`: nothing outside `tutti-ump` shifts a
  header by hand. A note's identity is `NoteId`, `channel * 128 + note` (the
  plugin formats' wire value, one formula); an id above `NoteId::MAX_CHANNEL_NOTE`
  is one an allocator minted and `channel_note()` is `None`. A complete SysEx is a
  `SysexMessage` (checked once: `F0`, 7-bit bytes, `F7`), and the one SysEx7
  reassembler is `SysexAssembler<B>` over a buffer the caller sizes
  (`SysexAssembler::room_for`): it allocates nothing, holds one open message
  across all groups and counts what it drops.
- `tutti-midi-protocol` depends on `tutti-ump` alone, keeps every module
  private (`tutti_midi_protocol::widen`; CI runs the per-module path gate on
  it) and is `#![forbid(unsafe_code)]`. It holds no state and allocates
  nothing. A consumer's call to `widen` is transitional: the stage 3 translator
  calls it at the input edge and consumers drop theirs.
- `tutti-voicing` depends on `tutti-ump`, `tutti-midi-protocol` and `tutti-types`
  only, keeps every module private (`tutti_voicing::Voicing`; CI runs the
  per-module path gate on it), is `#![forbid(unsafe_code)]`, `publish = false`
  and has no `bevy` feature. **The note logic of an instrument is one pure step
  here, and an instrument keeps its voices and its render:** it builds a
  `VoiceView` list, calls `step`, and does what the `Command`s say (`PolySynth`
  does, in `tutti-polysynth`'s `VoiceAllocator`, which is now its slot table).
  What differs between instruments is a value of `Rules` or a `Patch`, never a
  branch in the core; a difference only one instrument has and the spec does not
  require is a reason to look again. `step` allocates nothing: its tables are
  sized in `new` and a `Clone` keeps their capacity; the instrument passes
  `VoiceView`s sorted by id, and only for a note-on (liveness is read there).
  The spec is the only oracle: a rule gets a test from the spec and a mutation,
  and nothing is pinned against another implementation. Mono and legato follow
  tutti's named rule (last-note priority over a per-channel stack of held keys),
  which MIDI 1.0 does not specify.
- `tutti-midi-device` depends on no workspace crate, keeps every module private
  (`tutti_midi_device::DeviceTable`; CI runs the per-module path gate on it) and
  is `#![forbid(unsafe_code)]`. It holds no OS handle, thread or clock: a backend
  feeds `DeviceTable::apply` an `OsChange`, and `tutti-midi-hardware` owns the
  threads and the OS. It also holds the MIDI-CI codec (`CiMessage::encode` and
  `decode` over the bytes between `F0` and `F7`, which the caller frames with
  `tutti_ump::frame` and reassembles with `tutti_ump::SysexAssembler`; `Muid::new`
  refuses the reserved range and broadcast, which is a `Destination`). It has no
  state machine and no inbound path; design 017 §5, "MIDI-CI", is the rebuild.
  Doc 017 §6 gives it `tutti-ump` for the Endpoint and Connection of stage 4b; the
  codec needs no `tutti-ump` type, so the edge is not there yet. A consumer that persists a `DeviceInfo` depends on it with
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
  `tutti-rt`, `tutti-graph` and `tutti-tempo` (the project's tempo is a
  `TempoMap` that `TransportSettings` publishes through an `ArcLend`, swept by
  the session's `Reclaim` phase: `reclaim_tempo`); `tutti-engine` on those and the two crates
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
  themselves, whatever `Bpm` accepts, and no query allocates. Time is `Seconds`,
  an `f64`. A map reaches a constant-tempo reader as `Cell`s on one
  fixed `CellGrid`, also cut at every point's frame (`TempoMap::next_change`);
  `chord_cell` is a pure function of the absolute frame, a cell inside a step
  has that step's tempo to the bit, and the crate derives no grid from a block.
  The transport follows a map, steps and ramps, with one walker
  (`tutti-transport`'s `TempoWalk`, doc 020 stages 3 and 4) on one engine grid
  of 128 frames: a step or a ramp cell is a `TransportChanges` entry, the engine splits a block that needs more than 8, and
  a constant map never reaches the walker, so its render is bit-identical.
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
  WAV writer is one: `tutti-export` and `FileOut` (the codec's `Sink`, the
  mirror of `FileIn`) both write
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
  `unsafe` runtime structure (apart from `arc-lend` and the `-ffi` crates), and
  the one thread-priority call, `request_priority` (a request for an ordinary-
  scheduler raise, never real time, whose `PriorityOutcome` is what a caller
  logs; the plugin server and the sampler's streamer both use it). Keep
  every module private (`tutti_rt::RtVec`, never `tutti_rt::vec::RtVec`; CI runs
  the per-module path gate on it).
- `tutti-dsp` depends on no tutti crate but `tutti-types` (its outside deps are
  rustfft, libm and thiserror), keeps every
  module private (`tutti_dsp::RealFft`, never `tutti_dsp::fft::RealFft`; CI runs
  the per-module path gate on it), is `#![forbid(unsafe_code)]` and has no
  graph or runtime, no `Arc`-published state and no atomics for control: every
  control arrives per call as a plain typed value. It holds the real FFT, the
  cosine windows (`CosineWindow`, `Window`), the STFT's `StftGeometry`, `Grid` and
  `Error`, the SVF, ladder and Linkwitz-Riley filters (and their `Real`
  float), the dynamics curves, followers and kernels, the partitioned
  convolver, the delay line and swept-delay kernel, the cubic Hermite kernel, the phaser's all-pass chain, the
  block ramps, the export resampler and its streaming
  bookkeeping (`StreamingResampler`, `ChunkSize`), the dither noise
  (`DitherNoise`), the Freeverb reverb (`Freeverb`, which is also the
  `Kernel` of `FreeverbDesign`), the feedback delay network (`FdnDesign`,
  `FdnKernel`: its kernel ramps between `retarget`s, in 32-frame chunks counted
  from the last reset, so the output does not depend on the block length), the
  reverb model of doc 022 (`Network`, `Design`, `Kernel`, `Setup`, `Bounds`,
  `FreeverbDesign`: a plain
  fixed-capacity description with analytic queries, never an instruction stream
  a kernel must follow, and the kernel that runs it, moved to each network by
  `retarget`; a `Design` writes its network in place, `design_into`, because a
  `Network` is too large to build on an audio thread), the
  ring buffers and the waveshaping curves. `tutti-nodes` holds the `tutti_graph::Node` adapters over
  them (shape, prepare, process, params, fork; `ReverbNode<D: ReverbDesign>` is
  the one node over every reverb `Design`, with `FreeverbNode` and `FdnNode`
  its two instances and `ReverbKind` the choice between them), and re-exports only the
  algorithm types that sit in a node's signature (`SvfType`, `LadderType`,
  `Real`, `CrossoverError`, `InterpolationMode`, `ShapeKind`, `FdnControls`,
  `FdnDesign`, `FreeverbControls`, `FreeverbDesign`).
- `tutti-measure` depends on `tutti-types` and `tutti-dsp` alone among workspace
  crates (its outside deps are `ebur128`, the only place it is pulled, and
  `thiserror`; `tutti-graph` is a dev-dependency only, for the README's tap
  example), keeps every module private (`tutti_measure::Loudness`, never
  `tutti_measure::loudness::Loudness`; CI runs the per-module path gate on it), is
  `#![forbid(unsafe_code)]` and has no `bevy` feature or other feature. Like
  `tutti-dsp` it has no graph, runtime or `Arc`-published state: every control
  arrives per call as a plain config value, and a measurement that runs a
  stream is a `Config`/`State`/`step` triple. It reuses `tutti-dsp`'s
  `RealFft`/`FftScratch`, `Complex`, `StftGeometry`, `Grid`, `Error` and
  `CosineWindow` and does not copy them: those serve the processors too (the
  sampler's vocoder builds on `StftGeometry`).
  A processor goes in `tutti-dsp`, a measurement in `tutti-measure`.
- **A pure algorithm over slices goes in `tutti-dsp` (or, if it only reads audio and
  returns a measurement, `tutti-measure`); a node file holds only
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
- `tutti-plugin-ipc` depends on `tutti-types`, `tutti-rt`, `tutti-ump`,
  `tutti-midi-types` and `tutti-plugin-types` alone among workspace crates,
  keeps every module private but `state_chunk` (the namespace both ends chunk
  a state with; CI runs the per-module path gate on the rest) and has no
  `bevy` feature. Its `unsafe` is the process edge's own (shared memory and
  its mapping, the Windows control-socket deadline, the launch), each block
  under a `// SAFETY:`. It knows no format:
  the editor-only instance a `PluginBridge` opens is loaded by the host's
  `EditorLoader`, and no `tutti-*-host` depends on it
  (`scripts/check-layout.sh`). Its test-only knobs (`set_rate_timeout`,
  `settled_replies`) are behind `test-support`, which `tutti-plugin`'s
  dev-dependency turns on.
- `tutti-spatial` depends on `tutti-nodes`, never the reverse.
- `tutti-midi-file` is OS-free.
- `tutti-sf2` is the SoundFont 2 file as data (parse, presets, instruments,
  sample headers, sample data, raw generators) and holds nothing that plays.
  Its workspace dependencies are `tutti-ump` and `tutti-types` (the units its
  accessors return: `Samples`, `SampleRate`, `Cents`) and no others; `serde` is
  an optional feature, on `Bank` only. It keeps every module private
  (`tutti_sf2::SoundFont`; CI runs the per-module path gate on it) and is
  `#![forbid(unsafe_code)]`. It has one error type, `tutti_sf2::Error`, with a
  `Result` alias, and no public four-character-code type. `tutti-soundfont`
  does not re-export `SoundFont`: its one path is `tutti_sf2::SoundFont` (the
  umbrella `tutti` offers it as `tutti::sf2`). `tutti-soundfont` is the player over it: the
  note logic is `tutti-voicing`'s (`Voicing::step` and its `Rules`, with a
  `Patch` that starts one voice for each matching preset zone and instrument
  zone), and the crate keeps what renders a voice (the oscillator, filter,
  envelopes, LFOs, the generator summing of SF2 section 9.4 and the unit
  conversions of 9.3). That voice render derives from Nobuaki Tanaka's
  rustysynth (the former `tutti-rustysynth`, folded in): every file derived from
  it carries the MIT notice in its header and `LICENSE-RUSTYSYNTH.txt` is the
  licence text. `tutti-sf2` keeps the original author's MIT license and
  copyright (`LICENSE.txt`) likewise.
- (While `bevy-tutti` is parked a Bevy app has no maintained adapter.) A Bevy app depends on `bevy-tutti`; a headless consumer depends on `tutti`.
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
- **A bare number becomes a unit with `new` and leaves with `get`** (`Unit::from_raw`
  and `to_raw` in generic code). The `unit!` macro emits no `From<raw>` and no
  `From<Unit> for raw`, so `.into()` never crosses between a unit and its raw: the
  quantity is named where the number is written. A signature takes the unit type
  (or `impl Into<Unit>` for a conversion between units, such as `Amplitude` to `Db`),
  never `impl Into<f32>`.
- **Convert with the named converters** (`Db::to_amplitude`,
  `Seconds::to_samples{,_floor,_ceil}`, `Cents::to_semitones`), never by hand.
- **Measurements are not controls.** `Confidence`, `Correlation` and `Pan`
  are readings. Do not swap them for `Mix` or `Depth`.
- **Where the types stop:** C ABI, IPC and WIT boundaries, and precision the
  unit cannot carry. Say why in a comment. `Seconds` is `f64` (short or long);
  a DSP kernel that runs in `f32` narrows once at its edge (`Seconds::to_f32`),
  and a modulation lane's value is an `f32` (`ParamCell`). A frame count or a
  coefficient is computed from the exact `Seconds`, never from a value rounded
  through `f32` first.
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
- `BeatsPerBar` and `NoteValue` keep the shape: `new` refuses and serde
  refuses. Only `BeatsPerBar::clamped` saturates (the beat index in `bar_at`);
  `NoteValue` has no saturating constructor until a lenient importer needs one.
- **A new type is checked by `tutti_fixtures::check_range_contract`** in a
  test (valid and invalid raws through `new`, `TryFrom`, `of` and serde), and
  `cargo xtask check-range-types` fails a private tuple field with a derived
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
  runs, and nothing more), the streamer's cycle (its rings' maps). The
  session's sweep is decided in `tutti-session` alone: a node whose
  controls publish returns a `Reclaimer` from `SessionNode::reclaimer`,
  which the graph keeps by node from `add`/`crossfade` until the node goes;
  the graph sweeps its hosted plugins' meters and closes the level meters
  whose readers were dropped (`Graph::reap_meters`); the session sweeps its own
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

- **`Source` / `Sink`** (`tutti-types::io`) are engine **edges** (`MicIn`,
  `FileIn`, a `TapReader`; `FileOut`): a stream on a clock the graph does not
  keep, not the disk streamer's positional ring. They take flat
  interleaved `&[f32]` with a runtime `ChannelLayout`. **Every count on this
  boundary is in frames**, typed as `Samples` (the frame count). Cross to a
  slice length only with `Samples::interleaved_len` /
  `Samples::from_interleaved_len`.
- **`tutti_graph::Node::process`** is for anything that is a node in the
  graph (including sampler voices): planar `f32` through `Io`, any block
  length up to `Prepare::max_block`, the transport from `Env`.
- **A channel fold goes through `tutti_types::Fold`**, built on the control side
  and applied without allocating. A width is not a placement: an edge reports a
  topology (a channel with no speaker is `Speaker::Discrete`), and an edge that
  knows only a width (a device, an export file, the graph root) builds
  `Fold::widths` / `Fold::to_width`: the engine's order where the width has one
  (`ChannelTopology::assumed`: mono, stereo, quad at 4, 5.1, 7.1, 7.1.4), channels
  by index (`Fold::by_index`, widening only: a narrowing there is
  `FoldError::Unplaced`, never a truncation; `Fold::select` truncates on purpose)
  where it has none. The width-only `fold_frame` is
  gone; do not add another. `Engine::new` builds the folds from every root width to every device
  width up to 16 (`OutputFolds`, published through an `ArcLend` and lent once per
  call) and `Engine::support_output` adds a wider one; a buffer of an unsupported
  width is a silent counted block, and `OutputBlock::new` makes the call. Export folds the graph onto the file in `GraphSource::new`.
  The sampler builds a `Place` from `Wave::topology()` when it builds a source
  (`MemorySource::try_with_channels`, `memory_voice`), and a wave with a channel
  no speaker names (`FoldError::Unnamed`) is refused there as a `PlaceError`;
  the streaming tier reads a file at its own width and order. The input edges
  report a topology: a decoded file (`Wave::topology`, `FileIn::topology`,
  `WaveMetadata::topology`), a capture device (its inputs are `Discrete`;
  `MicIn::open_inputs` chooses them with `Fold::select`). The quad spelling is
  WAVE's (`FL FR BL BR`); `Fold` treats the side-surround spelling the plugin
  hosts decode as the same four feeds.
- **Width is runtime.** Do not reintroduce a const-generic channel count. The
  two runtime width checks that replaced it (`pump`'s `debug_assert` and
  `Recorder::start`'s error, in `tutti-session`) are mandatory.
- **Every `Source::pull` must answer `Starved` (live "not yet") or `Ended`
  (finite "never again") correctly**, and `Failed` only after the frames
  before it (a live source reads its fault flag before its ring:
  `tutti_rt::FaultLatch`). `Pull` is `#[must_use]`; getting it wrong ends a
  recording with no error.
- **A new trait must state its boundary in one sentence without "and".**

## Wiring is declared, not called

`bevy-tutti` (parked) runs on `tutti-graph` only (doc 013, "Flip the adapter").

- `spawn_audio_node` adds an *unwired* node. `PortSources` on a sink and the
  `MasterSources` resource declare what feeds each port. The rebuild builds a
  `Topology` value from them (`LiveGraph` holds the last one), writes the
  declared ports that differ into the session graph's spec, and the frame's
  commit compiles it iff the graph was edited (`Graph::is_edited`); PDC is
  the compiler's, and nothing is spliced into the graph.
- A headless `Session` declares through its own spec, keyed by
  `NodeKey`, and `Session::update()` commits iff the graph was edited.
- **The root's channels are declared too** (`MasterTopology`, a pure
  translation into `Session::declare_root_topology` /
  `SessionBuilder::root_topology`): `Discrete` for independent buses, a named
  topology for speaker feeds, nothing for the engine's `assumed` order. A
  declaration sets the root's width (no device floor) and the width cannot
  leave it: `Graph::widen_outputs` refuses to widen a declared root, and a
  declaration the device or file cannot place is `FoldError`, never a silent
  drop (doc 013, "Channel placement").
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
- The VST2 shell uses `testing/tutti-vst2-author` (the plugin-authoring half of the
  old `vst-tutti`, re-exporting `tutti-vst2-ffi`'s ABI modules) as a dependency.

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
- **Dead crate names:** `tutti_types::{Topology, NodeKey, Valid, TapId, TapSpec, TapAlign,
  MeterId, MeterSpec, graph, latency, tail, Compensation, Delays, Feed, LatencyGraph, delays,
  GraphTail, TailGraph, graph_tail}` → `tutti_graph::` (`latency::plan` is
  `tutti_graph::latency_plan`), `tutti-io` → dissolved: `TapIn` is gone (`TapReader` is a
  `Source`), `WavOut` → `tutti_codec::FileOut` (any `Format`, beside `FileIn`),
  `MicMonitorNode` and `MicRing` → `tutti_graph::InletNode` (opened by
  `Graph::open_inlet`), `Recorder` and `PumpLoop` → `tutti-session` (feature
  `record`, implied by `audio-io`), `AudioIn` → `Source`, `AudioOut` → `Sink`, `OnEmpty`/`ON_EMPTY` → the `Pull` a
  `Source::pull` answers (`Starved`, `Ended`, `Failed`), `tutti_types::pump` → a private
  step of `PumpLoop`, `vst-tutti` → `vst-tutti` → `tutti-vst2-ffi` (the host half: ABI types, loader,
  host callback, buffers) and `testing/tutti-vst2-author` (the plugin side, test-only), `tutti-rustysynth` → `tutti-soundfont` (the voice
  render; the note logic went to `tutti-voicing`), `butler` → disk streamer (`tutti-sampler`'s `streamer/`: `ButlerCommand` → `StreamerCommand`, `ButlerThread` → `StreamerThread`, `ButlerSender` → `StreamerSender`, `ButlerGone` → `StreamerGone`, `ButlerCycle` → `StreamerCycle`; the thread is `tutti-streamer`), `tutti-units` → `tutti-nodes`, `tutti-synth` →
  `tutti-polysynth` + `tutti-soundfont`, `tutti-midi` → the four `midi/`
  crates, `tutti-analysis` → `tutti-dsp`, whose analyzers are now `tutti-measure`, `tutti-core` → `tutti-transport`
  (`Transport`, `ProjectMeter`, the clocks), `tutti-metering` (`LoadMeter`;
  the master meter is now a plan op, `Editor::open_meter`) and `tutti-engine` (`Engine`; `MAX_ROOT_CHANNELS`
  is `tutti-graph`'s), `tutti-node` and `fundsp-tutti` →
  deleted (the node contract is `tutti_graph::Node`; the filters' `Real` is
  `tutti-nodes`'), `tutti-mod` → `tutti-modulation` (renamed: "mod" is unclear), and tutti-modulation's runtime (`ModMatrix`, `ModParams`, `ParamFrom`,
  `modulation::legacy`) → deleted (doc 014 §10: `tutti_modulation::compile` and
  a `ControlLaneNode` per param). A comment or doc that disagrees with the
  code is the bug.
- **MSVC render gate:** `render_is_bit_identical_to_the_audionode_era` is
  gated off MSVC, because `sin` differs in the last ulp between CRTs.

## Consumers: two pins that must stay in step

Consumers pin this repo as a git dependency. Keep these in step with them;
both are in `[workspace.dependencies]`:

- **`audio-automation`'s rev.** A mismatch surfaces as
  `expected Curve, found Curve`.
- **The bevy 0.19 line**, for anything that touches `bevy-tutti` (its pins now
  live in `crates/bevy-tutti/Cargo.toml`, while it is parked).
