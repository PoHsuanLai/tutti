# CLAUDE.md

Guidance for Claude Code working in the tutti audio engine. This file is rules
and commands. The reasoning and history behind them is in
[`docs/engineering-notes.md`](docs/engineering-notes.md): read the matching
section there before you relax or work around a rule.

## Direction: the native graph

**fundsp's `Net` is being replaced by a native graph**, per
[`docs/design/013-native-graph.md`](docs/design/013-native-graph.md): a
`Topology` value, a pure compiler producing an immutable plan, units stored
once, and events as ports. Phase 3 is done: `Engine` renders only the
native graph (`Engine::new(&transport, &mut editor, executor)`, PR 15), it
is `bevy-tutti`'s only runtime (`AudioGraphRes` holds an `Editor`, PDC is
the compiler's, export forks the live graph with `Editor::fork`), and
tutti-export renders only it. `Net` is left as a container, not a runtime,
until Phase 5 deletes fundsp: `topology::compile`, the `Net` form of one
builder (`build_vbap_mix` / `VbapMixParts::insert_into`), the
nodes' own tests and `tutti-graph`'s A/B bench wire units in one
(`tests/no_net_backend.rs` keeps `NetBackend` out of tutti-core). Until the
migration lands:

- Do not add new dependencies on `Net`, `NetBackend`, `Setting` or the
  fundsp combinators. Write nodes against the smallest surface you can
  (`process`, `reset`, `set_sample_rate`, declared latency and tail).
  Do not compare against a `Net` render in a new test: pin an analytic
  figure, an invariant of the render, or (for samples that call libm) a
  golden digest asserted only on the target it was recorded on.
- Musical delay is not latency. Report only processing latency to PDC.
- Transport commands meant for playback take an `At`
  (`MotionFsm::schedule`); the untimed `try_send` means `At::NextBlock`. A
  graph node that needs the transport at a frame reads `Env::transport_at`:
  a start or seek inside a block is in `Env`, never a split block.
- Doc 013 lists the defects and types each phase addresses. Update it when a
  phase lands or a decision changes.

## Commands

`just` wraps these; `just --list` shows them. `just ci` runs what CI runs.

```bash
cargo build
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt
scripts/check-canonical-paths.sh   # the import-path gate (also searches *.md)
scripts/check-submodule-pins.sh    # VST3 SDK pins, asserted by SHA
```

### Testing

**Use `cargo nextest run`, not `cargo test`.** The audio tests touch device
and global state; nextest isolates each test in its own process.

```bash
cargo nextest run --workspace
cargo nextest run -p tutti-core -E 'test(my_test_name)'
cargo test --doc --workspace       # nextest does NOT run doctests, silently
just test-all                      # both
```

- **Build `plugin-server` first:** `cargo build -p tutti-plugin-server`.
  Without it, `tutti-plugin`'s `clap_pdc_alignment`, `clap_crash_recovery` and
  `real_stall_tests` fail with that exact command. They must fail rather than
  skip, and they cannot use a dev-dependency because that would be a cycle.
- **Feature-gated suites** (for example `tutti-nodes --features convolution`)
  are only covered by a workspace run. Name the feature when testing one crate.

### Setup

- **Submodules:** `git clone --recursive`, or
  `git submodule update --init --recursive`. The VST3 SDK (three Steinberg
  repos pinned at `v3.8.0_build_66` under `crates/plugin/vendor/vst3-sdk/`) is
  otherwise empty. `git describe` reports `v3.7.3_build_20-NN`; that is
  expected, and the gate asserts SHAs.
- **Linux:** `tutti-cpal` needs ALSA headers (`libasound2-dev pkg-config` or
  `alsa-lib-devel pkgconf-pkg-config`). Without them the build fails in
  `alsa-sys`'s build script.
- **Bevy-free floor:** `bevy` is an optional, off-by-default feature on every
  engine crate. `bevy-tutti` is the only crate that requires Bevy. Check with
  `cargo check -p <crate>` and `--features bevy`. `--no-default-features` is
  *not* this check.
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
with every format feature. It excludes the five members that cannot
cross-compile: `ogg_next_sys` (via `tutti-export`) and `tutti-vst3-host`'s
`conformance` probe need real Windows C headers. CI's `clippy (windows)` job
runs everything natively on a `windows-latest` runner.
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
crates/
  tutti          Bevy-free umbrella. Every item is a `pub use` (tests/no_logic.rs).
  bevy-tutti     Bevy umbrella + adapter; the only Bevy-mandatory member.
  core/          tutti-core (graph runtime, transport, metering, PDC)
                 tutti-types (value vocabulary, units, io edges, rt primitives, Topology)
                 tutti-node (node contract), tutti-cpal (device), tutti-io (I/O edge: live + file decode)
                 tutti-mod (modulation), tutti-export (offline render)
                 tutti-graph (doc 013 Phases 1–2: Node contract, Topology→Plan compiler,
                 serial executor + reference interpreter; `Engine` renders it)
  dsp/           tutti-nodes, tutti-spatial (vbap, hrtf), tutti-sampler,
                 tutti-polysynth, tutti-soundfont, tutti-analysis
  midi/          tutti-midi-{types,runtime,hardware,file}
  plugin/        tutti-plugin{,-types,-server}, tutti-shm-model (loom),
                 formats/{vst2,vst3,clap,au}-host, vendor/{vst-tutti,vst3-sdk}
  vendor/        fundsp-tutti, rustysynth-tutti, tutti-clap-test-plugin
```

- `tutti-types` names no other tutti crate.
- `tutti-graph` depends on no tutti crate but `tutti-types` and `tutti-node`
  (its outside deps are `bytemuck` and `ringbuf`, the editor→executor queue), keeps every
  module private (`tutti_graph::Plan`, never `tutti_graph::plan::Plan`; CI runs
  the per-module path gate on it) and is `#![forbid(unsafe_code)]`.
- `tutti-spatial` depends on `tutti-nodes`, never the reverse.
- `tutti-midi-file` is OS-free.
- The vendored forks are excluded from clippy and rustdoc.
- A Bevy app depends on `bevy-tutti`; a headless consumer depends on `tutti`.
  `bevy-tutti` does not depend on `tutti`.

## Unit types (MANDATORY)

Use the `tutti_types::value::units` newtype, never a bare float, for any
quantity one covers: fields, signatures, `Param<U>` and return types.

- **Operators are opt-in per type, and every omission is deliberate.** Read
  the omission ledger in `units.rs`'s tests before adding one. If it is listed
  there, write a named method instead.
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

## Publishing to the audio thread (MANDATORY)

Non-scalar state handed to the audio thread goes through
`tutti_types::{RtPublish, RtRef}`. Scalars use `Param<U>`.

- **The audio thread never holds an owning handle to published state, and
  never frees it.** `read()` returns a `!Send` borrow and is wait-free;
  `publish()` is control-thread only and frees retired values there. This is
  structural (hazard slots, overflow epochs, and a retirement list only the
  publisher touches),
  and the loom model `tutti-types/tests/rt_publish_loom.rs` checks it.
- Read once per block, never per sample. Never park an `RtRef` across blocks
  (a parked one delays the free of what it holds). Avoid nested reads: past
  the cell's slot count they stay safe but pin every value current during
  their overflow epoch, not just the one they hold.
- **The design's limit:** once parked overflow `RtRef`s occupy every overflow
  epoch, each publish's retired value stays pinned and memory grows until they
  drop. It is unreachable if the rule above is kept. A host can poll
  `retired_len()` and `epoch_stalls()`; debug builds assert at 1024.
- Never `publish` from the audio thread.
- Do not try to prove the race with a no-alloc test. The loom model and miri
  cover it; a single-threaded gate can only pin the reader's code path.
- Nullable hot-swap slots (`InputSlot`, `Midi::out`) stay on
  `ArcSwapOption`. Control-thread-only cells stay plain. (The sampler's
  `SharedReader` is a plain `Arc` of a position-indexed ring of atomics;
  nothing swaps it.)

## Buffers: edges vs graph nodes

- **`AudioIn` / `AudioOut`** (`tutti-types::io`) are engine **edges** (mic,
  WAV, `TapIn`, `FileIn`, the sampler's butler ring). They take flat
  interleaved `&[S]` with a runtime `ChannelLayout`. **Every count on this
  boundary is in frames**, typed as `Samples` (the frame count). Cross to a
  slice length only with `Samples::interleaved_len` /
  `Samples::from_interleaved_len`.
- **`AudioUnit::process`** is for anything that is a node in the graph
  (including sampler voices).
- **Width is runtime.** Do not reintroduce a const-generic channel count. The
  two runtime width checks that replaced it (`pump`'s `debug_assert` and
  `Recorder::start`'s error) are mandatory.
- **Every `AudioIn` impl must state `ON_EMPTY` correctly** (live "not yet" vs
  finite "never again"). Getting it wrong ends a recording with no error.
- **A new trait must state its boundary in one sentence without "and".**

## Wiring is declared, not called

`bevy-tutti` runs on the native graph only (doc 013, Phase 3 PR 13).

- `spawn_audio_node` adds an *unwired* node. `PortSources` on a sink and the
  `MasterSources` resource declare what feeds each port. The rebuild builds a
  `Topology` value from them (`LiveGraph` holds the last one), writes the
  declared ports that differ into the editor's spec, and the frame's commit
  compiles it; PDC is the compiler's, and nothing is spliced into the graph.
- A declaration may be partial: a port a short `PortSources` leaves out, and
  every output channel while `MasterSources` is empty, belong to whoever
  wires them through `AudioGraphRes`. The rebuild's debug check
  (`topology::disagreements`) compares only declared ports; never compare a
  whole-graph fold (a latency plan) against the value.
- A declaration names an *entity*, the graph keys an `AudioNode`; re-binding
  an entity to a new node re-derives its wires (`Changed<AudioNode>`).
- Keys are sink ports, so fan-in cannot be represented. Summing is a node's
  job.
- `GraphReconcileSystems`: `Spawn → Params → Despawn → Compensate → Commit`.
  Node removal is an `On<Remove, AudioNode>` observer.
- Params are `AudioParam<U, const P: u16>`, registered with
  `App::add_audio_param::<U, P>()`.

## Testing policy

- **Never simplify, skip or modify tests to make them pass.** Fix the bug.
- **Report failing tests and their root causes.** Never silently remove one.
- **Mutation-test new tests:** break the property, watch the test fail,
  restore it, and write the mutation in a comment. If a property cannot be
  covered here, say so in a comment rather than asserting something that
  always passes.

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
- **The `tutti` umbrella holds no code.** Anything that wants to live there
  belongs in the crate that owns what it wires.
- **The engine must not reference `dawai`.** The provenance note in
  `bevy-tutti/Cargo.toml` is the one exception. The `docs/` audits are dated
  records: fix only what claims to be current, and never "update" a reference
  to a new app-side path.
- **Dead crate names:** `tutti-units` → `tutti-nodes`, `tutti-synth` →
  `tutti-polysynth` + `tutti-soundfont`, `tutti-midi` → the four `midi/`
  crates. A comment or doc that disagrees with the code is the bug.
- **MSVC render gate:** `render_is_bit_identical_to_the_audionode_era` is
  gated off MSVC, because `sin` differs in the last ulp between CRTs.

## Consumers: two pins that must stay in step

Consumers pin this repo as a git dependency. Keep these in step with them;
both are in `[workspace.dependencies]`:

- **`audio-automation`'s rev.** A mismatch surfaces as
  `expected Curve, found Curve`.
- **The bevy 0.19 line**, for anything that touches `bevy-tutti`.
