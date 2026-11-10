# 021: plugin format and plugin hosting are two axes

A plugin has a *format* (CLAP, VST3, VST2, AU) and a *hosting* (in this process,
or in a `plugin-server` process behind IPC). The code fuses them: IPC is written
as "the thing that hosts CLAP, VST3 and AU", and in-process as "the thing that
hosts VST2". This design separates the axes. Each format implements the
`tutti-plugin-types` traits once, each hosting is generic over a format, and two
marker traits, in the manner of `Send` and `Sync`, say which hostings a format
supports. `OutOfProcess<Vst2>` then does not compile.

Builds on [016](016-plugin-hosts.md) (`Loaded`, `Processor`, `Controller`,
`Lifecycle`). Where 016 §2.4 makes the server dispatch over an `AnyPlugin` of four
formats, this design makes it dispatch over the formats that can be served.

## Summary

- **Two axes.** Format: `impl Loaded` in each `-host` crate, as 016 has it.
  Hosting: `InProc<F: InProcess>` and `Ipc<F: OutOfProcess>`, both of which are
  themselves `Loaded`. One graph node, `PluginNode<H: Loaded>`, one fork and one
  parameter-delivery path serve both (§2).
- **Markers.** `InProcess` and `OutOfProcess` are safe, explicit opt-in traits on a format's
  `Loaded` type. A host bounds on its marker. VST2 implements `InProcess` and not
  `OutOfProcess` (§3, §4).
- **The server is generic.** It runs any `F: OutOfProcess` behind the protocol; the four
  per-format loaders (7.7k lines) and the server's capability traits go (§2.3).
- **Editor placement is its own concern**, carried by `OutOfProcess::Editor` (§5).
- **Probing is its own thing.** A separate `tutti-plugin-probe` binary reads a
  plugin's descriptor and exits; it depends on neither hosting. `plugin-server`
  drops probe mode and VST2 entirely (§2.5).
- **Three new crates**, `tutti-plugin-ipc`, `tutti-plugin-host` and
  `tutti-plugin-probe`; `tutti-plugin` becomes the facade and the one place that
  names formats and hostings together (§6).
- **VST2 loses IPC hosting; CLAP, VST3 and AU gain in-process hosting** (§9).

## 1. Current state

### 1.1 Sizes (this branch, lines of `.rs` including tests)

| Piece | Where | Lines |
|---|---|---|
| IPC client (bridge, audio thread, composite control side) | `tutti-plugin/src/host/ipc_client/` | 6,332 (4,799 without tests) |
| Pipelining `Batcher` (IPC only) | `host/node/batcher.rs` | 761 |
| Subprocess launch, probe, bundle | `host/subprocess/` | 1,655 |
| Protocol, shm transport, state chunks | `protocol/` + `util/transport/` | 2,125 + 3,260 |
| The graph node, fork, param delivery, MIDI intake, meter, transport source | `host/node/` | 11,273 (8,242 without tests) |
| Handles (`PluginHandle`, `Host*` capability traits) | `host/handles/` | 1,416 |
| In-process VST2 (node, fork, client, control backend, instance, loader) | `format/vst2_in_process/` | 2,274 |
| Editor as a second dlopen, host side | `format/gui/{clap,vst3,au}.rs` | 886 |
| Server session, audio pipeline, transport, server | `tutti-plugin-server/src/` | 5,101 (+ `plugin.rs`, 752) |
| Server loaders | `tutti-plugin-server/src/loaders/` | 7,689 (clap 2,876, au 2,121, vst3 1,545, vst2 1,044) |
| Format hosts | `tutti-{clap,vst3,vst2,au}-host` | 12,030 / 11,600 / 7,008 / 16,278 |

### 1.2 Two fused axes

`host/plugin.rs` is the join. `Backend::{Subprocess(Box<PluginClient>),
InProcessVst2(Box<InProcessVst2Client>)}` is chosen by `opens_in_process(path)`,
which is `cfg!(feature = "vst2") && format == Vst2`. So "which format" and
"where it runs" are one decision, and each hosting is welded to the formats it
serves:

- The IPC side names no format, but its only editor path (`format/gui`) is a
  per-format `match` over `vst3`, `clap` and `au`, and refuses VST2 by name.
- The in-process side names one format: `client.rs` imports
  `tutti_vst2_host::{ChannelLayout, PluginInfo, Vst2ProcessContext}` and the node,
  fork and control backend are written against `Vst2Loaded`.
- The server holds a second, parallel set of per-format adapters
  (`PluginMeta`, `PluginAudio`, `PluginParams`, `PluginState`, `PluginPresets`,
  `PluginEditorHost`, `PluginInstance`; `loaders/*.rs`) instead of running
  `Loaded`. Doc 016 §4 already schedules these for deletion; the loaders for
  VST2 already drive a `Lifecycle<Vst2Loaded>` and "present it through the
  capability traits the server still dispatches on".

### 1.3 What is and is not duplicated between the two nodes

Less than it looks. `vst2_in_process` already imports `host::node`'s
`ParamDelivery`, `param_input`, `MidiIntake`, `SegmentPoints`, `PluginMeter`,
`DeliveryCounters` and `transport_source`. Those are shared. What is written
twice:

| Concern | IPC | In-process VST2 |
|---|---|---|
| `tutti_graph::Node` impl and `IntoNode` | `host/node/graph_node.rs` (596) | `vst2_in_process/node.rs` (504) |
| The client that drives a block | `host/node/{mod,process}.rs` + `ipc_client/audio/` | `vst2_in_process/client.rs` (675) |
| Fork by state transfer | `host/node/fork.rs` (576), new server per fork | `vst2_in_process/fork.rs` (269), second `dlopen` |
| Control surface behind `PluginHandle` | `ipc_client/composite.rs` (748) | `vst2_in_process/control_backend.rs` (246), both behind `HostParams`, `HostState`, `HostEditor`, `HostRenderMode` |
| Loading and state-before-activate | `host/plugin.rs` + `PluginClient::new_with_state` | `vst2_in_process/loader.rs` (168) |

The in-process plugin also keeps its instance behind `Arc<Mutex<_>>` with an
audio-thread `try_lock` that falls back to silence (`contention_count`), a
workaround for the split 016 now provides in the type system.

### 1.4 Editors per path

| Path | Where the editor lives | Evidence |
|---|---|---|
| IPC, CLAP / VST3 / AU | A **second `dlopen`** of the plugin in the host process, editor only. Parameters from the editor are polled and forwarded to the audio instance; the host mirrors `set_state` into it | `format/gui/mod.rs`, `composite.rs:241-340`, `close_editor_inner`, `editor_idle` |
| In-process VST2 | The audio instance itself, under the instance lock | `control_backend.rs` (`HostEditor`) |
| IPC VST2 | **None.** `load_gui_instance` returns `Err("VST2 plugins use the in-process backend")` | `format/gui/mod.rs` |
| Server window | `HostMessage::OpenEditor { parent_handle }` and `Session::handle_open_editor` embed in a host-supplied native handle. **Nothing in the host sends it**: the only references are the enum, the handler and a unit test | `grep OpenEditor` over `crates/` |

Two further facts. The second-dlopen editors are `cfg(unix)` for CLAP and VST3
(`gui/mod.rs`), so on Windows an out-of-process CLAP or VST3 plugin has no
editor at all (read from the `cfg`s; worth confirming on Windows). And the
mirror is lossy: only *parameter* changes flow from the editor instance to the
audio instance. A GUI-only state edit (a patch browser, a sample load) does not
reach the audio instance, because `editor_idle` and `close_editor_inner` poll
`poll_gui_param_changes` and nothing pulls state back.

### 1.5 The VST2 contradiction, resolved

`tutti-plugin/Cargo.toml` says "VST2 is always in-process … There is no
subprocess VST2 path". `tutti-plugin-server` has `default = ["vst2", …]` and a
1,044-line `loaders/vst2.rs`. Both are partly right:

- **The server fully supports VST2.** `Plugin::Vst2(Vst2Instance)` is an arm of
  the server's format enum; `tutti-plugin-contract/tests/server.rs` runs
  `vst2_mode_and_transcript_cross_the_process_boundary` through a real
  `plugin-server`; `session.rs` has `an_idle_drain_does_not_allocate_vst2`.
- **Production reaches it in two ways.** (a) The probe. The scanner is a
  thread in the host (`tutti-plugin-db`'s `Scanner::spawn`); each probe spawns a
  short-lived process per plugin file, today the `plugin-server` binary with
  `TUTTI_PROBE_MODE=1` (`host/subprocess/probe.rs`), which calls
  `Vst2Instance::probe`, a full `Vst2Loaded::load` and drop (the code says VST2
  has no cheaper probe), reports over the hosting socket and is killed. That is
  crash isolation *at scan time*, and it is worth keeping, but it has nothing to
  do with hosting (§2.5). (b) `PluginClient::new` on a `.vst` path. `Plugin::open`
  routes VST2 in-process only when `tutti-plugin`'s `vst2` feature is on
  (`opens_in_process` is `cfg!(feature = "vst2")`); a build without it sends the
  path to the server, which renders audio but cannot show an editor (§1.4).
- `bevy-tutti`'s `vst2` feature turns both on, so for the Bevy path the server's
  VST2 *hosting* arm is reachable only from tests and (b).

So the Cargo.toml comment is true of `Plugin::open` with the feature on and
false of the crate: there is an undocumented, editorless subprocess VST2 path.
This design makes the comment true by construction.

**History.** `26d748df7` (2026-06-12, "make VST2 always in-process, drop
vst2-in-process feature") records the reason: `AEffect` fuses editor and audio,
so the editor cannot live in another process, and the subprocess path was never
a real implementation behind `vst2`. The server's VST2 audio passes the contract
case, but without an editor it is not usable for a real VST2. The server's VST2
*hosting* is therefore a leftover, not a feature.

## 2. The target

### 2.1 Traits (in `tutti-plugin-types`, beside `Loaded`)

```rust
/// The format can run in the host's own process.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be hosted in this process",
    note = "implement `InProcess` only if the plugin can be loaded and run here (design 021 §3)"
)]
pub trait InProcess: Loaded {
    fn load_in_process(path: &Path, load: &Load) -> PluginResult<Self>;
}

/// The format can be served from another process, with its editor elsewhere.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be hosted over IPC",
    note = "its editor cannot run apart from its audio instance (design 021 §4)"
)]
pub trait OutOfProcess: Loaded {
    /// An editor-only instance in the host process (§5).
    type Editor: Editor;
    /// What the server calls to build the audio instance.
    fn load_served(path: &Path, load: &Load) -> PluginResult<Self>;
    /// What the host calls to build the editor instance.
    fn load_editor(path: &Path) -> PluginResult<Self::Editor>;
}
```

`Load` is the load-time value both need today (sample rate, block, state to
restore before activation, `prefer_f64`). `Editor` is 016 step 5's safe editor
trait, plus the three calls the second instance needs: `set_parameter`,
`set_state`, `poll_edits`.

### 2.2 Hostings (in `tutti-plugin-host` and `tutti-plugin-ipc`)

```rust
pub struct InProc<F: InProcess>(F);                       // tutti-plugin-host
impl<F: InProcess> Loaded for InProc<F> { /* delegates; Processor = F::Processor */ }

pub struct Ipc<F: OutOfProcess> { bridge: Bridge, editor: Option<F::Editor>,
                             _f: PhantomData<fn() -> F> }  // tutti-plugin-ipc
impl<F: OutOfProcess> Loaded for Ipc<F> {
    type Processor = IpcProcessor;        // submits a chunk, plays the last (the Batcher)
    type Controller = IpcController<F>;   // requests over the control socket
    const RESTARTS: RestartRules = F::RESTARTS;
    /* activate / deactivate / rebuild are protocol round trips */
}

pub struct PluginNode<H: Loaded + Reopen> { /* ParamDelivery, MidiIntake, SegmentPoints,
                                              meter, transport source, H::Processor */ }
impl<H: Loaded + Reopen> tutti_graph::Node for PluginNode<H> { /* written once */ }
impl<H: Loaded + Reopen> tutti_graph::IntoNode for PluginNode<H> { /* + ForkSource */ }

pub trait Reopen: Loaded { fn reopen(&self) -> PluginResult<Self>; }   // same load, same hosting
```

Both hostings are `Loaded`, so everything written against `Loaded`
(`Lifecycle<H>`, the node, the fork, the contract) takes either. `Ipc`'s
`Processor` hides the pipeline: chunk, shm slab, one chunk of latency declared to
PDC (`Shape::latency`, as today). `InProc` adds none.

**Fork** is `reopen` + `save_state` + `load_state` + `prepare` and is written
once in `PluginNode`: `InProc::reopen` loads the module again here,
`Ipc::reopen` starts a new server (what `fork.rs` does today). The 845 lines
of `fork.rs` and `vst2_in_process/fork.rs` become one.

**Parameter delivery** is already shared below the node (§1.3). The node builds a
`ProcessContext` (automation, MIDI, transport) and calls `Processor::process`;
`IpcProcessor` serialises that context into the slab, `F::Processor` consumes it.

### 2.3 The server

`Session<P: Hosted>` (016 §2.7) is instantiated at `Served<F: OutOfProcess>` for each
served format. The per-format loaders reduce to rows of a table:

```rust
served! { Clap(ClapLoaded), Vst3(Vst3Loaded), Au(AuLoaded) }   // macro: arms of AnyServed
```

`served!` calls `F::load_served`, so a row for a non-`OutOfProcess` format does not
compile. The server drops its dependency on `tutti-plugin`, takes
`tutti-plugin-ipc`, and **drops probe mode and VST2 entirely**
(`tutti-vst2-host` leaves its dependencies). It is only `Ipc` hosting over
`OutOfProcess` formats.

### 2.5 Probing

Probing is neither hosting. If it stayed in the server it would stay coupled to
the IPC protocol (`Ready`, `ProbePlugin`, `PluginLoaded` over the hosting socket)
and to the set of served formats (which excludes VST2).

- **A binary, `tutti-plugin-probe`.** `plugin-probe <path> <result-file>`: load
  the plugin just far enough to read its `PluginDescriptor`, write the result,
  exit. It depends on the four format `-host` crates (features) and
  `tutti-plugin-types` (the descriptor types) and on neither `tutti-plugin-ipc`
  nor `tutti-plugin-host`.
- **Its wire is its own.** The result is a file: the probe writes the serialised
  outcome (descriptor, or a typed error) to `<result-file>.tmp` and renames it
  into place; the host passes the path, waits for exit with a timeout, kills on
  timeout, then reads it. Recommended over stdout and over a socket. Stdout is
  not safe, because plugins print from their init code (the probe redirects
  stdout and stderr to null, so a plugin that writes cannot corrupt a result
  that does not travel there). An inherited extra fd is not portable to Windows.
  A socket needs the handshake and retry loop that `probe.rs` has today
  (`connect_with_retry`, `Ready`, version check) for one reply. With a file the
  outcomes are unambiguous: file present means the probe finished (descriptor or
  load error); no file with a signal or non-zero exit means it crashed; no file
  at the timeout means it hung.
- **The probe side.** `probe_metadata`, `ServerLocator` and the probe half of
  `host/subprocess/` become a `ProbeLocator` and a short spawn-wait-read function
  in `tutti-plugin` (about 150 lines, replacing `probe.rs`'s 110 plus the
  socket handshake); `tutti-plugin-db`'s `Scanner` still takes the probe as a
  parameter. **The blacklist, pedal and crash-history logic stays in
  `tutti-plugin-db`, unchanged**: it sees the same `ProbeFailure` cases.
- **Fixtures.** `tutti-fixtures` gains `plugin_probe()`, like `plugin_server()`,
  found in the test binary's own profile directory. Absence is a failure, never
  a skip. It is not a dev-dependency of the crates whose tests spawn it, for the
  reason `plugin_server` is not. CLAUDE.md's "build `plugin-server` first" note
  becomes "build `plugin-server` and `plugin-probe` first" for the tests that
  scan (`tutti-plugin`'s `db_probe`, `tutti-session`'s `plugin` suite).

**As built (step 4c).**

- **The crate.** `crates/tutti-plugin-probe`, binary `plugin-probe <path>
  <result-file>`, layer 6, `#![forbid(unsafe_code)]`. Features `clap`, `vst3`,
  `vst2` (all default). It depends on `tutti-plugin-types` and the three format
  hosts and on no hosting; `check-layout.sh` holds both that and "nothing depends
  on it". It calls `ClapLoaded::probe`, `Vst3Loaded::probe` and, for VST2,
  `Vst2Loaded::load` (a load without activation, the instance dropped), then
  `descriptor_of` / `Control::info`, so the loaders' own `probe` functions were
  deleted rather than moved. Exit status is `0` once a result is written (a
  plugin that would not load is a result), `2` for a bad command line, `3` if
  the file could not be written.
- **The wire** is `tutti_plugin_types::{ProbeOutcome, write_result, read_result}`
  (feature `probe-wire`, which `tutti-plugin` and the probe both turn on): a
  framed bincode payload (magic and version, length, bytes) staged at
  `<result>.tmp` and renamed. The frame makes a file cut short some other way a
  refused read (`ResultError::Truncated`), not a prefix decoded as far as it
  goes. The failure carries the loader's message only; the stage a host crate
  tagged it with is not on the wire (the old server wire did not carry one
  either, and `LoadFailed` reports `Opening`).
- **The redirect is the host's.** The probe cannot null its own stdout and
  stderr without `unsafe`, so `tutti-plugin` spawns it with both on the null
  device. Nothing a plugin prints can reach a result, or fill a pipe.
- **The probe side** is `host/subprocess/probe.rs` (about 120 lines of code, tests aside):
  `ProbeLocator` (`TUTTI_PLUGIN_PROBE`, beside the executable, `PATH`; `at` for
  a test), spawn, poll `try_wait` to the caller's deadline (`DEFAULT_PROBE_TIMEOUT`, fifteen
  seconds, spawn and the plugin's load included; the old socket probe allowed
  about twelve), kill and reap on timeout, then read the file. The child sits in a
  `Reaped` guard that kills and waits on drop, so a panic cannot orphan it, and
  the result lives in a private `tempfile` directory (`create_new` for the
  staging file), not a name in the shared temp directory. `catalog::probe_record_with`
  takes the `ProbeLocator` and the deadline; `probe_record` is the defaults. The file is read after any exit, so a probe that
  finished at the deadline counts. No file and an exit is
  `BridgeError::ProcessCrashed`; no file at the deadline is `Timeout`; a file that
  is not whole is `IpcError` (not blacklistable: it is a fault of ours, not
  evidence about the plugin). `BridgeError::ProbeNotFound` replaces
  `ServerNotFound` as the one error a scan answers with filename metadata.
  `probe_plugin` and `probe_record` are switched; `Scanner` and `ProbeFailure`
  are untouched.
- **Deleted from the server:** `HostMessage::ProbePlugin` and its session arm
  and `handle_probe`, `Plugin::probe`, the four `*Instance::probe` functions and
  `a_probe_does_not_activate`, which moved to the probe crate
  (`tests/vst2_not_activated.rs`, through the crate's `describe`; the crate is
  also a library so a test can call it in-process). There was no `TUTTI_PROBE_MODE` in the server itself: it was set by the
  host and never read, the server serving a probe as an ordinary request. Removing
  a variant from the request enum renumbers the tags after it, so
  `PROTOCOL_VERSION` is 21 and the tag test pins `LoadPlugin` as 0.
- **Tests.** `tutti-plugin-contract`'s `probe` suite (`--features server`) runs
  the real binary on the shared plugin in each format, and on the CLAP plugin
  under file names its `init` reads (`init-crash`, `init-hang`, `init-print`,
  `init-refuse`): a crash, a hang, a plugin that prints two thousand lines to
  each stream, and a refusal. The other formats' plugins have no such mode; a
  probe sends no state and no environment variable may configure a plugin. The
  host's own cases (kill at the deadline, no file after an exit or a signal, a
  partial file) run in `probe.rs` against shell stand-ins (Unix only).
- **Corrections to this document.** `tutti-session`'s `plugin` suite does not
  scan; it needs `plugin-server` and not `plugin-probe`. The suites that need
  `plugin-probe` are the contract's `probe` suite and `db_probe`'s opt-in test.
- **AU** joins by a `"component"` row in `describe` once `AuLoaded` exists,
  with `tutti-au-host` behind an `au` feature (macOS only); no AU code is
  in the crate until then.

### 2.4 Runtime selection stays typed

A host picks hosting per plugin at runtime from a path. The facade holds one
table from which both the enum and the constructors are generated:

```rust
hosts! {
    Clap(ClapLoaded): [InProcess, OutOfProcess],
    Vst3(Vst3Loaded): [InProcess, OutOfProcess],
    Au(AuLoaded):     [InProcess, OutOfProcess],
    Vst2(Vst2Loaded): [InProcess],
}
pub enum AnyHost { ClapInProc(PluginNode<InProc<ClapLoaded>>), ClapIpc(PluginNode<Ipc<ClapLoaded>>),
                   Vst2InProc(PluginNode<InProc<Vst2Loaded>>), /* … */ }
pub enum Hosting { Auto, Local, Isolated }
pub fn open(path, hosting: Hosting, …) -> Result<AnyHost, HostingRefused>
```

Each row's `OutOfProcess` entry expands to `Ipc::<$L>::open`, so adding `OutOfProcess` to the
VST2 row is a compile error, not a runtime refusal. At runtime, `Isolated` on a
VST2 path returns `HostingRefused { format: Vst2, wanted: Isolated }`. `Auto`
picks `OutOfProcess` where the row has it, else `InProcess`. This replaces `Backend`
(`host/plugin.rs`), `opens_in_process` and, in 016, the server-side `AnyPlugin`.
`Plugin` keeps its public accessors and reports `hosting()`.

## 3. The markers

### 3.1 Names

The marker is a fact the format states, like `Send`; the host is the thing that
requires it, like `thread::spawn`. A trait and a struct cannot share a name, so
each layer has its own: markers `InProcess` and `OutOfProcess` (both `: Loaded`,
in `tutti-plugin-types`), hosts `InProc<F: InProcess>` and `Ipc<F: OutOfProcess>`.
The refusal reads "the trait bound `Vst2Loaded: OutOfProcess` is not satisfied".

| Option | For | Against |
|---|---|---|
| **Both layers: markers `InProcess` / `OutOfProcess`, hosts `InProc<F>` / `Ipc<F>`** | A format-agnostic bound exists (`F: OutOfProcess`), so the VST2 rule is a fact about the format and not code in a host. Each host has a type to hold its own state (`Ipc`: bridge, editor instance; `InProc`: nothing) | Two names per axis |
| Host types alone (`Ipc<F: Loaded>`, the rule inside `Ipc::new`) | One name per axis | No bound a generic caller (the server table, the contract) can write; the VST2 rule sits in the host, as a runtime check or an ad hoc where clause per use |
| Markers alone (no host types) | The rule is a fact about the format | No type to hold the per-hosting state, so the node would carry a hosting enum again (`Backend` by another name) |

### 3.2 What each promises

| Marker | Promise | Obligation on the impl |
|---|---|---|
| `InProcess` | The host can load the module into its own process and run `Processor` on a host thread. Nothing else: no isolation, no crash recovery, load and drop on the main thread where the format needs it | `load_in_process` honours the format's own thread rules (VST2: main thread, §7) |
| `OutOfProcess` | (1) the audio instance runs in another process; (2) an editor-only second instance of the same module can run elsewhere and show what the audio instance holds; (3) **state and parameters cross the protocol only by id and by blob**: no pointer, handle or callback of the plugin's survives the boundary. Where the editor window lives is `Editor`'s concern (§5) | `Editor` keeps the second instance faithful through parameters (both directions, with gestures), the state blob (audio to editor at open and on change; editor to audio when the editor says it changed) and host-relayed messages where the format has them |

Does any format lack `InProcess`? **No.** CLAP, VST3 and VST2 are dlopen'd by the
host today (the second-dlopen editors already do it for the first two). AU:
AUv2 components always load in-process. AUv3 is an app extension that the OS
hosts out of process by default; the host can ask to load it in the host with
`kAudioComponentInstantiation_LoadInProcess`, which on macOS needs the
extension's architecture to match the host and, for a hardened-runtime host, the
library-validation exemption (`com.apple.security.cs.disable-library-validation`)
because third-party code is then mapped into the host; on iOS it is not
available to a host for another app's extension. From the AU host's source (it
instantiates synchronously and `error.rs` already names "component requires
asynchronous instantiation (AUv3 with a view)") it passes no such flag. These OS
rules are from Apple's documentation and **must be confirmed on the AU lane**.
Consequence: `AuLoaded: InProcess` is implemented, and `load_in_process` may
*refuse at runtime* with a typed error for an AUv3 that will not load in
process. The marker is a statement about the type; per-plugin refusal stays a
`PluginResult`. Since every format is `InProcess`, that marker earns its keep as
the home of `load_in_process`, the `Debug`/`hosting()` identity, and the
symmetric bound; it prevents nothing today.

### 3.3 Safe, not `unsafe`

`Send` and `Sync` are `unsafe trait` because a wrong impl is a data race the
compiler cannot see. A wrong `OutOfProcess` impl is a behaviour bug: an editor that
drifts from its audio instance, or state that fails to round-trip. Neither is
undefined behaviour, and `unsafe` would force a `// SAFETY:` on correctness
claims that no reviewer can check mechanically. So both are safe traits.
Instead of `unsafe`:

- `on_unimplemented` messages (above) say why a format is refused.
- `contract!` takes the hostings (§8): an `impl OutOfProcess for X` with no
  `hostings = [.., OutOfProcess]` line in the contract suite is caught by a new
  `scripts/check-hosting.sh` (greps `impl .* (InProcess|OutOfProcess) for`, requires a
  matching `contract!`), the same pattern as `check-range-types.sh`.
- The traits are not sealed: an embedding application with its own format can
  implement them. The doc says an impl must pass the hosting cases.

Stable Rust has no auto traits and no negative impls (`impl !OutOfProcess for Vst2Loaded`
is unstable). The markers are therefore explicit opt-in, unlike `Send`: a
format is `InProcess` and `OutOfProcess` only by an `impl` in its `-host` crate, and a new
format that writes neither is hostable nowhere, which fails closed. The absence
is documented on `Vst2Loaded` (a doc paragraph and the compile-fail test), since
an absent impl has no place to carry a comment.

### 3.4 Compile-fail test plan

The repo already uses `compile_fail` doctests (`arc-lend`) and has no
`trybuild`; stay with doctests. In `tutti-plugin` (the one crate that sees formats
and hostings; `tutti-vst2-host` is a dev-dependency there):

```rust
/// ```compile_fail,E0277
/// use tutti_plugin::Ipc; use tutti_vst2_host::Vst2Loaded;
/// fn host() -> Option<Ipc<Vst2Loaded>> { None }
/// ```
```

plus one for `served!`/`hosts!` (a table row `Vst2(Vst2Loaded): [InProcess, OutOfProcess]`),
and one positive twin per format that compiles, so the failing test cannot pass
for an unrelated error. The `E0277` pin is the guard against "fails for the wrong
reason". **Mutation record, written on the test:** add `impl OutOfProcess for Vst2Loaded`
and the doctest unexpectedly compiles and fails; change the test's type to a typo
and it fails with a different code (`E0412`) and the pin fails it; remove the
`F: OutOfProcess` bound from `Ipc` and it compiles and fails.

## 4. Why VST2 lacks `OutOfProcess`

The property is not "a second instance can open a window"; any format can load
twice. It is that **a second instance can be kept faithful using only what the
format lets a host observe**. Per format:

| Needed | CLAP | VST3 | AU | VST2 |
|---|---|---|---|---|
| Edit to the audio instance by parameter, with gesture | param events in/out | `performEdit` + `beginEdit/endEdit` | parameter listener | `audioMasterAutomate`; `audioMasterBeginEdit/EndEdit` only from 2.3, optional |
| Host learns that non-parameter state changed | `clap_host_state.mark_dirty` | `restartComponent`, `IComponentHandler2::setDirty` | property listener | **none**: a patch browser or sample load is silent |
| A sanctioned channel from processor to editor | extension messages | `IConnectionPoint` (host relays) | properties | **none**: the editor reads the `AEffect`'s own memory |
| Spec separates editor from processor | no (`gui` is on `clap_plugin`; tutti's two-instance scheme is a convention that works for plugins whose editor state is params + state) | yes (component and controller are separate, may be distributed) | designed to (AUv3 runs view and unit apart) | **no**: `effEditOpen` is on the same `AEffect` that `processReplacing` runs on |

For VST2 the editor and the DSP object are one thing, and the host has no
notification that tells it when a second copy has gone stale. The workaround,
polling `effGetChunk` and comparing, calls the dispatcher from a second thread
beside `processReplacing`; 016 §2.10 records that some VST2 plugins crash or
corrupt under exactly that. Meters, scopes and analyzers in a VST2 editor read
the audio object, so an editor-only instance shows silence.

Be honest about the limit of this argument. It is a *judgement* about what the
host can know, not a physical impossibility, and CLAP's and AU's own
separability is weaker than VST3's: the current second-dlopen already shows
state-only editor changes do not flow back (§1.4). The marker therefore asks the
format to meet the table above, and §10 (Q8) makes the lossy state path part of
the promise before the marker is stated for CLAP and AU. VST2 fails the table at
two rows.

### 4.1 IPC with the editor in the server

A `plugin-server` that opens VST2's editor on the same `AEffect` as the audio
has no divergence problem. It gives crash isolation of the DSP and a faithful
editor. It fails elsewhere:

| Platform | Embedding a server-owned editor in a host window | Floating server window |
|---|---|---|
| X11 | Works: the parent is an XID, global to the X server (`XReparentWindow` / XEmbed; what the unused `OpenEditor` assumes) | Works |
| Wayland | **Impossible**: no embedding of a foreign client's surface; `xdg-foreign` only parents toplevels. XWayland only | Works as a separate toplevel |
| Windows | Possible: `SetParent` across processes is legal, but input queues are attached, so a blocked server hangs the host's UI thread; DPI contexts and focus differ | Works |
| macOS | **Impossible** with public API: an `NSView` cannot be hosted from another process (`CALayerHost` is private). `plugin-server` is also a plain child process, not an app with a WindowServer-backed run loop | Needs the server to become an app |

And a crash in the editor then takes down the audio instance, which is half of
what IPC was for. It would satisfy the *isolation* aim; it does not satisfy the
owner's rule, because the editor is not a separate instance but the audio
instance. It is a different capability, `ServerEditor`, which every format has
and only VST2 would use. Recommendation (Q2, Q4): the rule stands as stated;
`ServerEditor` is not added now; the unused `OpenEditor` path is deleted in the
cleanup step (git has it). If a user needs isolated VST2, it returns as a marker
of its own, floating window only, with the macOS and Wayland rows documented.

## 5. Editor placement

Editor placement is a property of the `OutOfProcess` marker's `Editor`, not of the node.

| Hosting | Editor | Owner |
|---|---|---|
| `InProc<F>` | The audio instance. `F`'s `Controller` opens it (016 step 5 `Editor`) | the format |
| `Ipc<F: OutOfProcess>` | `F::Editor`, a second instance loaded in the host process by `F::load_editor`, fed parameters and state as today | the format's `-host` crate |

Today's `format/gui/{clap,vst3,au}.rs` (886 lines, one `PluginEditor` trait in
`tutti-plugin`) move into the `-host` crates as the `OutOfProcess::Editor` impls, next to
the code that already wraps each format's editor calls; `tutti-plugin`'s `vst3`,
`clap` and `au` features stop meaning "in-process GUI". The `cfg(unix)` gating of
the CLAP and VST3 editors is revisited there (Windows has none today). The
mirror gains the missing direction: on close and on `mark_dirty`-style signals,
`IpcController` asks the editor instance for its state and, if it differs,
`load_state`s it into the audio instance (Q8).

## 6. Crates and layering

Aim: formats do not know hostings; hostings do not know formats. A crate
boundary is the only enforcement that holds (today `vst2_in_process` names
`tutti_vst2_host` from inside `tutti-plugin`, and a `cfg(feature)` is all that
separates it from the IPC code).

### 6.1 Proposed

| Crate | Contents (today's path, lines) | Depends on | Layer | `forbid(unsafe_code)` |
|---|---|---|---|---|
| `tutti-plugin-types` (exists) | adds `InProcess`, `OutOfProcess`, `Editor`, `Load`, `Reopen` (~300 new) | none | 3 | yes (today) |
| **`tutti-plugin-ipc`** (new) | `protocol/` (2,125), `util/transport/` (3,260), `host/ipc_client/` (6,332), `host/subprocess/` (1,655), `host/node/batcher.rs` (761), `error.rs` (476), `util/config/` (317); `Ipc`, `IpcProcessor`, `IpcController`, the server-side halves of the slab and framing | `tutti-plugin-types`, `tutti-types`, `tutti-rt`, `tutti-ump`, `tutti-midi-types` (+ serde, bincode, memmap2, interprocess) | 4 | **no**: shm, mmap and process launch (`slab.rs`, `header.rs`, `launch.rs`, `control.rs`). The only crate with such `unsafe`, each block with a `// SAFETY:` |
| **`tutti-plugin-host`** (new) | `host/node/` less the batcher (about 7,500 + tests), `host/handles/` (1,416) reworked over `Control`, the reusable parts of `vst2_in_process` (node, client, fork, about 1,450 merged into the above), `host/param_values.rs`, `liveness.rs`, `render_mode.rs`, `util/node`, `util/window`; `PluginNode<H>`, `InProc<F>` (~300) | `tutti-plugin-types`, `tutti-graph`, `tutti-transport`, `tutti-midi-runtime`, `tutti-midi-types`, `tutti-types`, `tutti-rt`, `tutti-ump` | 5 | yes, once `os_monotonic_ns` (`transport_source.rs:430-465`, a `clock_gettime` / `QueryPerformanceCounter` call, 35 lines) moves to `tutti-rt` |
| **`tutti-plugin`** (shrinks) | the facade: `plugin.rs` (472, becomes the `hosts!` table), `plugins.rs` (1,026), `db_probe.rs` (279), `lib.rs`; `AnyHost`, `Hosting`, `open` | `-host`, `-ipc`, `tutti-plugin-db`, and the four format hosts behind features | 6 | yes |
| **`tutti-plugin-probe`** (new, bin) | `main.rs` (~150) and the descriptor read per format, which `Vst2Instance::probe`, `ClapInstance::probe`, `Vst3Instance::probe` and `AuInstance::probe` (inside `loaders/`) do today | `tutti-plugin-types`, the four format hosts behind features (not `-ipc`, not `-host`, not `tutti-plugin`) | 6 (above the layer-5 format hosts; nothing depends on it) | yes: every host it calls forbids `unsafe` and the binary adds none |
| `tutti-plugin-server` (shrinks) | `session.rs` (2,111), `audio_pipeline.rs` (1,406), `transport.rs` (649), `server.rs`, `lib.rs`; the `served!` table; probe mode removed; `loaders/` (7,689) gone | `-ipc`, `tutti-plugin-types`, the CLAP, VST3 and AU hosts behind features (not VST2, not `-host`, not `tutti-plugin`) | 7 (could be 6; leave) | yes, once the loaders' `unsafe impl Send` and `WindowHandle::from_u64` go and the `libloading` tests move to the contract |
| `tutti-{clap,vst3,vst2,au}-host` | gain `impl InProcess`, `impl OutOfProcess` (not VST2), and `OutOfProcess::Editor` | `tutti-plugin-types` only (unchanged) | 5, 5, 4, 5 (`tutti-au-host` moves from 4 to 5 over the new `tutti-au-ffi`, layer 4; PRs #350 and #353) | yes (unchanged); the `-ffi` crates keep the `unsafe` |

The ordering is lower-to-higher with no new upward edge: `-ipc` (4) is above
`tutti-plugin-types` (3); `-host` (5) is above `tutti-midi-runtime` (4);
`tutti-plugin` (6) is above both; `tutti-plugin-server` and `tutti-session` stay
at 7; `tutti` and `bevy-tutti` at 8. **`tutti-plugin` stays at layer 6, so
`tutti-session` and the umbrellas need no change**, and no new crate depends on
`tutti-session` (`check-layout.sh` line 393 holds).

### 6.2 What breaks in the scripts

| Script | Change |
|---|---|
| `check-layout.sh` | Add both crates to `LAYERS` (4 and 5). New rules, in its `only(...)` style: `-ipc` and `-host` depend on no `tutti-*-host` format crate; no format host depends on `-ipc` or `-host`; `tutti-plugin-server` depends on neither `-host` nor `tutti-plugin`. It fails an unplaced crate, so the crates land with their rows |
| `check-plugin-ffi.sh` | It scans `tutti-plugin-server/src/loaders/<fmt>.rs` and `tutti-plugin/src/format/gui/<fmt>.rs`. Both disappear; point it at `tutti-<fmt>-host` (already scanned) and drop the two paths in the step that moves them |
| `check-layout.sh` (probe) | `tutti-plugin-probe` is placed at layer 6 and depends on no `-ipc`, `-host` or `tutti-plugin`; nothing depends on it |
| `check-adapter.sh`, `check-bevy-free.sh` | none: `bevy-tutti` reaches the plugin crates through `tutti-session` and `tutti-plugin` as now |
| `check-canonical-paths.sh` | `tutti_plugin::{PluginClient, …}` paths in code and `*.md` move to the new crates' roots |
| new `check-hosting.sh` | §3.3 |
| CLAUDE.md | workspace tree, layering table, the plugin-hosts paragraph, the contract paragraph; engineering notes |

### 6.3 Candidates dropped

| Candidate | Verdict |
|---|---|
| In-process host crate | **Dropped.** `InProc<F>` is a newtype over `Loaded` of about 300 lines with no dependency the node does not have. It is a module of `tutti-plugin-host` |
| Protocol crate (doc 016 §8) | **Dropped.** The server needs the protocol and the slab, and the client needs the same; both come from `-ipc`. Splitting protocol from client buys nothing until a second client exists |
| Generic-server crate | **Dropped.** `tutti-plugin-server` already is that crate; it only stops dispatching on formats |
| Keeping the hosting-agnostic core inside `tutti-plugin` | Rejected: then `tutti-plugin` is both the node and the only crate that names formats, and the `cfg(feature)` leak of §1.2 is one `use` away. `-host` pays for the compiler-enforced edge |

Minimum split: three new crates, and `tutti-plugin-probe` pays because it is the only way for a probe to stop depending on a hosting protocol and on the served set: it is a binary that links the four format hosts and nothing else. (The owner widened Q6 through Q9: two crates, then three.) If step 4 shows `-host` cannot be extracted
cleanly (for instance the handles drag in `-ipc` types), fall back to a module of
`tutti-plugin` guarded by a grep in `check-layout.sh`; `-ipc` is not optional,
because the server must not depend on the graph node.

## 7. What stays on one side

| IPC only | In-process only |
|---|---|
| Crash and hang recovery (016 §9: `Recovery`, `PluginHandle::block_counts`, `terminate`, `PluginReloadDue`). `Plugin::hosting()` lets the session skip it for an in-process plugin, which it already cannot recover | The VST2 quirk (016 §2.10): `Vst2Quirks::serialize`, the edit ring and the shadow instance. It sits in `Vst2Loaded` and applies wherever `Vst2Loaded` runs, which after this design is only in the host process (and the scanner probe, which makes no concurrent calls) |
| The machine lock (`host/node/machine_lock.rs`) for server spawn | Main-thread load and drop of VST2 (`Plugin::opens_in_process` doc) |
| `plugin-server` tests (`tutti-plugin-contract`'s `server` suite, hostile-peer tests) | Zero added latency, no chunk (`Batcher`) |
| shm slab, framing, state chunks | Editor is the audio instance |
| Server-start and server-death errors (`BridgeError`) | A plugin crash is a host crash |

## 8. The plugin contract

`contract!(Fmt, gaps = N)` runs 20 cases against `Format::load() -> Fmt::L`
through `Loaded`. Because both hostings are `Loaded`, the same cases run on them
unchanged if the format gets a hosting:

```rust
contract!(Clap, hostings = [InProcess, OutOfProcess], gaps = { InProcess: 0, OutOfProcess: N });
contract!(Vst2, hostings = [InProcess],       gaps = { InProcess: 0 });
```

`hostings` is a list of zero-sized `Hosting` types in the contract crate:

```rust
trait Hosting { type Host<F: Format>: Loaded; fn host<F: Format>() -> Self::Host<F>; }
struct InProcH; impl Hosting for InProcH { type Host<F: Format> = InProc<F::L> where F::L: InProcess; … }
struct IpcH;    impl Hosting for IpcH    { type Host<F: Format> = Ipc<F::L>    where F::L: OutOfProcess;    … }
```

The macro emits `mod inproc { … }` and `mod ipc { … }`, one test per case each.
Listing `OutOfProcess` for `Vst2` fails to compile on the `where` clause. The mode word
and transcript already travel in state, so the cases need no new channel. What
changes:

- **Gaps become per hosting** and the ratchet is per hosting. Expected `OutOfProcess`
  gaps are the frame-exact cases: the output is one chunk late, so
  `latency_reported` reports plugin plus chunk, and `automation_point_offset`
  and `note_at_offset` need a stated offset in `spec`. Each is a `Gap { why,
  fails_with }`, reviewed, as 016's contract rule has it. Never `#[ignore]`.
- `process_does_not_allocate` runs under `assert_no_alloc` for `IpcProcessor`
  too (today's gate on the client's audio entry points becomes this case).
- `tests/server.rs` (64 lines, three tests) is subsumed and deleted;
  `tests/vst2.rs` loses nothing.
- The contract builds `plugin-server` first for its `OutOfProcess` modules, like
  `tutti-plugin`'s other server tests (feature `server`).
- The compile-fail tests of §3.4 stay in `tutti-plugin`, since they need no
  runtime.

## 9. Behaviour changes

| Change | Who sees it | Mitigation |
|---|---|---|
| **VST2 over IPC is removed.** The server's VST2 hosting arm, and the editorless `Plugin::open` path for a build without `vst2`, go. A VST2 crash takes the host down | Anyone who built without `tutti-plugin/vst2` and hosted VST2 audio over IPC (no editor, so unlikely); no one on the `Plugin::open` default | Scanning stays isolated: `plugin-probe` still loads VST2 (§2.5). `Hosting::Isolated` on a VST2 path returns `HostingRefused` and names the reason |
| CLAP, VST3, AU become hostable in-process | New: lower latency (no chunk), no isolation, editor is the audio instance (no second instance, no mirror loss), no crash recovery | Opt-in per plugin via `Hosting::Local`; `Auto` is unchanged |
| Windows IPC editors | Unchanged until §5 revisits the `cfg(unix)` | In-process hosting gives Windows CLAP/VST3 an editor now |
| Fork of an in-process CLAP/VST3 | Loads the module again in-process, as VST2 does | Same state-transfer rules as `fork.rs` documents |
| `Plugin` API | `Backend` is gone; `hosting()` added; `opens_in_process` replaced by `hosting_of(path, Hosting)` | Recorded in the PR that lands it (016 §7: no compatibility commitment) |

**Default hosting.**

| Format | `Auto` | Why |
|---|---|---|
| CLAP, VST3 | `OutOfProcess` | Crash and hang isolation is the reason the IPC exists; today's behaviour |
| AU | `OutOfProcess` | Today's behaviour; revisit if the AUv3 path prefers the OS's own out-of-process instantiation |
| VST2 | `InProcess` | The only hosting it has |

## 10. Staged plan

Each step is one reviewable PR and leaves `cargo xtask ci` green (with `cargo build -p
tutti-plugin-server` first). It runs after the AU FFI lane lands, wherever a
step touches AU files: PRs #350 and #353 extract `tutti-au-ffi` (layer 4), make
`tutti-au-host` `forbid(unsafe_code)` and move it from layer 4 to 5, and edit
`tutti-plugin`'s features and `gui/au.rs`. It also runs after 016 step 3 gives AU
a `Loaded`.

| # | Step | Needs AU? | Deletes / adds |
|---|---|---|---|
| 1 | `InProcess`, `OutOfProcess`, `Editor`, `Load`, `Reopen` in `tutti-plugin-types`; `impl InProcess` for `ClapLoaded`, `Vst3Loaded`, `Vst2Loaded`; `impl OutOfProcess` for CLAP and VST3 with `load_editor` wrapping today's `format/gui`; the compile-fail doctests with mutation record; `check-hosting.sh` | No | no behaviour change |
| 2 | Move `format/gui/{clap,vst3}` into the host crates as `OutOfProcess::Editor`; the state-back direction on close (Q8) | No (AU's `gui/au.rs` waits for step 7) | -250 lines in `tutti-plugin` |
| 3 | Extract `tutti-plugin-ipc`: protocol, transport, client, subprocess, batcher, `BridgeError`. Mechanical: no logic change; `check-layout.sh` rows, `check-plugin-ffi.sh` paths, CLAUDE.md | After #350 and #353 merge (they edit `tutti-plugin`'s features and `gui/au.rs`, and `check-layout.sh`'s layer rows move `tutti-au-host`); no AU dependency otherwise | moves about 13k lines |
| 4a | Extract `tutti-plugin-host`; `PluginNode<H>` and `InProc<F>`; `Vst2` in-process runs through them; **delete** `vst2_in_process` (2,274) and its `Arc<Mutex>` / `try_lock` | No | -2.3k, +~0.9k |
| 4b | `Ipc<F>: Loaded` over the existing bridge; `PluginNode<Ipc<F>>` replaces `PluginClient`'s node; **delete** `graph_node.rs`, the IPC `fork.rs` and `composite.rs` duplicates and the `Host*` capability traits where `Control` replaces them | No (CLAP and VST3 rows) | -1.5k |
| 4c (built) | **The probe binary.** Build `tutti-plugin-probe` (CLAP, VST3, VST2; AU's probe row joins when AU's `Loaded` lands), the file wire, `ProbeLocator` and `tutti-fixtures::plugin_probe`; switch `probe_plugin` (`db_probe.rs`) to it; then delete probe mode (`TUTTI_PROBE_MODE`, `ProbePlugin`, `Plugin::probe`) from the server. `check-layout.sh` row, CLAUDE.md "build first" note. Independent of 4a and 4b | No (AU row later) | -300 in server, +~300 new |
| 5 | Generic server: `Served<F: OutOfProcess>` and `served!`; delete `loaders/clap.rs`, `vst3.rs`, `vst2.rs` (the whole VST2 loader), `plugin.rs`, `format_host.rs`'s capability traits. Supersedes 016 step 4's `AnyPlugin` | AU row waits; the macro takes it later | -5.5k |
| 6 | The facade: `hosts!`, `AnyHost`, `Hosting`, `open`; replace `Backend`; CLAP and VST3 hostable in-process; contract grows `hostings` and per-hosting gaps; delete `tests/server.rs` | No | `Backend` gone |
| 7 | **AU:** `impl InProcess` (confirm `LoadInProcess`, §3.2) and `impl OutOfProcess` for `AuLoaded`, `OutOfProcess::Editor` from `gui/au.rs`, the AU row in both tables, `loaders/au.rs` (2,121) deleted | **Yes** (AU `Loaded`, 016 step 3) | -2.1k |
| 8 | Cleanup: the unused `OpenEditor` message and handler, stale Cargo comments and features, 016 and CLAUDE.md and engineering-notes updates | After 7 | docs |

Steps 1, 2, 4a, 4b, 4c, 5 and 6 (without the AU rows) can start before AU is ready;
3 should wait only for the AU lane's merge to avoid conflicts; 7 is the only
step blocked on AU's `Loaded`.

## 11. Open questions

| # | Question | Recommendation |
|---|---|---|
| Q1 | Marker and host names | **Decided.** Both layers: markers `InProcess`/`OutOfProcess`, hosts `InProc<F>`/`Ipc<F>`; the rejected alternatives are in §3.1 |
| Q2 | Remove VST2-over-IPC, or keep it behind an explicit opt-in? | **Decided: remove VST2 hosting from the server.** The owner recalled that it "doesn't work, that's why it's like that; if it doesn't work, remove it", and `26d748df7` (§1.5) confirms: `AEffect` fuses editor and audio, and the subprocess path was never a real implementation. An opt-in would need the server-window machinery (§4.1), unused and impossible on macOS and Wayland |
| Q3 | Default hosting per format | **Decided.** CLAP, VST3, AU `OutOfProcess`; VST2 `InProcess` only; per-plugin override by `Hosting` (§9); `Auto` never moves a blacklisted plugin in-process (see Q11) |
| Q4 | Does the server-window option count as a remote editor? | No: the editor is the audio instance, so it is not "a separate instance"; it is a different capability (§4.1). Recommended, owner did not object |
| Q5 | `unsafe trait` like `Send`/`Sync`? | Safe, unsealed, guarded by `on_unimplemented`, the compile-fail doctests and `check-hosting.sh` (§3.3). Recommended, owner did not object |
| Q6 | How many crates? | **Decided; widened by the owner through Q9.** Three new, `tutti-plugin-ipc`, `tutti-plugin-host` and `tutti-plugin-probe`; no in-process, protocol or server crate (§6.3) |
| Q7 | Does `InProc` keep the `Mutex` and silent `try_lock`? | No. The `Processor`/`Controller` split and 016 §2.2's per-format argument replace it; where VST2 needs serialisation it is the quirk (§2.10). Recommended, owner did not object |
| Q8 | Must `OutOfProcess` promise that GUI-only state reaches the audio instance? | **Decided: yes.** `OutOfProcess` promises that the full GUI state reaches the audio instance. The state sync (pull the blob from the editor on close or on a dirty signal) is built in step 2, before CLAP, VST3 and AU claim the marker |
| Q9 | Where does probing live? | **Decided:** a separate probe binary, `tutti-plugin-probe`, independent of both hostings (§2.5). The server drops probe mode and VST2. The wire is a result file (recommended over stdout and a socket, §2.5) |
| Q10 | Should AU's `OutOfProcess` be the OS's own out-of-process instantiation instead of `plugin-server`? | No for now: one protocol, one recovery path; revisit once AUv3 `LoadInProcess` is confirmed. Recommended, owner did not object |
| Q11 | Should `Hosting::Auto` flip to `InProcess` for a plugin the session has blacklisted for crashing the server? | No. A crashing plugin is never moved into the host's process; `Auto` stays `OutOfProcess` and the blacklist refuses (also recorded under Q3). Recommended, owner did not object |
