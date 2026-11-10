# 027: the plugin crates, one coherent cleanup

The plugin crates hold the same logic several times over. There are four note
translations, three server loaders, two node implementations, two trait
families for one set of operations, five panic guards and a dozen copies of
each test helper. They also hold a few real defects that the copies hide. This
doc lists what the 2026-11-10 audit found and queues every item into the plan
that already exists: doc 021's steps, doc 016's §7, doc 023's steps and doc
017's MIDI stages. Where no existing step fits, it adds a numbered PR to the
same order. It is a work list, not a new design: an item that changes a design
says which doc's section it amends.

Status: plan only. The owner asked (2026-11-10) for every item to be
implemented, and for the work to go through the existing plan rather than run
beside it. Decisions taken here follow the delegation rule (correctness first,
then cleanliness, and record the choice). Each one is marked **Decided** with
its reason, so a reviewer can overturn it in this PR.

## 1. Sources

There were seven read-only surveys over main at 619b12f01 plus the
`tutti-plugin-ipc` branch (#406). The key claims were checked by hand:

| Survey | Scope |
|---|---|
| IPC audit (four reviewers) | `tutti-plugin-ipc`: cleanliness, correctness, soundness, performance |
| Format host duplication | `tutti-{clap,vst3,vst2,au}-host` |
| Server vs host overlap | `tutti-plugin-server` against the host crates and `tutti-plugin-types` |
| `tutti-plugin` internals | the node, the handles, the facade, the catalog |
| `tutti-plugin-types` | the vocabulary crate |
| FFI | the four `-ffi` crates |
| Tests | every plugin crate's tests, the contract, the fixtures |

The MIDI items were agreed with the MIDI lane (2026-11-10). Items marked
"MIDI" are applied by this lane, using the MIDI lane's API, on its schedule.

## 2. Defects first

These are wrong behaviour, not duplication. Each one is in an early PR (§10),
whatever else its code is waiting for.

| # | Defect | Where | PR |
|---|---|---|---|
| D1 | **Aliasing UB in the slab mapping.** `as_mut_slice(&self)` returns a `&mut` over the whole mapping, and `header()` writes through a shared `&[u8]`. | `tutti-plugin-ipc` `transport/shm/mmap.rs` | P1 |
| D2 | **`AudioSlab` is `Sync` with safe `&self` writers.** Two threads can write one slot. | `transport/shm/slab.rs` | P1 |
| D3 | **Predictable shared-memory and socket names, opened with `create` and `truncate` rather than `create_new` and `O_NOFOLLOW`.** Another process can pre-create or link the name, and a truncate under a live mapping raises SIGBUS. | `config`, `transport/shm`, `transport/control` | P1 |
| D4 | **Slot reuse with no recheck.** With `RING_SLOTS = 2`, a slow reader can read a slot that is already being rewritten. | `transport/shm/slab.rs` | P1 |
| D5 | **Control commands never settle owed replies.** After an abandoned block, the next query (`GetParameterList`, `SaveState`, `LoadPreset`, ...) reads the stale `AudioProcessed` and answers wrong once. | `client` dispatch: only the `Process` arm calls `settle` | P2 |
| D6 | **A `PARAM_TIMEOUT` crashes a healthy bridge** (a `?`), and a full queue is reported as `PluginCrashed`. | `client` | P2 |
| D7 | **`recv_reply` restarts its deadline on each unsolicited message**, so a chatty plugin never times out. | `client` dispatch | P2 |
| D8 | **The `Reassembler` accepts unbounded empty chunks**, and socket sends have no write deadline. | `protocol`, `transport/control` | P2 |
| D9 | **`rate_refusal()` locks a `Mutex` and clones a `String` on the audio thread.** | `tutti-plugin` `batcher.rs` | P2 |
| D10 | **`POOL_CAPACITY` is one short.** It must be `COMMAND_QUEUE_SIZE + 1`. | `client::audio` | P2 |
| D11 | **`LaunchedServer` holds a bare `Child`**, so a dropped launch leaks the process. | `subprocess/launch.rs` | P2 |
| D12 | **`num_samples` from the wire is unbounded.** The server resizes its scratch to `channels × num_samples` on the audio thread before the slab refuses the block. The formats also disagree on an over-long block: CLAP refuses it, VST2 splits it, and VST3 passes it to the plugin above `maxSamplesPerBlock`. | `tutti-plugin-server` `session.rs`, `audio_pipeline.rs` | P2 |
| D13 | **`SetSampleRate` writes the clock before the plugin accepts the rate.** A NaN, zero or refused rate stays in `Clock` and reaches every later block. | `session.rs` `SetSampleRate` | P2 |
| D14 | **The scratch reallocates whenever the block length changes**, on the audio thread, against the crate's "no allocation after warm-up". | `audio_pipeline.rs` `reshape` | P2 |
| D15 | **The VST2 callback wrapper drops the panic payload**, unlike the other four guards. | `tutti-vst2-ffi` | P5 |
| D16 | **AU drops the bank of a banked Program Change** (VST2 sends bank MSB, bank LSB, then the program), and passes the event offset unclamped. | `tutti-au-host` `instance.rs` MIDI path | P3 (MIDI item 6) |
| D17 | **`HostRenderMode` is not exported**, so a backend outside the crate cannot implement it. | `tutti-plugin` `lib.rs` `backend` | P5 |
| D18 | **`publish_with_extension(real, ext)` replaces `real` with a link to itself when `real` already has `ext`.** This happened to the Linux `.so` cdylib in #406. | `tutti-fixtures` | P4 |

D1 to D4 are soundness defects. D5 to D14 are correctness defects on the
process edge. D15 to D18 are small.

**Decided:** D12 is fixed once, at the server, rather than in each format. The
server records the activated `max_block` at load. A block that is empty or
longer than `max_block` is silence and is counted (the refusal path that
already exists), and the scratch is sized to `max_block` once, which also
fixes D14. VST3's processor gets the same check as defence in depth, since it
is the host's own contract with the plugin.

## 3. The process edge (`tutti-plugin-ipc`, doc 021 step 3)

Step 3 is #406. The probe spawner left the crate in that PR. It is now
`tutti-plugin`'s `host/probe.rs`, and `ServerLocator` and `ProbeLocator`
became one `tutti_plugin_types::Locator` with a `Helper`. What remains:

- **P1, soundness:** D1 to D4.
  - The mapping becomes a raw `NonNull<u8>` base with `copy_nonoverlapping`
    in and out, and no reference ever spans the mapping.
  - `AudioSlab`'s writers take `&mut self`.
  - Names come from a random source and are opened `create_new` (the socket
    lives in a private `0700` directory).
  - A slot's sequence is rechecked after the copy.
  - The missing `// SAFETY:` comments and the unchecked layout arithmetic
    (checked multiplication in `SlabLayout`) go in the same PR.
  - Loom cannot model a mapping, so the seqlock recheck is pinned by a test
    that interleaves two writers by hand, with the mutation recorded.
- **P2, the control path and the wire's validation:** D5 to D14. It also:
  - deduplicates the three kill-and-reap copies (the probe's `Reaped`, the
    launch teardown, `tutti-plugin`'s `ProcessGuard`) into one guard in
    `-ipc`, which `host/probe.rs` uses as well;
  - merges the two name counters into one;
  - compares the editor's state pull-back against a baseline taken at open
    rather than against "anything".
- **P2b, the frame path's allocations:**
  - `read_frame` allocates a body and deserializes into fresh structures on
    every frame;
  - `write_frame` serializes into a fresh `Vec`, then makes two `write_all`
    calls and a flush.

  The fix is one reusable body buffer per direction, `serialize_into`, and one
  write. Also: `AudioResponse` is about 5 KB and is copied per block, and
  `serde_bytes` belongs on the byte fields. Pinned by a no-alloc gate around
  a client and server round trip.
- **P5, small items:**
  - `serde(default)` does nothing under bincode, so drop it. Every wire
    change already bumps `PROTOCOL_VERSION`.
  - `shm_prefix` is dead.
  - The `Command::Shutdown` arm is unreachable.
  - The ~350-line `handle` function is split by message family.

## 4. MIDI (with doc 017)

| # | Item | Uses | When |
|---|---|---|---|
| M1 | `Midi1Decoder` and `Midi1Splitter` go to `tutti_ump::Midi1Parser` (one behaviour change: a status byte that cuts a SysEx short emits no End) | `tutti-ump` | P3, now |
| M2 | `convert`'s integer scaling goes to `tutti_midi_protocol::widen` / `narrow_*`. The float helpers (`unit_f32_to_u32`, `u16_to_unit_f32`, `signed_f32_to_bend_u32`) stay until 017 stage 3b | `tutti-midi-protocol` | P3, now |
| M3 | The `fn widened` copies in the CLAP and VST3 event code become direct `widen` calls | `tutti-midi-protocol` | P3, now |
| M4 | VST2's `to_midi1_bytes` goes to `midi1()`, `Voice1::bytes` and `System::bytes`, with `tutti_ump::frame`; `sysex7_fragments` stays | `tutti-ump`, `tutti-midi-protocol` | P3, now |
| M5 | AU's hand-built MIDI goes to `narrow`. This fixes D16. Tell the MIDI lane when it lands | `tutti-midi-protocol` | P3, now |
| M6 | `SysExAssembler`'s input becomes `decode` to `Sysex7Packet`. Which reassembler survives, the checked `SysexMessage` and the frame swap are 017 stage 3c (the MIDI lane's) | `tutti-ump` | P3, now; the rest is stage 3c |
| M7 | `midi_intake` uses `Ump::group()`, `message_type()`, `channel()` and `address()` | the MIDI lane's field-constructor PR | when that merges |
| M8 | The two note-id encodings become one `NoteId`, and per-note expression becomes `Ump` with a table per format | 017 stage 3c (decided 2026-11-10) | stage 3c, with §5.1 |

Nothing changes the hosts' input edge.

## 5. The format hosts

### 5.1 One note translation (new: P7, before 023 steps 4 and 7)

Each format translates `Voice2` into its note events by hand: CLAP
`events.rs`, VST3 `events/midi.rs`, VST2 through `tutti-midi-types`, and AU
with hand-assembled status bytes. CLAP and VST3 also translate back by hand.
The CLAP and VST3 copies share byte-identical helpers:

- the per-note controller expression map (7 is Volume, 10 is Pan, 74 is
  Brightness);
- the 20-arm `RegisteredPerNoteController` match;
- the per-note bend range tracking.

The bank-select expansion of a banked Program Change (CC0, CC32, then the
program, all or none) is written three times. About 350 lines of production
code and about 500 lines of tests cover the same spec.

**Decided:** one pure module in `tutti-plugin-types`, with these pieces:

- It turns a `Message` into a format-neutral note value: on, off, poly
  pressure, expression with its unit value, tuning in semitones, or MIDI 1
  bytes.
- The inverse turns a format-neutral value back into a `Ump`.
- One MIDI 1 path serves every format, so a banked Program Change has one
  definition.
- Each host keeps only its small table from the neutral value to its struct,
  and its own scaling: CLAP volume is a gain in `0..=4`, VST3 volume is
  unit-range, and VST3 tuning is `0.5 + st/240`.

Four limits, agreed with the MIDI lane (2026-11-10), keep the module from
starting a second copy of anything that already has an owner:

1. The MIDI 1 path is `tutti_midi_protocol::midi1(Message)`, with
   `Voice1::bytes`, `System::bytes` and `tutti_ump::frame` under it. `midi1`
   already writes a banked Program Change and every Note On or Off attribute.
   A format that needs more asks the MIDI lane to add it there.
2. The neutral value stays on the host side. It is never an event-port, wire
   or stored type: the graph and IPC carry `Ump`.
3. Value scaling comes from `tutti-midi-protocol` (`widen`, `narrow_*`, and
   the float converters, which the MIDI lane moves there from
   `tutti_midi_types::convert` before 017 stage 5b). Only a format's own
   scaling stays in its host.
4. The module mints no note id. It takes the one `NoteId` (M8), or the
   format's `i32` derived from it. **Decided:** the module owns
   `PerNoteBendRanges`, its only production consumer, and 017 stage 5b's move
   targets it.

This is 017 stage 3c's "one table per format" decision, built once rather
than four times. It lands before 023 steps 4 and 7, which would otherwise add
more per-format note-expression code. Oracle: M2-104 for the messages, plus a
mapping test in each direction per format; the existing spec-derived tests
move to the shared module. The frame offset clamp is decided once here too.
Today CLAP uses `max(0)` plus `clamp_times`, VST3 uses
`try_from(..).unwrap_or(MAX)`, VST2 uses `delta_in_piece`, and AU has no clamp
(D16). All of them clamp to the block through one function.

### 5.2 Controller plumbing (P6, then 016 step 10)

- **One `EditQueue`** beside `EditSink` in `requests.rs`. It holds the sink,
  `unsent` and `reset_pending`, with `send`, `reset`, `retry` and
  `take_unsent`. CLAP and VST3 each wrote this with the same field names, and
  `take_edits` treats `Edit::Reset` the same in both processors.
- **`RequestWord::forget(&[Request])`** replaces three copies of "answer the
  announcements raised during activate".
- **`Refused::keeping(plugin, reason)`** and a `From` for `Stuck` replace the
  same construction in three crates.
- **`rejoin`'s shared checks** (a pair, the share count, stop, drain, append
  unsent) become one function. Re-applying pending edits stays per format:
  VST3 keeps the latest edit per parameter, CLAP keeps every edit in order.
- **The forwarding `impl Control`** (about 85, 115 and 65 lines) is removed by
  016 step 10's `Plugin<S>`, which already replaces the forwarding layer. It
  is not a macro now, so the code is not written twice in a row.

### 5.3 Shared vocabulary (P6)

- `PluginError::{load, editor}` constructors and `From<XError> for
  PluginError`. These replace three `plugin_error` and three `editor_error`
  functions.
- **Three structs named `PluginInfo`**, plus `PluginDescriptor`. The host
  fills `PluginDescriptor` at load and keeps its extras under a
  format-specific name, which removes three `descriptor_of` functions. 023
  step 5's `Capabilities` assembly then reads one type.
- `TransportInfo::usable_tempo()` replaces three copies of the tempo rule.
  `TransportInfo` carries each position twice (`beats` and `quarters`), and
  one of the pair is a derived accessor.
- `ParamRange::plain(min, max)`, and one scaling used on CLAP's audio path,
  which today has its own f32 formula with no NaN guard.
- "The transport changed" is defined three ways: VST2 diffs flags, AU takes a
  `changed: bool`, and the server's AU loader uses `PlayheadWatch`. It becomes
  one `PlayheadWatch` in `tutti-plugin-types`.
- The re-export shims that the import rule forbids: AU `listener.rs` and
  `midi_out.rs`, CLAP `host/streams.rs`, VST3 `helpers.rs`.

### 5.4 Smaller shared pieces

| Item | Home | When |
|---|---|---|
| `InProcessEditor<L>` over the CLAP and VST3 editor wrappers, and one `LoadSpec` | `tutti-plugin-types` | 021 step 7, so the AU editor uses it from the start |
| Speaker tables: one `SpeakerTable` contract plus a test macro (the tables stay per format; AU's tag scheme genuinely differs) | `tutti-plugin-types` | P6 |
| One panic `guard(label, fallback, f)` replacing the four `-ffi` copies and `driver.rs`'s `guarded` (fixes D15); `check-guards.sh` follows | `tutti-plugin-types` (it does not forbid `unsafe`; the guard needs none) | P5 |
| Timer tables (CLAP `TimerState`, VST3 `run_loop`) | none | **Decided: skip.** The handlers differ, and the shared part is two fields |
| `Reopen` has no impls. VST3 keeps a private `origin` that `Reopen` should use. A shared `Origin { path, load }` lets `Reopen` be a blanket impl over `InProcess` | `tutti-plugin-types` | 021 step 4a, which is its first user |

## 6. The FFI crates

- **One `WindowHandle` decode** (P5). `HostEditor::open_editor` takes a
  `WindowHandle`. `util/window.rs`'s `extract_platform_ptr` becomes the one
  `unsafe` decode, with one `// SAFETY:`, and `from_ptr` is deleted. This
  removes the two backends' decodes, the no-op round trips in `gui/au.rs` and
  the server's `loaders/au.rs`, and the `VST2_CALLERS` pin in
  `check-plugin-ffi.sh`. This finishes 016 step 5's "`WindowHandle` in
  `HostEditor`".
- **The dead plugin-authoring half of `tutti-vst2-ffi`** (about 1.7 to 2k
  lines) moves to `testing/tutti-vst2-author`, the only crate that uses it
  (new P9, independent of the others). Its modules become private behind the
  per-module path gate, like the other `-ffi` crates.
- `BufferPtrs` is dead (P5).
- Narrow the VST3 SDK helpers and CLAP's `chars_to_string` to their callers
  (P5).
- The string helpers that are written per crate are low priority. **Decided:**
  they are left until a PR touches them.

## 7. `tutti-plugin-types`

- **`format_host.rs`** (461 lines, the old `PluginMeta` / `PluginAudio` /
  `PluginParams` / `PluginState` / `PluginPresets` / `PluginEditorHost`
  family) is used only by the server.
  - **Amends 021 step 5:** step 5 deletes the CLAP, VST3 and VST2 rows, but
    the family survives for AU's loader until step 7. Step 5 moves what AU
    still needs into the server's `loaders/au.rs` as private traits, so the
    public family goes in step 5 and the AU rest goes in step 7.
  - `parameter_count` has no counterpart in `Control`. The server calls it
    once at load to size the queues, so it becomes `parameters().len()`
    there.
- `Editor::poll_edits` returns bare `f32`; it becomes `Normalized` (P6).
- **About 100 bare `f64` sample rates** in the plugin crates become
  `SampleRate` (task 0b-5).
  - Comment the wire hop where the type stops.
  - The server's are in 021 step 5, where they are rewritten anyway.
  - The rest are P6.
- Dead exports: `push_sysex_stack`, `MAX_HOST_NOTE_ID` (017 stage 3c's note
  id), `clap_features_role` and `InvalidContentScale` (P5).
- `features` becomes private and the crate joins the per-module path gate
  (P5).
- `AspectRatio` and `EditorSize` take the range-checked shape (P6), with
  `check_range_contract`.
- `Layout` and `LoadedPlugin` describe the same wiring twice. **Decided:**
  `Layout` stays as the activation input, `LoadedPlugin` derives from it, and
  023 step 5 carries it.
- `midi1_decoder.rs`, `sysex.rs`, `note_id.rs` and `WireMidi` are §4 and 017
  stage 3c.

## 8. `tutti-plugin`

- **Two nodes, one `PluginNode<H>`** (021 steps 4a and 4b). What the two
  `process` bodies share:
  - the event intake;
  - `param_input::take`;
  - counting dropped MIDI;
  - the transport;
  - the hand-off and loss accounting;
  - writing MIDI out;
  - `reset`.

  Their forks share the reopen sequence. These stay per hosting:
  - chunk pipelining and its declared latency (IPC);
  - rate delivery (VST2 parks the rate for the main thread);
  - harmony (IPC only);
  - fork thread rules (VST2 pinned to the main thread, IPC deferred), which
    `Reopen` carries in its signature;
  - VST2's program-before-state restore;
  - `ForkWatch`.
- **Before 4a (new P8):**
  - One `TransportSource` (`at(cx, offset, features)`, `advance`) replaces
    `Bound`'s fields and VST2's `TransportFeed` (about 100 lines).
  - One `NodeControls` core holds the meter and the delivery counters. It is
    written five times (`Plugin`, `PluginClient`, `PluginControls`,
    `InProcessVst2Controls`, `InProcessVst2Client`).
  - The three listener sinks become one `Sink<A>`, with type aliases keeping
    the public names.

  These shrink 4a's diff and touch no hosting decision.
- **The `Host*` traits against `Control`: the gap 021 §10 leaves open**
  (**amends 021 step 4b**, decided here):
  - Parameters, state, presets and automation go to `Control`.
    `set_automation_mode`'s `Result` (delivered or crashed) is carried by the
    health half below.
  - `set_parameter_value` is callable from any thread today
    (`set_parameter_rt`), but `Control::set_parameter` takes the
    `MainThread` token. **Decided:** the handle queues the edit through the
    `EditQueue` (§5.2), and the controller drains it on its main-thread tick.
    Callers off the main thread keep a wait-free write, and `Control` keeps
    its token. A second write trait would be two ways to set one parameter.
  - **`Health`** is a trait of its own: crash, crash cause, rate refusal,
    block counts, terminate, dropped events. It is IPC only, and the handle
    holds an `Option<&dyn Health>`. In-process stops returning constant
    `false` and `0`.
  - **`EditorSurface`** (on the host side) holds the resize and capability
    calls that `Editor` lacks.
  - A render-mode change in mid-life has no `Control` route. **Decided:** it
    is `HostOp::Reconfigure` with `Prepare::with_mode`, driven through the
    lifecycle, as the server already does by hand. `HostRenderMode` (D17)
    goes in 4b, and P5 exports it until then.
- **The two capability backends** (`SubprocessBackend`,
  `InProcessVst2Backend`) become `IpcController<F>` and `InProc`'s
  controller in 4b and 4a. The ordering of VST2's `editor_idle` pump goes
  with its `Mutex`.
- **Host policy files with no user in the crate:**
  - **Decided:** `render_mode.rs` (`needs_announcement`) and `liveness.rs`
    (the debounce) move to `tutti-session` (P5). They are the session's
    policy over a hosted plugin, which is when to announce a render-mode
    change and when a silent plugin counts as hung. Neither is the host's
    mechanism: no node or format calls them. This **amends 021 §6.1**, which
    placed them in `tutti-plugin-host`. `Liveness::observe` keeps taking
    `now` as an argument. The session's phases receive the clock, and no
    `Instant::now()` goes inside.
  - `param_values.rs` stays until the `Control` change.
- **Three liveness enums** (`Verdict`, `PluginLiveness`, `PluginStatus`) and
  the bridge's two `AtomicBool`s become one state (task 0b-6). They land with
  4b, which owns the bridge's lifecycle.
- **The OS clock's `unsafe`** (`clock_gettime`, `QueryPerformance*`) moves to
  `tutti-rt` so that `tutti-plugin-host` can forbid `unsafe` (021 step 4a,
  as planned). `Instant` is not equivalent, because a plugin compares
  against the OS epoch.
- **The facade** (`Plugin`, `Backend`, `into_client`) reaches its final shape
  in 021 step 6. Two routes to `set_render_mode`, and `takes_harmony`
  hard-coded `false`, are removed there.

## 9. The server (021 step 5)

- **`Instance<L: Loaded>`** replaces the three loaders' copied glue (about
  1,500 of their 1,830 non-test lines). The copied pieces are:
  - `drive` and `absorb`;
  - `finish_refusals`, `refresh_meta` and `serve`;
  - `load_with_state`;
  - the `process` match;
  - `set_sample_rate`.

  These stay per format:
  - CLAP's `settle_edits` and its timer pass;
  - VST3's `discard_notifications` and its rollback;
  - VST2's live render-mode flag.

  **Amends 021 step 5:** step 5 calls the formats "rows of a table" without
  naming their hooks. The hooks are `take_requests`, `flush_edits` and "mode
  as restart or live". They go on `Loaded` or `Control`, which already have
  `flush_edits` and `run_main_thread`. The "restore state before activate"
  ordering is `Served::load`'s, because `OutOfProcess::load_served` has no
  state argument.
- **The f32 and f64 arms** of `process` and `with_audio_buffer_*` become one
  function generic over the sample type.
- **The loader tests are about 4k lines against real plugins.** They move to
  the contract or the host crates before their loader is deleted, never
  dropped. Step 5's line estimate assumed they would go.
- **`libloading` becomes a dev-dependency.** Every use is in test code (P5).
- **The dead editor-in-server path** (`OpenEditor`, `CloseEditor`,
  `EditorOpened`, `EditorState`, `close_editor_on_drop`, `editor.rs`, the
  `PluginEditorHost` impls, and `from_u64` with its `unsafe`) is about 250
  lines. Nothing sends these messages. **Amends 021 step 8:** it moves to P5,
  with a `PROTOCOL_VERSION` bump.
- `MAX_CHANNELS = 16` is defined twice (server and VST2 in-process). Both use
  `tutti-plugin-types`' one constant (P5).
- The plugin-server's `--features au`-only clippy failure (the dead
  `standing`; plan item 4) goes with P5.

## 10. Tests

All of these are test-only, low risk, and done before 4a so they shrink its
diff (new P4):

- **Into `tutti-fixtures`:**
  - `main_thread()`, written 31 times in the host crates and 9 times in
    `tutti-plugin`;
  - one `Active<L: Loaded>` (CLAP, VST3 and VST2 each wrote 250 to 420 lines
    of it). Its `reconfigure` behaves one way: re-activate the previous
    configuration on refusal, as VST2 and VST3 do;
  - one environment guard (five copies);
  - one cross-process lock (`machine_lock.rs` and `clap_probe.rs`'s
    `cross_process_lock` are two implementations of one lock);
  - `Latent`;
  - the CLAP timing constants;
  - `drive_block`;
  - `clap_probe()` and `vst2_probe()`, replacing three `probe_path.rs`
    copies.
- **The test plugins' capture readers** (`read_capture()`) move into the test
  plugin crates. This replaces about 17 `libloading` copies and the VST2
  `transmute`s.
- The `static PROBE_LOCK` / `AU_LOCK` mutexes serialise nothing under
  nextest, where each test is a process. They go where the shared plugin
  state does not need them. Where it does, the cross-process lock replaces
  them.
- **D18:** `publish_with_extension` refuses a link path equal to the real
  path.
- `launch.rs`'s hand-rolled `TempDir` becomes `tempfile`, a dev-dependency of
  `-ipc` since #406.
- **The contract** (016 §6.1):
  - G2 and C4 need nothing new and land in P4.
  - G1 and G3 need load-time identity and land with 023 step 5.
- **Per-crate tests that repeat a contract case** are deleted once the case
  runs for that format. Each deletion names its case.
- `tutti-clap-test-plugin` narrows to what the shared plugin cannot express,
  and `tutti-vst2-test-plugin` retires (plan item 19), after P4 moves their
  readers.
- **The VST3 `#[ignore]`s** (18 VSTGUI plus the display ones, all in
  `tutti-vst3-host`) move to the shared test plugin and the contract, then
  each `#[ignore]` goes after a mutation check (task 5, plan item 6).

## 11. Order

One list. Existing steps keep their names. The new PRs are P1 to P9. A PR
that runs in parallel with the one above it says so.

1. **021 step 3** (#406), in CI.
2. **P1** IPC soundness (D1 to D4).
3. **P2** control path and wire validation (D5 to D14), then **P2b** frame
   allocations.
4. **P3** MIDI swaps M1 to M6 (D16). Parallel with P1 and P2: it touches the
   host crates, not `-ipc`. M7 follows when the MIDI lane's field
   constructors merge.
5. **P4** tests and fixtures (D18), parallel with P3.
6. **P5** small items: D15, D17, the dead server editor path (from step 8),
   one `WindowHandle` decode, one guard, the dead exports, the private
   `features`, `BufferPtrs`, `libloading`, `MAX_CHANNELS`, the server's `au`
   clippy, `render_mode` and `liveness` to `tutti-session`, the `-ipc` small
   items.
7. **P6** shared vocabulary: `EditQueue`, `forget`, `Refused::keeping`,
   `rejoin`, `PluginError` constructors, one `PluginDescriptor`,
   `usable_tempo`, `ParamRange::plain`, `PlayheadWatch`, the shims,
   `SpeakerTable`, `Normalized` edits, the checked `EditorSize` and
   `AspectRatio`, and the non-server sample rates.
8. **P7** one note translation (§5.1), with 017 stage 3c's per-note
   decision.
9. **016 step 10** `Plugin<S>`, which removes the forwarding `Control` impls.
10. **016 step 3 / 021 step 7 groundwork:** AU's `Loaded`, built on P6 and
    P7.
11. **P8** `TransportSource`, `NodeControls`, `Sink<A>`.
12. **021 step 4a** (`Origin` and `Reopen`, the clock to `tutti-rt`), then
    **4b** with §8's `Host*` decisions and the one liveness state.
13. **021 step 5** with §9's hooks, the loader tests moved, and the server's
    sample rates.
14. **021 steps 6, 7 and 8.** Step 8 shrinks to docs, since its code went in
    P5. 023's steps follow its own order. Its step 5 carries the contract's
    G1 and G3.
15. **P9** `tutti-vst2-ffi`'s authoring half to `tutti-vst2-author`, at any
    point: it is independent.

The shared ledger (`clean-plan.md`) points to this list. Each PR adds its
line there when it merges.
