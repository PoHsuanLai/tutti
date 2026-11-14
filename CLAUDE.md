# CLAUDE.md

Rules and commands for working in tutti. What an item does is in its rustdoc; read the
crate docs of what you touch. The reasoning behind a rule is in
[`docs/engineering-notes.md`](docs/engineering-notes.md); the library style is
[`docs/style-guide.md`](docs/style-guide.md).

## Commands

`cargo xtask <recipe>` wraps everything (`cargo xtask help`; `--dry-run` prints the
commands). `cargo xtask ci` runs what CI runs. `cargo xtask gates` (rustfmt, xtask's
tests and the source gates, seconds and no build) is what CI fails fastest on: run it
before a push, or once per clone `git config core.hooksPath .githooks` to have
`.githooks/pre-push` run it.

```bash
cargo build
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt
cargo nextest run --workspace              # not `cargo test`: nextest isolates each test
cargo nextest run -p tutti-engine -E 'test(name)'
cargo test --doc --workspace               # nextest does not run doctests
cargo xtask test-all                       # every test tier CI runs
cargo xtask doc                            # rustdoc as CI runs it
cargo xtask check-loom                     # clippy under --cfg loom
cargo xtask check-windows                  # Windows lints from Linux
cargo xtask deny                           # needs cargo-deny 0.20.2 (`cargo install --locked`)
```

The static gates (`cargo xtask check-*`) are Rust in `xtask/src/gates/`: a pure function
from the files, read through `Tree`, to a `Report`, tested over in-memory trees. xtask is
std only. A rule the compiler already enforces is not a gate.

- **Toolchain:** `rust-toolchain.toml` pins it and `Cargo.lock` is committed (`--locked`).
  A bump is its own PR that fixes the new lints.
- **Setup:** `git submodule update --init --recursive` (the VST3 SDK under
  `vendor/vst3-sdk/`; the gate asserts SHAs). Linux needs ALSA headers (`libasound2-dev
  pkg-config`).
- **Windows:** `cargo xtask check-windows` lints for MSVC from Linux; CI's `clippy
  (windows)` lints natively and also runs `tutti-winmm-ffi`'s and `tutti-midi-hardware`'s
  tests there (no other CI job runs tests on Windows). A vst3 SDK enum constant
  is `c_uint` on unix and `c_int` on Windows: cast it with `helpers::sdk_enum_i32`. Read
  the engineering notes before touching `tutti-plugin-ipc`'s transport or plugin loading.

## Testing

- **Build `plugin-server` and `plugin-probe` first** (`cargo build -p tutti-plugin-server
  -p tutti-plugin-probe`). A suite that spawns one fails with that command when it is
  missing; it never skips.
- **A test finds a repo file through `tutti_fixtures`** (`asset`, `repo_root`, `cdylib`,
  `plugin_server`, `plugin_probe`), never a relative `..` path. Absence is a failure.
- **A feature-gated suite runs only where its feature is on.** Add a new one to CI's dark
  features job and to `cargo xtask test-features`, or it never runs.
  `tutti-midi-hardware --features sequencer-tests` needs the kernel's `snd-seq`; CI only
  lints it, so run it locally before changing `backend/alsa`.
- **Golden digests** go through `tutti_fixtures::assert_golden`: asserted on the recorded
  libm (x86_64, glibc 2.39), skipped elsewhere, required in CI (`TUTTI_GOLDEN=require`).
  Re-record only for an argued libm move, with the owner.
- **One integration binary per area**: `tests/<area>/main.rs`, one `mod` per file. An area
  binary installs `assert_no_alloc::AllocDisabler` once and its allocation tests are
  modules in it; a file is its own binary only for a different allocator (counting,
  no-free), `harness = false`, or `cfg(loom)`.
- **A test must be able to fail.** It asserts a spec property, an analytic figure or an
  invariant, and its name says which. Delete a test that cannot fail: one that is always
  true, asserts only that nothing panicked, or compares a value with the function that
  computed it.
- **Never weaken a test to make it pass.** Fix the bug, or report the failure and its
  cause.
- **Never compare a render against another render.** Pin an analytic figure, an invariant
  or a golden digest. No migration tests (old against new) and no test-only seams.
- The spec is the oracle: nothing is pinned against another implementation.

## Code rules

### The graph

- Every node is a `tutti_graph::Node` (`shape`, `prepare`, `process`, `reset`). A node with
  params also implements `ParamNode`, declares them with `tutti_graph::declare_params!`
  next to its `Param<U>` fields, is inserted as `Params(node)` (never `ForkByClone`), and
  reads a driven param through the declaration (`declared_param`), never by port index. A
  plain node is `ForkByClone` or `Unforkable`. The crate docs' "Writing a node" is the
  guide.
- `Shape::tail` is a ceiling. A node whose ring-out follows its params narrows it per block
  with `Io::report_tail`, covering the ring-out and erring long. A finite ceiling past
  `MAX_NODE_TAIL` counts as unbounded; nothing clamps it.
- Musical delay is not latency: report only processing latency.
- Nodes communicate only through ports. Two nodes sharing mutable state outside the graph
  are unordered on the parallel executor: share through an edge, or make it one node.
- A cheap node declares its cost (`Shape::with_cost`); one that declares none is `Heavy`.
- Op code reaches the arena, event slots, units, rings and outputs only through
  `AudioSlots`/`EventSlots` or a claim, which keeps `tutti-graph` `#![forbid(unsafe_code)]`.
- A parallel render is bit-identical to the serial one; a parallel test compares against
  the serial render bit for bit.
- Never make memory safety depend on an exact float result (nodes run inside
  `with_no_denormals`).
- A node that changes width declares its buses; the graph never guesses a dry path. A node
  reads a non-main bus through the handle its shape issued.
- Summing is the graph's: several sources into one port sum. Never add a node to sum.
- A transport command meant for playback takes an `At`. A node reads the transport at a
  frame through `Env::transport_at`; a block is never split for it.

### The session

- `Graph::add` is the one way a node enters, and keys are typed (`Key<N>`). Wiring is
  `Graph::connect(from, to)` over ports taken from keys; params are methods of the key.
- **One failure rule:** a call that names a node or port that does not exist fails at the
  call with a named error, never later and never as a default. A port that goes missing
  after a successful call reads silence, except a tap whose every source was removed:
  it ends (`Pull::Ended`) once drained. Removal returns a `#[must_use] Result`.
- The session graph declicks wiring (`connect` fades in unless `.hard()`); a test that
  renders a session wire for another reason wires `.hard()`.
- An inlet whose rate is not the graph's is refused, never resampled.
- An app reads the playhead through `Session::playhead()`.

### The audio thread

- Never allocate, free, lock or wait on the audio thread. A full queue is counted, never
  waited on.
- Non-scalar state reaches the audio thread through `tutti_rt::{ArcLend, Reader, Loan}`;
  scalars use `Param<U>`. Nothing the audio thread reads is an `ArcSwap`.
- The audio thread never holds an owning handle to published state and never frees it.
  `publish` and `reclaim` are control-thread only.
- Register a `Reader` on the control side where the reader is built, and store it in the
  node. Lend once per block, never per sample, and never keep a `Loan` across blocks.
- Every cell's publisher calls `reclaim()` on a control-side tick it already has; a new
  cell joins its owner's sweep in the same change.

### Unsafe and runtime structure

- A library with no `unsafe` carries `#![forbid(unsafe_code)]` (`cargo xtask check-ffi`).
- Plugin FFI `unsafe` lives only in `tutti-clap-ffi`, `tutti-vst3-ffi`, `tutti-au-ffi` and
  `tutti-vst2-ffi`; the process edge's in `tutti-plugin-ipc-ffi`; MIDI's OS calls in
  `tutti-alsa-ffi`, `tutti-coremidi-ffi` and `tutti-winmm-ffi`. Everything above them forbids `unsafe`.
  Test-only `unsafe` is `tutti-fixtures`'.
- New lock-free or `unsafe` runtime structure goes in `tutti-rt`, with a loom model that
  runs the shipped code. The publish cell is the `arc-lend` crate from its own repository
  (github.com/tutti-audio/arc-lend); a change to it is made there.
- Store-load pairs use `SeqCst` fences, not `SeqCst` accesses.
- A platform difference sits behind one seam: the `cfg` chooses a platform module, never
  each item.

### Unit types

- Use a `tutti_types` unit, never a bare float, for any quantity one covers: fields,
  signatures, `Param<U>`, returns.
- A unit's operators are its whole algebra. Read the omission ledgers in `value/units/`'s
  tests before adding one, and ship a replacement with any omission.
- A number becomes a unit with `new` and leaves with `get`; there is no `From` between a
  unit and its raw. Convert between units with the named converters.
- Measurements are not controls: `Confidence`, `Correlation` and `StereoBalance` are readings.
- Types stop at C ABI, IPC and wire boundaries; say why in a comment there.
- Add a unit only for a distinct range or a distinct algebra.
### Range-checked values

- A range-checked type has the `MidiChannel` shape: `new(raw) -> Option` (never clamps),
  a const `of` that panics for literals, `get` and `From<Self> for Raw`, `TryFrom<Raw>`
  agreeing with `new`, serde through `try_from`, and named `clamped`/`masked` only where a
  hot path or the wire needs them. No public field, no infallible `From<Raw>`. Test it with
  `tutti_fixtures::check_range_contract`.
- Write a new range-checked type with `tutti_types::ranged!`, never by hand or with a
  crate-local macro; a hand-written one moves to it when its file is touched.
- Make a wrong value unwritable before adding a runtime check for it.

### Edges and buffers

- `Source`/`Sink` are edges over a `Sample` format: a checked `Interleaved`/`InterleavedMut`
  view, a runtime `ChannelLayout`, every count in frames (`Samples`). Cross to a slice
  length only with `Samples::interleaved_len` / `from_interleaved_len`.
- Width is runtime: never a const-generic channel count. A channel fold goes through
  `tutti_buffers::Fold`, built on the control side; a narrowing by index is refused, never
  truncated.
- Every `Source::pull` answers `Starved` (not yet) or `Ended` (never again) correctly, and
  `Failed` only after the frames before it.
- A copy out of the graph is a tap (`TapReader`, `EventTapReader`), a copy in is an inlet.
  Both count what they lose and never block.

### Plugins

- A main-thread entry takes `&tutti_plugin_types::MainThread`; holding one is the proof.
- A format host is a `Loaded` typestate whose `activate` splits a plugin into a realtime
  `Processor` and a main-thread `Controller`.
- The plugin-server is one `Served<F: OutOfProcess>`; VST2 is not served.
- `tutti_plugin_types::bundle` is the only code that knows a bundle's layout. Build a VST3
  UID with `inline_uid`.
- **The plugin contract** (`testing/tutti-plugin-contract`) runs the same cases against
  every format with the one test plugin, `testing/tutti-shared-plugin`, and its C++ VST3
  twin. Expected figures live in the contract's `spec`, never asked of the plugin. The
  plugin records what the host sent before it orders or repairs anything, and is
  configured only through its state. A case a format cannot pass is a `Gap` that must fail
  with its text; `gaps = N` is a ratchet. Never `#[ignore]`.

### Style

- Import from a crate's root, never through its modules. There are no preludes, and no
  crate re-exports another crate whole.
- An umbrella holds no logic: a piece with one owner goes to that crate, a piece composing
  several goes to `tutti-session`.
- A pure algorithm over slices goes in `tutti-dsp` (a measurement in `tutti-measure`); a
  node file holds only the `Node` adapter.
- A new trait states its boundary in one sentence without "and".
- An error type derives `thiserror::Error` with its text in `#[error(..)]`; a hand
  `impl Error for` is a row of the gate's allow-list with its reason (`cargo xtask
  check-errors`), and a `pub` type named `...Error` implements `Error`.
- Naming: an algorithm uses the theory name, a spec keeps its own, a *frame* is one value
  per channel, a *sample* is one value. A user-facing node API keeps the control word.
- A comment states what the code does and the invariant it holds, in the present tense;
  rustdoc documents the item. No history, no "why it was added", no removed names, no
  design-doc citations. A comment or doc that disagrees with the code is a bug.
- Before working around a constraint, name it in one sentence and verify it (read the
  manifest, grep, run the probe). An indirection whose only job is to avoid a change is
  the finding: make the change.

## Workspace

One workspace at the root; the `members` list is the source of truth. `crates/` is what
ships (directory name = package name, no nesting), `testing/` is never shipped, `xtask/`
holds the development commands (std only, no dependencies, `publish = false`), and
`vendor/` holds the VST3 SDK, which is never edited. No crate depends on Bevy; the Bevy
adapter is [tutti-audio/bevy-tutti](https://github.com/tutti-audio/bevy-tutti).

**Layering.** Normal and build dependencies point strictly down this list
(dev-dependencies may go anywhere); `cargo xtask check-layout` holds it:

0. `tutti-alsa-ffi`, `tutti-coremidi-ffi`, `tutti-winmm-ffi`, `tutti-ump`, `tutti-types`,
   `tutti-plugin-ipc-ffi`
1. `tutti-rt`, `tutti-midi-protocol`, `tutti-midi-device`, `tutti-curve`,
   `tutti-buffers`
2. `tutti-graph`, `tutti-dsp`, `tutti-modulation`, `tutti-metering`, `tutti-codec`,
   `tutti-tempo`, `tutti-voicing`, `tutti-sf2`, `tutti-midi-network`
3. `tutti-transport`, `tutti-plugin-types`, `tutti-measure`
4. `tutti-engine`, `tutti-nodes`, `tutti-midi-runtime`, `tutti-midi-file`, `tutti-clap-ffi`,
   `tutti-vst3-ffi`, `tutti-au-ffi`, `tutti-vst2-ffi`, `tutti-plugin-db`, `tutti-plugin-ipc`
5. `tutti-cpal`, `tutti-export`, `tutti-sampler`, `tutti-polysynth`, `tutti-soundfont`,
   `tutti-midi-hardware`, `tutti-clap-host`, `tutti-vst3-host`, `tutti-au-host`,
   `tutti-vst2-host`, `tutti-plugin-host`
6. `tutti-plugin`, `tutti-plugin-probe`
7. `tutti-plugin-server`, `tutti-session`
8. `tutti`

- Nothing under `crates/` has a normal or build dependency on `testing/`.
- Only `tutti` depends on `tutti-session`, and nothing depends on `tutti`.
- A crate keeps its modules private and is named from its root; `check-paths` enforces it.
- `check-layout` also holds each crate's limits on its own dependencies: which workspace
  crates it may name, and which modules alone may name a wrapped library (`symphonia` in
  `tutti-codec`'s decode module, `winnow` in the listed parser modules).
- A codec format is a cargo feature (`encode-*`, `decode-*`), none on by default; a decode
  feature pulls no encoder, and the C Vorbis library is `encode-vorbis`'s alone.
