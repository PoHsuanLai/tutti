# Divergence ledger

Every departure from [the conventions](conventions.md) found by the Wave 2
coherence audit, sized and sequenced. Seven parallel read-only audits across three
model vendors, then verified against the code.

Status values: `open`, `done`, `wontfix` (with the reason recorded inline).

**A divergence both sides justify deliberately is a convention, not a defect.** The
FORCED items in §6 are recorded precisely so they are not "fixed" by a later pass.

---

## 1. Correctness — real-time safety

These are bugs, not style. They outrank everything below.

| # | Finding | Site | Status |
|---|---|---|---|
| RT-1 | **FIXED (ced10e10).** Both channels are bounded `ArrayQueue`s with named capacities and per-cause drop counters. Mutation-tested: injecting an allocation into `automate` aborts the no-alloc gate. Original finding: Two `crossbeam_channel::unbounded()` channels are fed from `audioMasterProcessEvents` and `automate`, which VST2 plugins call from inside `processReplacing` — i.e. the audio thread. `try_send` on an unbounded channel cannot fail, so `let _ =` guards nothing and each send allocates; the queue also grows without bound if the receiver lags. **Verified by direct reading.** Fix: bounded `ArrayQueue`, the idiom already used in `tutti-plugin`'s `ipc_client/audio/channels.rs`. | `tutti-vst2-host/src/instance.rs:157-158`, `src/host.rs:132,145` | done |
| RT-2 | **FIXED (ced10e10).** `PluginAudio::process` now writes into caller-owned storage that retains capacity; a second, unreported bug surfaced while fixing it — clearing `ParameterChanges` freed every queue buffer, so `add_change` reallocated per automated parameter per block; queues are now retired into a pool. Known limitation documented at the site: the real-loader gate cannot catch the specific `to_owned` regression because the vendored `audio-probe` emits no events. Original finding: `audio_pipeline.rs` documents the per-block path as allocation-free, guarded by `process_is_alloc_free` — which drives the `NanPlugin` stub, so no real loader is exercised. The real path allocates every block: `collect()` + `clone()` discard the borrowing, zero-alloc `ProcessOutputRef` that `tutti-vst3-host` returns deliberately (its owning conversion is marked "Allocates; off-RT only"). **Verified by direct reading.** Fix: propagate the borrowing shape through `PluginInstance::process`, and extend the test to a real loader. | `tutti-plugin-server/src/audio_pipeline.rs:207,966`; `src/loaders/vst3.rs:490-491`; also `loaders/vst2.rs:217,235` (SmallVec spill), `vst3.rs:748`/`clap.rs:458` (`to_string` on an error path) | done |
| RT-3 | **FIXED (ced10e10).** Pre-sized `VecDeque` with try_lock-or-drop, evicting oldest to preserve intent ordering; the drain's `mem::take` (which handed back a zero-capacity deque and put the reallocation back on the audio thread) is now `drain(..)`. Original finding: **Verified — partly overstated as reported.** `push_transport_request` uses `if let Ok(mut reqs) = state.transport.requests.lock()`, so it does *not* block on a poisoned mutex as first described; `.lock()` still blocks under contention. Two real defects remain: the queue is an unbounded `Vec` (`state.rs:320`) that grows without limit if the host never drains it, and it is reached from the `CLAP_EXT_TRANSPORT_CONTROL` vtable, which CLAP permits a plugin to call from the audio thread. Fix: bounded storage plus `try_lock`-or-drop, matching `tutti-core/src/metering/tap.rs`. | `tutti-clap-host/src/host/callbacks.rs:633-639`, state at `src/host/state.rs:319-321` | done |
| RT-4 | **FIXED (ced10e10).** `Drop` now shuts down and finalizes, sharing one `shutdown()` with `stop()`; the taken `Option<JoinHandle>` makes the join once-only. Outcome goes to a `FinalizeStatus` handle since `Drop` cannot return a `Result` and this workspace does not panic in `Drop`. Mutation-tested independently by the orchestrator: disabling the `Drop` body fails the test. Original finding: **Verified — real, but narrower than first reported.** There is no `Drop` impl; `finalize()` runs only at the end of the spawned closure (`recorder.rs:152`). The loop has *two* exits, not one: a finite source (`OnEmpty::EndOfStream`) breaks and finalizes on its own, so files-to-file recording is safe. The data-loss window is a **live** source (`OnEmpty::Starved` — every microphone capture), whose arm only sleeps and never breaks: dropping the `Recorder` without `stop()` leaves the thread spinning forever, `finalize()` never runs, and the WAV header is left unpatched, which the crate's own doc says makes the file unreadable. Every sibling thread owner joins in `Drop`. | `tutti-io/src/recorder.rs:138-178` | done |
| RT-5 | **FIXED (ced10e10) — and the finding was worse than reported.** Writing the missing test revealed the three `assert_no_alloc` gates already in `src/` are INERT: that unit-test binary installs no `#[global_allocator]`, so they passed regardless of whether the code allocated. Proven by mutation — an injected allocation aborts the three new integration tests and leaves all three in-`src` gates green. The render path was always allocation-free; it simply was not gated. Original finding: A documented RT guarantee names a gating test that does not exist — the crate has no `tests/` directory. Either write the test or drop the claim. | `tutti-cpal/src/lib.rs:76-77`, `src/output.rs:104` | done |
| RT-6 | Four `unsafe impl Send`/`Sync` carry no `SAFETY:` argument, against ~40 elsewhere that do. | `tutti-vst3-host/src/host/plugin_state.rs:78-79,126-127`, `src/host/library.rs:57-58`; `tutti-plugin-server/src/loaders/au.rs:331` | open |

## 2. The sample-rate seam

The one cross-cutting concern each subsystem solved independently — the clearest
signature of bottom-up construction in the codebase.

| # | Finding | Site | Status |
|---|---|---|---|
| SR-1 | **DONE (f54b65a7).** All 18 placeholder-rate constructors now state the contract and their own audible failure. Two docs that were actively wrong are corrected. The convolvers turned out not to be rate-dependent at all — their `sample_rate` is write-only; documented as such. | `tutti-nodes` | done |
| SR-2 | **DEFERRED with reasons — not attempted.** Making the rate a mandatory constructor argument is the better fix, blocked on: three public `Default` impls (`ChorusNode`, `FlangerNode`, `BusStripNode` — `Default::default` takes no args); `dawai_model::ProcessorSpawn`, a **dyn-safe registry trait** in the APP workspace with no rate in scope, feeding 7 sites (`processor.rs:832,932`, `bus.rs:66,71,151`, `conform.rs:98,130`) this workspace cannot compile; public `tutti_spatial::build_vbap_mix` (`mix.rs:97,104,120`) with the same problem; and `conform.rs:130`, which builds a node only to call `.narrows()` and discards it. ~250 engine call sites + 5 unverifiable app ones. `bevy-tutti/src/modulation/audio_rate.rs:379` is the cheapest first step if revisited — `AudioConfig` is a resource and could be a system param. A half-migration is worse than either end state. | `tutti-nodes` + app workspace | deferred |
| SR-3 | **DONE (f54b65a7).** Both sites now state what they do to state, what they cost, and cross-reference each other and the fundsp contract: `tutti-sampler/src/stretch/unit.rs:524` preserves phase history allocation-free; `tutti-spatial/src/hrtf/panner.rs:275` rebuilds and resamples the HRIR sphere, allocating. Nothing documents what happens when a constructor argument and a later setter disagree; two crates silently differ — one preserves phase history, the other rebuilds and resamples an HRIR sphere (allocating). | `tutti-sampler/src/stretch/unit.rs:524-531`; `tutti-spatial/src/hrtf/panner.rs:275-285` | done |
| SR-4 | **DONE (f54b65a7).** `DEFAULT_SR` removed in favour of the `DEFAULT_SAMPLE_RATE` newtype; only 2 sites, both in-crate. `DEFAULT_SR` (raw `f64`) and `DEFAULT_SAMPLE_RATE` (newtype) are used interchangeably in sibling files. | `tutti-nodes/src/compressor.rs:5`, `src/gate.rs:5` | done |

## 3. Documentation

The engine is one system; the documentation is two — an accurate one inside the
crates, and an inaccurate one in the READMEs that never met it.

| # | Finding | Site | Status |
|---|---|---|---|
| D-1 | **Structural fix, highest leverage in the campaign.** The two crates using `#![doc = include_str!("../README.md")]` are the only two with zero README drift; adopting it workspace-wide would have prevented 19 of the 25 drift items found. | all crates; pattern at `tutti-midi-file/src/lib.rs:25`, `tutti-midi-hardware/src/lib.rs:87` | done |
| D-2 | The root README describes an engine that was never built: `TuttiEngine`, `TuttiGraph`, `TuttiNet`, `TransportManager`, `MidiSystem`, `chain!`/`mix!`/`stack!`, `params!`, `pipe_all`, `load_vst3`, `graph_mut` — **zero occurrences workspace-wide**. Every code block in the file is unbacked. Distinct from the planned facade crate: these are API shapes, not a missing package. | `crates/tutti/README.md` | done |
| D-3 | Nine further wholly-fictional README quick starts. | `tutti-core` (`TuttiNet`/`TransportManager`/`MeteringManager`/`PdcManager`), `tutti-sampler` (`Sampler::builder`), `tutti-analysis` (`compute_summary`/`TransientDetector`/`CorrelationMeter`; `PitchDetector` is `pub(crate)`), `tutti-export` (`Export` builder, `MidiTrack`, omits the mandatory clock), `tutti-plugin` (async `PluginClient::init`/`load_plugin`; the real ctor is 3-arg sync), `tutti-clap-host` (wrong crate name; `ClapInstance` belongs to another crate), `tutti-vst3-host` (`Vst3MidiEvent`), `bevy-tutti` (five `TuttiPlugin` builder methods, `SynthNode`, `MidiUnit`) | done |
| D-4 | The root README lists 7 crates against ~25 members, omitting `tutti-types` (the shared floor) and `tutti-cpal` (the only path to a sound card). Its one-line crate roles contradict each crate's own header: MIDI routing attributed to `tutti-core`, MPE/CC to `tutti-midi-hardware`, recording to `tutti-sampler`, spatial to `tutti-nodes`. | `crates/tutti/README.md` | done |
| D-5 | "Umbrella" names `tutti` upstairs and `bevy-tutti` downstairs — the two levels disagree on the entry point, the most basic fact a newcomer needs. | `crates/tutti/README.md:25` vs `tutti-core/src/lib.rs:8` | done |
| D-6 | Four README feature claims contradict the manifests — three of them assert exactly what a Cargo.toml comment was written to deny. | `bevy-tutti/README.md:342` (`dsp` feature), `:337` (soundfont implies synth), `tutti-export/README.md:121` (`midi`), `tutti-core/README.md:24` (`std`) | done |
| D-7 | `bevy-tutti`'s README says there is no ECS surface for export; the crate ships `ExportPlugin` and its own module doc opens "An export is an **entity**". The signature given is also wrong (4 args, `net` by value). | `bevy-tutti/README.md:277-285` vs `src/export/mod.rs:10,123` | done |
| D-8 | Bevy compatibility table says 0.17; the workspace pins 0.19. Highest-traffic line in a Bevy plugin README. | `bevy-tutti/README.md:362` | done |
| D-9 | Stale type names in rustdoc: `PluginHost::load` (no such type), three references to `ClapInstance` in a crate that has `ClapLoaded`/`ClapActive` (the name belongs to a different crate), `ClapInstance::process`. | `tutti-plugin-server/src/lib.rs:262`; `tutti-clap-host/src/instance/extensions.rs:3`, `instance/config.rs:62`, `host/mod.rs:51`; `tutti-plugin-types/src/channels.rs:43` | done |
| D-10 | Self-miscounts: `tutti-types` says "Four families" then lists six (11 modules exist); `tutti-mod`'s module map omits four modules, one of which its own prose discusses at length. | `tutti-types/src/lib.rs:4`; `tutti-mod/src/lib.rs:170-176` | open |
| D-11 | The plugin-lifecycle essay is told four times: three format crates each say "the comparison lives in `tutti-plugin`" and then deliver the comparison anyway; one explains a *different* format's design. Keep each format's own forced constraint, delete the re-litigation (~150 lines). | `tutti-plugin/src/lib.rs:98-184` (canonical); `tutti-vst3-host/src/lib.rs:53-114`; `tutti-clap-host/src/lib.rs:8-108`; `tutti-vst2-host/src/lib.rs:24-76` | done |
| D-12 | Narration to cut (~200 further lines): five design principles duplicated verbatim between a README and its `lib.rs`; two sections explaining the doc's own rustdoc markup; two commit-messages-as-comments (one counts "31 call sites" of a past refactor); one hardcoded "roughly forty methods". | `tutti-plugin/README.md:34-42`; `tutti-mod/src/lib.rs:11-17`; `tutti-au-host/src/lib.rs:12-15`; `tutti-plugin-types/src/lib.rs:122-125`; `tutti-plugin-server/src/lib.rs:152-154`; `tutti-vst3-host/src/lib.rs:68` | done |

**Protected — do not cut.** These state constraints the code cannot show, or record a
rejected alternative that would otherwise be re-litigated: `tutti-nodes/src/lib.rs:32-51`
(the mandatory shared-storage rule), `tutti-types/src/lib.rs:55-63` (why `Db: Mul` is
withheld), `tutti-io/src/lib.rs:61-65` (frames vs samples), `tutti-midi-types/src/lib.rs:61-72`,
`tutti-cpal/src/lib.rs:71-77` (RT discipline), `tutti-midi-hardware/src/lib.rs:63-75`,
`tutti-plugin-types/src/lib.rs:46-53` (the silent clamp).

## 4. Parallel families

| # | Finding | Site | Status |
|---|---|---|---|
| P-1 | **DONE (5a3c3b28).** 20 `pub mod` → 7. Every item was already root-re-exported, so the surface is unchanged. Three stay public because they export free `unsafe fn`s over raw AudioUnit pointers with no receiver. `tutti-au-host` exposes 20 `pub mod` against siblings' 1–4 — and already re-exports those types properly, so the `pub mod` lines are pure leak. Flagged independently by three auditors across three vendors. | `tutti-au-host/src/lib.rs:86-149` vs `:151-258` | done |
| P-2 | **DONE (5a3c3b28).** `get_state`/`set_state` and `get_parameter`/`get_parameter_list` across all four; error variants unified. The four format hosts do not use the shared vocabulary that already exists one layer up (`LoadStage`, the capability traits, `ParameterInfo`), and the shared trait already picked the names. Diverging: `save_state`/`load_state` vs `state`/`set_state`; `parameter_list` vs `get_parameter_list` vs none; `parameter` vs `get_parameter` returning `bool`/`()`/`&mut Self`/`Result`; `EditorError` vs `GuiError` vs `NotSupported`; `StateRestoreError` vs `StateError`; `NotActive` vs `NotActivated`. | `tutti-plugin-types/src/format_host.rs:271-363`, `load_stage.rs:14-38`, `parameters.rs:537`; the four format crates | done |
| P-3 | **DONE (5a3c3b28).** Local type deleted; its one extra field (`current`, the live value) had a single internal use and its own accessor, so nothing was lost. `tutti-vst2-host` defines a local `ParameterInfo` beside the shared one; `tutti-clap-host` already projects only the shared type. | `tutti-vst2-host/src/types.rs:74-89` vs `tutti-plugin-types/src/parameters.rs:190` | done |
| P-4 | **DONE (5a3c3b28).** `LoadFailed { component, stage, reason }` — `component` not `path`, since an AU is an OS-registered component. `OsStatus` retained for live calls. AU alone has no `LoadFailed`/`LoadStage`; the other three wrap ABI failures identically. Keep `OsStatus` for live calls. | `tutti-au-host/src/error.rs:10-178` | done |
| P-5 | **DONE (5a3c3b28).** All four now expose `has_editor`/`open_editor`/`close_editor` on the instance. AU's editor is a separate type with `has_editor` as an associated function; the other three put `has_editor`/`open_editor`/`close_editor` on the instance. VST2 has `has_editor` only as a metadata field, not a method. | `tutti-au-host/src/editor/mod.rs:30-63,81`; `tutti-vst2-host/src/types.rs:41` | done |
| P-6 | **DONE (5a3c3b28).** Renamed to `Vst2ProcessContext`/`ClapProcessContext`; structs untouched, as merging them would be a false alignment. The collision was already being worked around by an import alias. Three different local `ProcessContext` types. The **name** collision is drift — rename the locals. **Merging the structs would be a false alignment**: VST3's chord/scale/expression fields and VST2's `sample_rate` are ABI-forced. | `tutti-plugin-types/src/process.rs:44`; `tutti-vst2-host/src/types.rs:99`; `tutti-clap-host/src/instance/audio.rs:83` | done |
| P-7 | **DONE (5a3c3b28).** `Vst3Active`/`ClapActive`/`AuActive`; `Vst2Instance` kept, since VST2's fused lifecycle means it genuinely names the whole life. `Instance` names two different stages: `Vst2Instance` is the whole life, `Vst3Instance` is the active stage while CLAP's equivalent is `ClapActive`. **Decision made — see V-8: `*Active` wins.** | the format crates | done |
| P-8 | `tutti-soundfont` offers two ways to start a note — the shared MIDI inbox and public MIDI-1 scalars — where `tutti-polysynth` offers one. Hide the rustysynth-shaped scalars. | `tutti-soundfont/src/lib.rs:152,187-195` | open |
| P-9 | `VoiceSlot` names two different jobs: an allocator slot carrying identity and state, and a playback slot. Opportunistic rename of the sampler's (`pub(crate)`, no API impact). | `tutti-polysynth/src/voice.rs:136-149`; `tutti-sampler/src/voice/slot.rs:30` | open |

## 5. Vocabulary and API shape

| # | Finding | Site | Status |
|---|---|---|---|
| V-1 | MIDI newtypes die on decode: constructors take `MidiGroup`/`MidiChannel`, but `MidiEvent::group()` returns `u8`, `MidiMessage::channel()` returns `Option<u8>`, `SmfTimedEvent.channel` is `u8` — with no stated rationale at any of the three. Consumers then carry bare `u8` downstream. **Blast radius needs a counting pass first**: every match site binding `u8` (not yet enumerated). | `tutti-midi-types/src/ump/mod.rs:84`, `src/message.rs:377`; `tutti-midi-file/src/smf.rs:49`; consumer e.g. `tutti-polysynth/src/voice.rs:141` | open |
| V-2 | ~~`with_*` means two things~~ — **DECLINED, with reasons.** Measured: 60 chainable builders (`mut self -> Self`) against 61 alternate constructors — an even split, not a majority convention with stragglers. Of the 61, **25 are in `tutti-nodes`** following one internally consistent pattern (`with_channels` / `with_param_inputs` / `with_ir`, repeated identically across node types, each meaning "construct with this instead of the default") and **10 are in vendored `fundsp-tutti`**, where a rename fights upstream. `std` itself holds both senses (`Vec::with_capacity` is an alternate constructor). Renaming 61 public-API sites to enforce a distinction the language's own library does not hold would be the largest breaking change of the campaign, bought for a convention that is legible as it stands. Revisit only if a specific pair is shown to confuse a caller. | — | wontfix |
| V-3 | `PolySynth` uses `&mut self` for three atomic-backed setters where every peer uses `&self` — and hands out the same cell for `&self` writes anyway, so the `&mut` guards nothing. Keep it on the one setter that genuinely allocates. | `tutti-polysynth/src/polysynth.rs:242,298,307`; cf. `:320` | open |
| V-4 | `flush` names four unrelated operations across four crates; the bare `Plugins::flush` (which means "persist to disk") is the one that reads as the wrong verb. | `tutti-mod/src/param.rs:115`; `tutti-sampler/src/voice/node.rs:177`; `tutti-plugin/src/host/plugins.rs:377`; `tutti-clap-host/src/instance/params.rs:286` | open |
| V-6 | **Done.** Ten `AudioUnit` impls suffixed `*Node`: `BrickwallLimiter`, `Compressor`, `Gate`, `ChannelSumUnit`, `DownmixUnit`, `BusStripUnit`, `ParamSumUnit`, `ParamShaperUnit`, `AtomicSourceUnit`, `AutomationLane`. Each was confirmed to carry an `impl AudioUnit` before renaming; inner DSP objects that are *not* graph nodes (`DelayLine`, `Lfo`, `Modulator`, `BandState`) deliberately keep their names. `Compressor`/`Gate` were renamed at live code sites only — the words also name a `ClapFeature` variant in `tutti-plugin-types` and appear throughout prose, so a global sweep would have corrupted both. Original finding: `AudioUnit` impls carried three different suffixes with no rule separating them, and the split is *within* one crate: `LimiterNode` beside `BrickwallLimiterNode`, `DistortionNode` beside `Compressor` and `Gate`, `DelayLineNode` beside `DownmixNode`/`ChannelSumNode`/`BusStripNode`. 14 `*Node` vs 8 `*Unit` vs 6 bare. The crate's own doc header already states the rule its exports break: "Every node here is an `AudioUnit`". `*Unit` also re-collides with the `tutti-nodes`/`tutti-types` trap (V-5). Winner: `*Node`. | `tutti-nodes/src/lib.rs:3,119-169`; `tutti-soundfont`; `tutti-sampler/src/stretch` | done |
| V-7 | `Plugin::open` is the one place `open` does not mean an OS resource. Everywhere else the word is held with unusual discipline — device, ALSA seq, shared memory, tap lease, `open_editor` across all four hosts — and four of the five plugin crates already say `load`. Winner: `load`. | `tutti-plugin/src/host/plugin.rs:125,135`, `src/host/plugins.rs:298` | open |
| V-8 | The active plugin state has three spellings — `ClapActive`, `Vst3Instance`, `AuReady` — while the *loaded* halves already agree (`ClapLoaded`/`Vst3Loaded`/`AuLoaded`). `Vst3Instance` additionally uses AU's word for AU's *other* concept (`AuInstance` wraps the whole state machine). Winner: `*Active`, the state its own transition verb names. **This resolves P-7.** | `tutti-vst3-host/src/host/instance.rs:239`; `tutti-au-host/src/instance.rs:72,94`; cf. `tutti-clap-host/src/instance/mod.rs:103` | done |
| V-9 | 18 Bevy `Resource`s carry `*Res`, 8 do not — including `MpeModeHandle`, a Resource wearing a Handle noun three lines from `MpeModeConfig` (also a Resource) in the same file as `MidiBusRes` (which has the suffix). `AudioConfig` is both unsuffixed and collides with an unrelated `AudioConfig` in `tutti-plugin`. | `bevy-tutti/src/midi/endpoint/bus.rs:56,78`, `graph/resources.rs:16`, `graph/transport.rs:157`, `midi/endpoint/target.rs:88`, `midi/sequence.rs:85`, `midi/hardware/hardware_out.rs:198`, `midi/inbound/route.rs:135`; collision at `tutti-plugin/src/util/config/catalog.rs:90` | open |
| V-10 | `fill` means two things: write audio into an output plane (`tutti-export`, matching `slice::fill`) vs populate a control-side struct with per-block parameter/transport/harmony changes (`tutti-plugin`). Rename the latter to `refill`. | `tutti-export/src/render/driver.rs:172,186,235` vs `tutti-plugin/src/host/node/input_slot.rs:60` + 5 impls | open |
| V-11 | ~~`block_size` vs `frames`~~ — **MISDIAGNOSED; do not "fix".** The two words do not name the same quantity. Checked all 55 `frames:` parameters against 423 `block_size:` ones: outside macOS-gated `tutti-au-host` (20, unverifiable here) they are test helpers and file-length arguments — a *total* count of frames to write — plus four in `tutti-core::metering` (`tap.rs:185`, `rt.rs:35,55,86`). Those four take an interleaved buffer AND a frame count, which is precisely the frames-vs-samples distinction CLAUDE.md calls this boundary's most repeated defect; `deinterleave`'s own doc says "the first `frames` stereo samples". Renaming them to `block_size` would erase the distinction the codebase paid to establish. `block_size` names a *configured* per-block length; `frames` names *how many frames this call is about*. Both are correct where they stand. | — | wontfix |
| V-12 | **A `reset()` contract violation — verified by direct reading.** `VbapPannerNode::reset` overwrites four caller-set configuration values (`spread`, `width`, and position via `set_position`/`set_spread`), where the sibling `SvfFilterNode::reset` clears only integrator state; a host calling `reset()` to flush tails silently loses its panner placement. All ~70 bodies were read: `reset` means "clear runtime state, keep configuration" essentially everywhere. `HrtfBinauralNode::reset` resets `width` similarly. Behavioural fix, not a rename. | `tutti-spatial/src/vbap/node.rs:241`, `src/hrtf/node.rs:116` | open |
| V-13 | `OfflineTimeline::reset(start_beat)` *seeks* rather than clears, and takes an argument — its own doc says "re-seat the playhead". Rename to `seek_to`. | `tutti-core/src/transport/offline.rs:171` | open |
| V-14 | `bevy-tutti` uses "frame" for the ECS tick in ~30 doc comments while using it in the audio sense (1 sample × channels) in its own `graph/pump.rs`. No item is named `Frame`, so this is a prose pass only — but two meanings sit in adjacent files. The author already noticed the seam once. | `bevy-tutti`: `midi/inbound/route.rs:145-147`, `graph/param.rs:57,224,261`, `graph/spawn.rs:83` vs `graph/pump.rs:26-31,147` | open |
| V-15 | `MidiWriteOptions` is a construction-time by-value snapshot, which the workspace calls `Config` at ~25 other sites. | `tutti-midi-file/src/smf.rs:364` | open |
| V-5 | `tutti-nodes` (DSP nodes) and `tutti-types` (Hz/Db/Bpm measurement units) collide on "units", meaning opposite things. `tutti-core` is a vocabulary crate rather than a main crate; `tutti-cpal` names its dependency rather than its job. Rename is real churn — sequence it after the prose is true. | crate names | open |


**Refuted by the vocabulary audit — do not re-flag.** `spawn` is *not* inconsistent: it means ECS
entity spawning in `bevy-tutti` (Bevy's own word, forced) and `std::thread::spawn` everywhere else,
with no overlap. `node_id.rs` in 8 crates is an exemplary convention, not duplication — each cites
`tutti_core::node_id` as the ledger (this closes L-4). `Id` is essentially unchallenged as the
identity noun; the two other spellings justify themselves in their own docs. `Config` vs `Settings`
is a real, well-held distinction — `Config` is a construction-time snapshot, `Settings` is live
shared atomics — and must not be collapsed. `pump` in `tutti-midi-hardware` and `tutti-plugin` is a
`Drop`-body detail, not public API, so it does not collide with the audio-edge `pump`. `VoicePool`
vs `VoiceAllocator` are genuinely different things: one sums audio, one is pure bookkeeping.

## 6. Layout

| # | Finding | Site | Status |
|---|---|---|---|
| L-1 | **DONE (7e59e56c).** `bevy-tutti`'s crate `Error` lives at `src/engine/error.rs` while re-exported at the crate root — public position and file position disagree. | `bevy-tutti/src/engine/error.rs`, `src/lib.rs:139` | done |
| L-2 | **DONE (7e59e56c).** `tutti-midi-hardware` has a `src/core/` level containing everything, naming nothing. | `tutti-midi-hardware/src/core/` | done |
| L-3 | ~~The `types/` split rule is inverted~~ — **MISDIAGNOSED; do not "fix".** The audit claimed `tutti-clap-host`'s single 1207-line `types.rs` is larger than the whole of `tutti-vst3-host`'s `types/` directory. Measured: vst3's directory is **4201 lines** across five files (`events.rs` alone is 2373), three and a half times clap's file. The rule is not inverted — vst3 split because it is genuinely much bigger, and clap (1207), au (760) and vst2 (115) are each a reasonable single file. No action. | — | wontfix |
| L-4 | ~~`node_id.rs` in 8 crates~~ — **resolved: not duplication.** Verified an intentional convention; each crate cites `tutti_core::node_id` as the ledger. | 8 crates | wontfix |
| L-5 | ~~Redundant H1 headings~~ — **obsolete.** After the include_str conversion all 20 crates lead with an H1 naming the crate, including the two reference crates that never drifted. It is the house convention now, not a deviation. | — | wontfix |

## 7. Forced — recorded so they are not "fixed"

Real constraints, not drift. Each is a documented decision.

- **Plugin lifecycles differ because the ABIs differ.** VST2 fuses init/resume into its
  constructor; VST3 and CLAP have a genuine pre-activation surface; AU's initialize and
  uninitialize are both fallible, which a pure typestate cannot express.
- **VST3 carries an extra `Vst3Library` stage** — one bundle exposes many classes.
- **CLAP's `activate()` takes no rate or block size** — both are fixed at load.
- **AU constructs from an OS-registered component, not a path**, and its `process` takes
  no MIDI or transport (`AudioUnitRender` has no such arguments).
- **Native parameter domains differ** (VST2/VST3 normalized, CLAP/AU plain).
- **`tutti-sampler` has no `note_on`, allocator, or voice stealing** — it is a clip player
  with host-assigned slots and no sounding-note identity to steal. Do not MIDI-ify it, do
  not invent a shared `Voice` trait.
- **`tutti-soundfont` has no allocator and its `set_sample_rate` is a no-op** — the vendored
  synth cannot re-rate — and it drops MPE and per-note expression, being MIDI-1.
- **Every time-base wrap in the MIDI stack is earned**: clip delta-ticks (wire format),
  `Beat` (musical vs block time), SMF messages (cannot carry UMP), the decode view, and the
  OS wall-clock to frame-offset conversion. `MidiEvent` itself is defined once and flows
  through unchanged.
- **`f64` at the C ABI seam** — the unit types stop where the ABI begins.
- **`&mut self` on format-host control methods** — it is the main-thread affinity marker.
- **`bevy-tutti`'s `&mut self` setters** — Bevy change detection requires it.
- **High `pub` field counts in the format hosts and `tutti-plugin-types`** — these are FFI
  descriptor mirrors, where the field is the datum.

## 8. Sequencing

1. **§1 correctness** — RT-1 and RT-2 are verified; RT-3 and RT-4 need verification first.
2. **§2 sample rate** — SR-1's documentation fix is cheap and closes a silent-failure class immediately; SR-2 is the real repair.
3. **§4/§5 renames** — while breaking changes are still cheap. P-7 and V-5 need a decision before any mechanical work; V-1 needs its counting pass.
4. **§3 documentation** — after the names settle, so the prose is written once against final names. D-1 first: it makes the rest self-maintaining.
5. **§6 layout** — lowest risk, do last or opportunistically.

Every change lands with both workspaces green, and app-workspace fallout is fixed in the
same commit.

## 9. Forced, discovered during Wave 5

Recorded so a later pass does not "fix" them:

- **AU has no host-synthesised editor or state error.** Its editor and state calls
  return AudioToolbox status directly, so there is no failure for the host to name;
  empty variants would be alignment theatre.
- **VST2 has no `NotActive`** — the lifecycle is fused, so that state cannot exist.
- **VST3 keeps index-addressed `parameter_info`** beside the new shared-type list: it
  genuinely enumerates by index and addresses by opaque `ParamID`.
- **`tutti-plugin`'s `save_state`/`load_state` are the host-side IPC seam**, not the
  format-host layer. Renaming them would merge two deliberately separate layers.
- **The convolvers are not rate-dependent.** Their `sample_rate` is written and never
  read; the real coupling is that an impulse response carries no rate and is never
  resampled.
