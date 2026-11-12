# 023: plugin capabilities

A hosted plugin cannot *run* tutti's abstractions: its logic is opaque inside
`process`. But every format lets it *describe itself*, and what it describes is
the same set of things tutti already has pure abstractions for: how voices are
allocated (`tutti-voicing`), what a parameter measures (the units and
`UnitParam`), how long a node rings (`Shape`, `Tail`, `Io::report_tail`), what
channels it carries (`ChannelTopology`, `Fold`), what it costs (`Cost`), how it
tunes (`tutti-polysynth`), what time it needs (`Env::transport_at`, the tempo
map). Today the translation is half done and half stringly: a parameter's unit
is a free `String`, `VoiceInfo` exists only in `tutti-clap-host`, and a
plugin's tail is a number the executor trusts without ever checking it.

This design does two things.

1. **It translates each capability into tutti's vocabulary as a typed value**,
   assembled once per plugin into a `Capabilities`, with the same discipline the
   crate already applies to flags (`FeatureReport`, `ParamFlags::known`): what
   the plugin did not say is not what it said no to, and nothing is guessed from
   an untrusted string.
2. **It treats the four formats as an oracle for what tutti should abstract.**
   Together they enumerate what an audio processor can be and declare. Where all
   of them carry a concept and tutti has no type for it, the concept is real, and
   tutti's own nodes (the sampler, the SoundFont player, the compressor) will
   want to declare it too. Section 3 inventories every capability of every
   format, section 4 ranks what is missing, section 5 sketches the types.

Builds on [016](016-plugin-hosts.md) (`Loaded`, `Processor`, `Controller`) and on
[021](021-plugin-hosting-axes.md) (plugin format and hosting are two axes),
whose probe binary (021 §2.5) carries the honesty probes of §6. It uses the
runtime tail of [013](013-native-graph.md), which is built (PR #359):
`Io::report_tail` and `contract::assert_honest_tail`.

## Summary

- **A complete inventory** (§3): 188 capabilities across CLAP, VST3, AU and VST2,
  each mapped to exactly one of *Covered* (names the tutti type), *Partial*
  (what exists, what is missing), *Gap* (no abstraction; named and grouped) or
  *Host-only* (UI or plumbing, with where it lives today), and each checked
  against the host crates for whether we already read it (§3.6 has the tally).
- **Most of the vocabulary exists already**, in `tutti-plugin-types`
  (`Features`, `ParameterInfo`, `NoteExpressionType`, `Preset`, `RenderMode`,
  `TransportInfo`, harmony). The work is not new parsing. It is typing what is
  parsed (§5.2), connecting what is typed to the graph (§5.3, §5.4) and checking
  what plugins claim (§6).
- **Nineteen gap groups, ranked** (§4). The three every format carries are
  *layout options*, *per-note expression* and *a typed preset library*; a
  fourth, *note names and keyswitches*, is in three of four. Each of them is something a tutti
  node would declare too, so they are engine abstractions, not hosting
  conveniences. Undo, track context and the remote-control pages are the
  opposite: hosting or application concerns that the engine should carry as
  plain data and nothing more.
- **The first three steps are small and self-contained** (§9): the parameter
  unit and kind, the tail into `Io::report_tail`, then the tail and latency
  probes. The voice description follows.
- **Out of scope here**: exporting tutti nodes as plugins (§7, parked) and ARA
  (§8).

## 1. Current state

What is typed, what is a string, and what stops at a format host.

| Concern | Today | Where |
|---|---|---|
| Capability flags | One `Features` bitset plus a `probed` mask (`FeatureReport`); a clear bit with a clear `probed` bit means "not asked" | `tutti-plugin-types` `features.rs`, `LoadedPlugin` |
| Parameter unit | `ParameterInfo.unit: String` stays as the display text (VST3 the `units` string, VST2 the label, AU its own `ParameterUnit` print with 9 of the 27 codes decoded, CLAP `""`), and `ParameterInfo.measures: Option<ParamUnit>` types it where the format declared it exactly and the range fits (step 1, built) | `tutti-plugin-types` `param_unit.rs`, `tutti-vst3-host` `loaded/parameters.rs`, `tutti-vst2-host` `parameters.rs`, `tutti-clap-host` `instance/params.rs`, `tutti-au-host` `unit_code.rs` |
| Parameter kind | `ParameterInfo.role: Option<UnitParam>`: `Bypass` from the CLAP and VST3 flag, `Wet` from VST3's `DryWetMix`, `Pan` from AU's pan code; never from a name (step 1, built) | same |
| Tail | `PluginTail` (an alias of `tutti_types::Tail`) stored in `PluginControls` and handed to `Shape::with_tail` in `declared_shape`. The figure is the **plugin's claim**, and the executor parks the node on it; nothing measures it, and nothing calls `Io::report_tail` | `tutti-plugin` `host/node/graph_node.rs` |
| Latency | `LoadedPlugin.latency_samples`; the node declares it plus the IPC chunk. Also a claim, unmeasured | same |
| Voices | `VoiceInfo { voice_count, voice_capacity, supports_overlapping_notes }` in `tutti-clap-host`; no other format has a counterpart; `tutti-voicing` is not involved | `tutti-clap-host` `types.rs` |
| Note expression | `NoteExpressionType` (volume, pan, tuning, vibrato, brightness, pressure, expression, custom) and the per-block value list; the *description* of an expression (range, bipolar, one-shot) is read by `tutti-vst3-host` and does not cross | `tutti-plugin-types` `note_expression.rs` |
| Presets | `Preset`, `PresetId`, `PresetSupport`; list and load only | `tutti-plugin-types` `presets.rs` |
| Render | `RenderMode { Realtime, Offline }`; VST3 and VST2 "prefetch" is deliberately dropped | `tutti-plugin-types` `render_mode.rs` |
| Cost | A hosted plugin declares nothing, so it is `Cost::Heavy`, the default | `tutti-graph` `cost.rs` |
| Channels | Per-bus `ChannelLayout` and an optional `ChannelTopology`; `LayoutSupport` says how much of it the plugin reported. The *set* of layouts a plugin can accept is not stored (`tutti-clap-host` keeps `AudioPortConfig` in its own types) | `tutti-plugin-types` `metadata.rs`, `layout_support.rs` |
| Identity | `PluginDescriptor` and `PluginClass` in the catalog; `PluginDb` rows are `FileState::Probed(Vec<PluginDescriptor>)` or `Blacklisted` | `tutti-plugin-types` `descriptor.rs`, `tutti-plugin-db` `table.rs` |

Three observations drive the design.

- **The unit is the loss.** Three formats carry a unit and all three flatten it
  to text; a consumer that wants "this parameter is a cutoff in hertz" has to
  parse display strings. AU's code is a real enumeration and is the most honest
  of the four, and it is the one we reduce to text.
- **A claim is used as a fact.** `Shape::tail` is the ceiling the executor
  parks on. A plugin that reports a tail shorter than it rings is cut off by the
  silence skip, and a plugin that reports nothing (VST2 answers 0 for both "no
  tail" and "I do not implement this") is either never skipped or wrongly
  skipped. Nobody has measured one.
- **Each format's richest declarations stop at its host crate.** The CLAP host
  reads voice info, note names, remote controls, track info, tuning, triggers
  and undo; the VST3 host reads the keyswitch list, the physical-UI map, the
  function names and the MIDI mapping; the AU host reads MIDI mapping, overview
  parameters and the presentation latency. None of it reaches the vocabulary the
  graph and the application share.

## 2. Principles

1. **A capability is a typed value in our vocabulary, or it is absent.** The
   sum type that carries it keeps "the plugin did not say" apart from "the
   plugin said no", exactly as `FeatureReport` and `ParamFlags::known` do.
   `Reported<T>` (§5.1) generalises that to a value.
2. **Never guess from an untrusted string.** A typed unit comes from AU's
   numeric code (a closed enumeration), from an exact match against a short
   closed list (VST3 `units`, VST2 label), or from a function name the format
   defines (VST3 `IParameterFunctionName`). A parameter's *name* is never read
   for meaning. The result is an `Option`; `None` is the common case and every
   consumer already handles it, because the flat string stays.
3. **A claim a probe can contradict is a claim.** Latency and tail are what the
   plugin says; the probe (§6) is what it does. Disagreement is recorded, never
   silently fixed, and a measurement can only *raise* a safety figure.
4. **The abstraction is real when a tutti node would declare it.** A concept
   that only a hosted plugin has is hosting vocabulary and stays in
   `tutti-plugin-types`. A concept four formats carry and a native node could
   state (a SoundFont drum kit has note names; a compressor has a gain
   reduction) is an engine abstraction and belongs in `tutti-types` or the
   crate that owns the matching pure model (§4).
5. **Declared once, read where it is used.** `Capabilities` is built on the
   control side, before activation (`Loaded` already has the whole pre-activation
   surface), is immutable, and is replaced whole when the plugin says something
   changed. The audio thread never reads it; it reads the per-block gate
   (`Features::CONSUMES`), which stays one mask-and-compare.

## 3. The inventory

### 3.1 How to read the tables

One row per capability a format offers a host or asks of one. The identifier is
the format's own name for it, given so it can be found; the description is ours.
Header citations are by file name.

- **Map** is exactly one of:
  - **Covered**: names the tutti type the capability already maps to.
  - **Partial**: what exists and what is missing.
  - **Gap G*n*** : no tutti abstraction; the candidate is named in §4 and
    sketched in §5.
  - **Host-only**: UI or host plumbing, not an engine abstraction; says where it
    lives today.
- **Read today** says how far we read it: `types` (a value in
  `tutti-plugin-types` exists and the format host fills it), `host` (the format
  host crate has an accessor; the value stops there), `ffi` (only the `-ffi`
  crate answers the callback, nothing reads it), `no` (nothing in the tree).
- CLAP identifiers are from `clap-sys` 0.5.0 (`src/ext`, `src/ext/draft`,
  `src/factory`). Four CLAP extensions in the current spec are **not in that
  source**: gain-adjustment-metering, mini-curve-display, project-location and
  scratch-memory. Their rows are marked *spec* and come from the published CLAP
  headers as I know them, unchecked here. AU names are AudioToolbox's, from the
  properties the `-ffi` and `-host` crates use and from memory for the rest
  (the headers are not on this machine). The VST2 SDK is not vendored; the
  opcodes are `vst-tutti`'s `OpCode` enum and the `canDo` strings are the ones
  the host reads, plus the standard set from memory.

### 3.2 CLAP

| Id | Capability | Declares | Map | Read today |
|---|---|---|---|---|
| C1 | factory: plugin descriptor | id, name, vendor, url, manual, support, version, description, feature strings | Covered: `PluginDescriptor` (url, manual, support and description are dropped) | types |
| C2 | plugin features (`plugin_features.rs`) | category and sound-class strings (instrument, audio-effect, note-effect, analyzer, reverb, drum, ...) | Covered: `ClapFeature`, `PluginRole` | types |
| C3 | factory: several plugins per file | a file lists many descriptors | Covered: `FileState::Probed(Vec<PluginDescriptor>)` | types |
| C4 | factory: preset discovery (`preset_discovery.rs`) | locations (file, plugin), file types, providers, per-preset name, creators, description, flags (factory, user, demo, favorite), features, timestamps | Gap G3: `PresetCatalog` | no (a constant only) |
| C5 | factory draft: plugin-invalidation | the plugin tells the host its search paths or files changed | Host-only: a `PluginDb` rescan trigger; no call site | no |
| C6 | factory draft: plugin-state-converter | converts a saved state from another format or id into this plugin's | Gap G16: `IdentityAlias` (with VST3 compat) | no |
| C7 | lifecycle: init, activate(min, max frames, rate), start/stop processing, reset | when the plugin is allowed to allocate and the block bounds | Covered: `Loaded` typestate, `Prepare`, `MaxBlock`, `Node::reset` (the min frame count has no use) | types |
| C8 | process status: continue, continue-if-not-quiet, tail, sleep, error | what the plugin tells the host after each block, including that it is asleep | Partial: `Status::Idle` and `Status::Silent` are the matching claims; the host reads `is_sleeping` and `is_tailing` and never turns them into a `Status` (§5.3) | host |
| C9 | audio buffers: constant mask, steady time | which channels are one value; a monotonic counter | Covered: `ConstantMask`, `SilenceMask`; steady time has no reader | host |
| C10 | `clap.params`: info, value, text conversion | id, name, module path, min, max, default, value-to-text and text-to-value | Covered: `ParameterInfo`, `ParamRange`; text conversion is `host` only | types |
| C11 | params flags: stepped, periodic, hidden, readonly, bypass, automatable | what the host may do | Covered: `ParamSteps`, `ParamFlags` (`WRAP`, `HIDDEN`, `READ_ONLY`, `BYPASS`, `AUTOMATABLE`) | types |
| C12 | params flag: is-enum | the stepped values are named choices | Partial: `ParamSteps` knows the count, not the labels; the flag types the parameter `ParamUnit::Choice` (step 1, built) | types |
| C13 | params flags: automatable per note id, key, channel, port; modulatable per the same | a parameter value may target one voice | Partial: `ParamFlags::PER_NOTE_ID/KEY/CHANNEL/PORT`; no modulation destination carries a voice (G2) | types |
| C14 | params flag: modulatable and the param-mod event | a modulation offset kept apart from the value | Partial: `ParamFlags::MODULATABLE`; `tutti-modulation` lanes drive the plugin's own knob, one value per block, never per voice | types |
| C15 | params flag: requires-process | a value change is only heard through `process`, not a flush | Host-only: `tutti-clap-host` `param_requires_process` picks flush or process | host |
| C16 | param-value and gesture begin/end events | sample-accurate edits and "the user is holding the knob" | Covered: `ParameterPoint`, `ParameterQueue`; gestures are `host`-level (`AutomationMode` touch and latch) | types |
| C17 | params host: rescan (values, text, info, all) and clear (automations, modulations) | the parameter list or its metadata changed | Host-only: the 016 `Lifecycle` rescan `Effect`; `take_params_flush_requested` | host |
| C18 | `clap.param-indication` | host tells the plugin which parameter is mapped to a hardware control (color, label) and the automation state (present, playing, recording, overriding) | Partial: `Features::AUTOMATION_STATE` carries the state; the mapping half is G6 | host |
| C19 | `clap.remote-controls` | pages of 8 parameters with a name, and a suggested page | Gap G7: `ParamPages` | host |
| C20 | `clap.note-ports`: ports, supported and preferred dialect (clap, midi, mpe, midi2), name | what note language each port speaks | Partial: `Features::MIDI_IN/OUT`; the dialect is not stored. tutti is UMP-native, so a dialect is what to translate to (`tutti-midi-protocol`) (§5.4) | host |
| C21 | note on and off events with note id, port, channel, key, velocity | a voice, named | Covered: `Ump` notes; the note id is `tutti_voicing::VoiceId`'s counterpart | types |
| C22 | note choke event | cut a voice now, no release | Partial: `Command::Cut` exists in `tutti-voicing`; no host sends a choke | host |
| C23 | note end event (plugin to host) | the plugin's voice has finished | Gap G19: `VoiceEnded` | host |
| C24 | note expression events: volume, pan, tuning, vibrato, expression, brightness, pressure | per-note continuous control | Partial: `NoteExpressionType` is the vocabulary; no graph event type carries it (G2) | types |
| C25 | MIDI, MIDI sysex and MIDI2 events | raw wire messages | Covered: `Ump`, `tutti_ump::SysexRef`, `tutti_ump::SysexAssembler` | types |
| C26 | event flags: is-live, dont-record | the event came from a person; do not record it | Host-only: sequencer policy | no |
| C27 | `clap.event-registry` | host lets a plugin register its own event space | Host-only: `tutti-clap-ffi` answers; no space is used | ffi |
| C28 | transport event: tempo and its increment, beats and seconds timeline, loop start and end, bar start and number, time signature, flags (has-*, playing, recording, loop active, within pre-roll) | what the plugin may read about time | Partial: `TransportInfo` (tempo, position, loop, bar); tempo increment and the pre-roll flag are not carried | types |
| C29 | `clap.audio-ports`: count, id, name, channel count, port type (mono, stereo, surround, ambisonic), flags (main, 64-bit, prefers 64-bit, common sample size), in-place pair | every bus | Covered: `LoadedPlugin` buses, `Features::F64_AUDIO`, `Shape::with_in_place` (the pair is read, not mapped) | types |
| C30 | `clap.audio-ports-config` and `-config-info` | named whole-plugin layouts the host can select, and the info of the current one | Gap G1: `LayoutOptions` | host |
| C31 | `clap.configurable-audio-ports` | host proposes a layout; plugin accepts or refuses | Gap G1: `LayoutOptions` (negotiated form) | host |
| C32 | `clap.audio-ports-activation` | enable or disable one port, while processing or not | Gap G13: `BusActivation` | host |
| C33 | draft `clap.extensible-audio-ports` | add a port to a running plugin | Gap G18: `DynamicPorts` | host |
| C34 | `clap.surround` | which speaker each channel feeds | Covered: `ChannelTopology`, `Fold` | types |
| C35 | `clap.ambisonic` | ordering (FuMa, ACN) and normalization (MaxN, SN3D, N3D, SN2D, N2D) | Gap G11: `Ambisonic` | host |
| C36 | `clap.latency` | processing latency, and a "changed" callback | Covered: `Shape::with_latency`, `Latency`, `Effect` re-read | types |
| C37 | `clap.tail` | ring-out in samples (u32 max is infinite), and a "changed" callback | Partial: `PluginTail`, in `Shape::tail`; never narrowed live, never checked (§5.3, §6) | types |
| C38 | `clap.render`: realtime or offline; hard-realtime requirement | quality mode, and "do not run me off the audio thread" | Partial: `RenderMode`; the hard-realtime flag has no home (§5.6) | types |
| C39 | `clap.state` | save and load a blob | Covered: the required state save and restore (`StateError`) | types |
| C40 | `clap.state-context` | the blob is for a preset, a duplicate or a project | Partial: `StateContext` in `tutti-clap-host`; the other formats cannot say | host |
| C41 | `clap.preset-load` | load a preset from a location | Covered: `PresetId::Location`, `Features::PRESET_LOAD` | types |
| C42 | `clap.voice-info` | voice count, voice capacity, supports-overlapping-notes | Gap G19: `DeclaredVoices` into `tutti_voicing::Rules` (§5.5) | host |
| C43 | `clap.note-name` | a name for a (port, channel, key) | Gap G4: `NoteNames` | host |
| C44 | draft `clap.tuning` | the host sends tuning tables; the plugin can list tunings | Partial: `tutti_polysynth::Tuning` is the model but lives at layer 5 (§5.6) | host |
| C45 | `clap.track-info` | host tells the plugin the track name, color, channel count, kind (return, bus, master) | Gap G10: `TrackContext` | host |
| C46 | spec: project-location (not in the sys crate) | host tells the plugin the project path and the position of the track in the project tree | Gap G10: `TrackContext` | no |
| C47 | `clap.context-menu` | plugin adds items to a host menu and the host adds items to the plugin's | Host-only: `tutti-clap-ffi` `menu.rs` | host |
| C48 | `clap.gui` | window APIs (x11, wayland, win32, cocoa), embedded or floating, resize hints | Host-only: `Features::EDITOR`, `EDITOR_RESIZE`, `EDITOR_FLOATING`; `tutti-plugin-types` `editor.rs` | types |
| C49 | `clap.timer-support` | host-driven timers | Host-only: `poll_timers` | host |
| C50 | `clap.posix-fd-support` | host polls a plugin's file descriptors | Host-only: `poll_posix_fds` | host |
| C51 | `clap.thread-check` | is this the main or audio thread | Host-only: debug assertion in `tutti-clap-ffi` | ffi |
| C52 | `clap.log` | plugin log lines at a severity | Host-only: `drain_log` | host |
| C53 | `clap.thread-pool` | plugin asks the host to run N tasks across threads inside `process` | Partial: `set_thread_pool` binds a pool; the node stays `Cost::Heavy` (§5.6) | host |
| C54 | draft `clap.undo` (+ context, delta) | the plugin exposes undo steps, delta formats, and what the host may undo | Gap G15: `UndoSteps` | host |
| C55 | draft `clap.triggers` | stateless trigger parameters, automatable per note | Gap G17: `ParamKind::Trigger` | host (`clap-extras`) |
| C56 | draft `clap.resource-directory` | a folder the plugin may store shared files in | Host-only: `resource_*` accessors | host |
| C57 | draft `clap.transport-control` | the plugin asks the host to start, stop, seek, loop, set tempo | Host-only: `drain_transport_requests`; a request becomes a `Transport` command with `At::NextBlock` | host |
| C58 | spec: gain-adjustment-metering (not in the sys crate) | the gain change the plugin currently applies, in dB, and whether it is a reduction | Gap G9: `Readout::GainChange` | no |
| C59 | spec: mini-curve-display (not in the sys crate) | the plugin draws a curve (EQ response, compressor transfer) into a small host widget, with the curve's kind and axis hints | Gap G8: `CurveResponse` | no |
| C60 | spec: scratch-memory (not in the sys crate) | the host lends scratch memory for the duration of a call | Host-only: `Prepare` already bounds every buffer | no |
| C61 | host requests: restart, process, callback | the plugin needs a re-activation, a process call, or the main thread | Host-only: the `Lifecycle` restart `Effect`, `take_process_requested`, `on_main_thread` | host |

### 3.3 VST3

Interfaces are in `vendor/vst3-sdk/pluginterfaces/vst` and `.../base`.

| Id | Capability | Declares | Map | Read today |
|---|---|---|---|---|
| V1 | `IPluginFactory` 1, 2, 3 (`PClassInfo`, `PClassInfo2`, `PClassInfoW`) | class id, category, name, vendor, version, sub-categories, SDK version | Covered: `PluginDescriptor`, `Vst3SubCategories`, `Vst3PlugType` | types |
| V2 | `IPluginCompatibility` | ids this class replaces | Gap G16: `IdentityAlias` | host |
| V3 | `IComponent` buses: `BusInfo` (media type audio or event, direction, channel count, name, main or aux, default-active, control-voltage) | every audio and event bus | Partial: `LoadedPlugin` buses; the control-voltage flag has no type (G1) | types |
| V4 | `IComponent::getRoutingInfo` | which input bus feeds which output bus | Gap G13: `BusRouting` | host |
| V5 | `IComponent::setActive`, `setIoMode` (simple or advanced; obsolete) | lifecycle | Host-only: `Loaded::activate` | types |
| V6 | `IComponent` and `IEditController` state | two blobs | Covered: required state save and restore | types |
| V7 | split controller, `IConnectionPoint`, `IMessage`, `IAttributeList` | component and controller exchange opaque messages | Host-only: `tutti-vst3-host` `shell.rs`; `tutti-vst3-ffi` `message.rs` | host |
| V8 | `IAudioProcessor::setBusArrangements`, `getBusArrangement` | the host proposes a speaker set and the plugin answers with what it uses | Gap G1: `LayoutOptions` | host |
| V9 | `IAudioProcessor::canProcessSampleSize` | 32 or 64 bit | Covered: `Features::F64_AUDIO` | types |
| V10 | `IAudioProcessor::getLatencySamples` | latency | Covered: `Latency` | types |
| V11 | `IAudioProcessor::getTailSamples` (no-tail, infinite-tail sentinels) | ring-out | Partial: as C37 | types |
| V12 | `ProcessSetup`: process mode, sample size, max block, rate | per-activation configuration | Covered: `Prepare`, `RenderMode` | types |
| V13 | process modes: realtime, prefetch, offline | pressure the plugin is under | Partial: `RenderMode` drops prefetch on purpose (G14) | types |
| V14 | `IProcessContextRequirements` | which fields of the process context the plugin reads (system time, continuous samples, music position, bar position, cycle, clock offset, tempo, time signature, chord, frame rate, transport state) | Partial: one bit, `Features::TRANSPORT`; the finer set would let a node skip fields it never reads | host |
| V15 | `ProcessContext` fields: state flags, project samples, system time, continuous samples, music position, bar position, cycle bounds, tempo, time signature, chord, SMPTE offset and frame rate, samples to next MIDI clock | time | Partial: `TransportInfo` carries tempo, position, loop, bar, system time; SMPTE frame rate and the clock offset are not carried; chord is in `harmony` | types |
| V16 | `IAudioPresentationLatency` | extra latency the plugin wants for what the user hears, per bus | Gap G12: `PresentationLatency` | host |
| V17 | `IPrefetchableSupport` | never, yes, or always prefetchable | Gap G14: `PrefetchSupport` | host |
| V18 | `ProcessData`: parameter changes in and out with sample offsets | automation, and parameter values the plugin reports back | Partial: `ParameterChanges` in; the output queue (a meter parameter, say) is dropped | types |
| V19 | events: note on (id, channel, pitch, tuning, length, velocity), note off, poly pressure | notes | Partial: `Ump` notes; per-note tuning and note length are not carried | types |
| V20 | events: data (sysex) | raw bytes | Covered: `tutti_ump::SysexRef` | types |
| V21 | events: note expression value, text, integer value | per-note control, text and integer forms | Partial: value is `NoteExpressionValue`; text and integer are in `harmony` (`NoteExpressionTextValue`, `NoteExpressionIntValue`) | types |
| V22 | events: chord, scale | what the sequencer's harmony track says | Partial: `ChordValue`, `ScaleValue` in `harmony.rs`; no graph event, no tutti node reads it (VST3-only, `SEQUENCER_CONTEXT`) | types |
| V23 | event: legacy MIDI CC out | plugin emits CC, pitch bend, aftertouch, program change | Covered: `Features::MIDI_OUT`, `Ump` | types |
| V24 | `IEditController::getParameterInfo` | id, title, short title, units string, step count, default, unit id, flags | Covered: `ParameterInfo` (the units string is its own row, V59) | types |
| V25 | parameter flags: can-automate, read-only, wrap-around, is-list, hidden, program-change, bypass | what the host may do | Covered: `ParamFlags` (`PROGRAM_CHANGE`, `BYPASS`, `WRAP`, ...); is-list is `ParamSteps` without labels (as C12) | types |
| V26 | normalized to plain and back; value to string and back | the plugin's own mapping | Covered: `ParamRange::to_plain`; strings `host` | types |
| V27 | `IUnitInfo`: unit tree (id, parent, name, program list), `getUnitByBus`, selected unit | parameter grouping, and which unit belongs to a bus | Partial: flattened to `ParameterInfo.group`; the tree and the bus link stay in `tutti-vst3-host` (doc 010) | host |
| V28 | program lists: `ProgramListInfo`, program names, program info attributes (name, category, instrument, style, character, state type, file), `hasProgramPitchNames`, `getProgramPitchName` | presets with metadata, and per-program key names | Partial: `Preset`, `PresetId::Program`; attributes and per-program key names are dropped (G3, G4) | types |
| V29 | `IProgramListData`, `IUnitData` | the state of one program or one unit | Gap G3: `PresetCatalog` (export or import a single preset) | no |
| V30 | `IUnitHandler`, `IUnitHandler2` | the plugin tells the host the selected unit, a program list or a bus-unit link changed | Host-only: rescan notices in the `Lifecycle` | host |
| V31 | `INoteExpressionController`: count, `NoteExpressionTypeInfo` (type id, title, units, unit id, value description with min, max, default, steps, associated parameter, flags bipolar, one-shot, absolute) | which per-note controls exist and how they behave | Gap G2: `ExpressionInfo` | host |
| V32 | `INoteExpressionPhysicalUIMapping` | which physical axis (x, y, pressure) drives which expression | Gap G2: `ExpressionInfo::physical` | host |
| V33 | `IKeyswitchController` | named key ranges that switch articulation | Gap G4: `NoteNames::keyswitches` | host |
| V34 | `IMidiMapping` | the parameter a CC on a bus and channel drives | Gap G6: `MidiMap` | host |
| V35 | `IMidiMapping2`, `IMidiLearn2` (MIDI 2.0 controllers) | the same for registered and assignable controllers, per note | Gap G6: `MidiMap` | no |
| V36 | `IMidiLearn` | host forwards live MIDI so the plugin can learn | Partial: `tutti-vst3-host` `midi_learn.rs`; part of G6 | host |
| V37 | MIDI CC as parameters: the extra controller numbers for pitch bend, aftertouch, program change (`ivstmidicontrollers.h`) | which pseudo-CCs exist | Covered: `Ump` voice messages | types |
| V38 | `IParameterFunctionName` | well-known roles a parameter plays: compressor gain reduction (and max, peak hold, reset), low-latency mode, dry-wet mix, randomize, pan position x, y, z | Partial: `DryWetMix` is read once per unit in `Vst3Loaded::parameter_list` and gives `role = Wet` (step 1, built); the rest is G9 (§5.2) | host |
| V39 | `IRemapParamID` | old parameter ids that map to new ones | Host-only: automation and state compatibility; `tutti-vst3-host` reads it | host |
| V40 | `IXmlRepresentationController` | an XML layout of parameters, including remote-control pages | Gap G7: `ParamPages` | host |
| V41 | `IChannelContextInfo` via `IInfoListener` | channel name, color, index and namespace, plugin location | Gap G10: `TrackContext` | no |
| V42 | `IAutomationState` | the host tells the plugin the automation state | Partial: `Features::AUTOMATION_STATE`, `AutomationMode` | types |
| V43 | `IComponentHandler`: begin, perform, end edit; `restartComponent` flags (reload, io, values, latency, titles, MIDI CC mapping, note expression, io titles, prefetchable, routing, keyswitch) | edits and "this part of my description changed" | Partial: edits are automation; the restart flags are the list of what a `Capabilities` re-read must cover (§5.1) | types |
| V44 | `IComponentHandler2`: set dirty, request editor, group edit | edit grouping | Host-only: `tutti-vst3-ffi` `component_handler.rs` | ffi |
| V45 | `IComponentHandler3`, `IContextMenu` | context menu | Host-only | ffi |
| V46 | `IComponentHandlerBusActivation` | the plugin asks the host to activate a bus | Gap G13: `BusActivation` | ffi |
| V47 | `IProgress` | progress of a long load | Host-only | host |
| V48 | `IEditController2` (knob mode, help, about), `IEditControllerHostEditing`, `IComponentHandlerSystemTime` | small UI hooks and a host clock | Host-only | no |
| V49 | `IPlugView`, `IPlugFrame`, `IRunLoop`, content scale, Wayland host/frame | the editor | Host-only: `Features::EDITOR*`; `tutti-vst3-host` `loaded/editor.rs` | host |
| V50 | `IParameterFinder` | which parameter is under a view coordinate | Host-only | ffi |
| V51 | `IDataExchange` (handler, receiver, block) | a lock-free queue from the processor to its own editor | Host-only: `tutti-vst3-ffi` advertises; tutti's own equivalents are the taps and meters | ffi |
| V52 | `IStreamAttributes` | metadata on a state stream (file name, time signature, ...) | Host-only | no |
| V53 | `IHostApplication`, `IPlugInterfaceSupport` | host name; which interfaces the host implements | Host-only: `tutti-vst3-ffi` | ffi |
| V54 | `IInterAppAudio` and its preset manager | iOS inter-app audio | Host-only: platform | no |
| V55 | `IVst3ToVst2Wrapper`, `...ToAUWrapper`, `...ToAAXWrapper`, `IVst3WrapperMPESupport` | identify a wrapper; ask it to enable MPE | Gap G2: `ExpressionInfo` (MPE declaration, with AU `SupportsMPE`) | no |
| V56 | `ITestPlugProvider` | test harness | Host-only | no |
| V57 | speaker arrangements (`vstspeaker.h`): named sets, ambisonic orders 1 to 7 in ACN, 3D formats | channel placement | Partial: `ChannelTopology` for speakers; ambisonic is G11 | types |
| V58 | `.vstpreset` keys (`vstpresetkeys.h`): media type, plugin name and category, name, comment, musical category, instrument, style, character, state type | preset file metadata | Gap G3: `PresetCatalog` | no |
| V59 | `ParameterInfo::units` (the units string) | a display unit such as Hz or dB, free text | Partial: passed through as `ParameterInfo.unit` and, for the closed list of eight strings over a plain range that fits, typed as `ParameterInfo.measures` (step 1, built); VST3 declares normalized values only, so most are `None` | types |

### 3.4 AU (v2)

`kAudioUnitProperty_*` unless noted. `tutti-au-ffi` declares the property
types, `tutti-au-host` reads them.

| Id | Capability | Declares | Map | Read today |
|---|---|---|---|---|
| A1 | `AudioComponentDescription` (type aumu, aumf, aufx, aumi, ...; subtype; manufacturer) and version | identity and what the unit does | Covered: `AuComponentType`, `PluginClass::Au`, `PluginRole` | types |
| A2 | `ParameterList`, `ParameterInfo` (name, unit, min, max, default, flags, clump) | every parameter, per scope and element | Covered: `ParameterInfo` | types |
| A3 | parameter units, `kAudioUnitParameterUnit_*` (generic, indexed, boolean, percent, seconds, sample frames, phase, rate, hertz, cents, relative semitones, MIDI note, MIDI controller, decibels, linear gain, degrees, equal-power crossfade, mixer fader curve, pan, meters, absolute cents, octaves, BPM, beats, milliseconds, ratio, custom) | what the number measures | Partial: all 27 codes map in `tutti_au_host::unit_measures` (18 typed, `Pan` also a role; step 1, built); `ParameterUnit` still prints 9 as text and the rest as `unit(N)` | host |
| A4 | parameter flags: writable, readable, can-ramp, meter read-only, non-realtime, expert, omit from presets, plot history, has name string, values have strings, has clump, global or element meta | what the host may do | Partial: writable, ramp, values-have-strings are used; meter read-only is G9; non-realtime and expert have no flag | host |
| A5 | parameter display curve flags: logarithmic, exponential, squared, cubed, square root, cube root | how a slider should map the range | Partial: `ParamRange` carries no skew; the flag is read and dropped | host |
| A6 | `ParameterClumpName` and the clump id | grouping | Covered: `ParameterInfo.group` | types |
| A7 | `ParameterValueStrings`, `ParameterStringFromValue`, `ParameterValueFromString` | names of indexed values and text conversion | Partial: text conversion is `host` only; the labels of a choice are not carried (§5.2) | host |
| A8 | `DependentParameters` | changing this parameter changes the metadata of those | Host-only: rescan | host |
| A9 | `ParametersForOverview` | a short list of the parameters worth showing first | Gap G7: `ParamPages` | host |
| A10 | `StreamFormat`, `SampleRate`, `MaximumFramesPerSlice`, `ElementCount`, `ElementName` | per-bus format and bounds | Covered: `Prepare`, `LoadedPlugin` buses | types |
| A11 | `SupportedNumChannels` | the (in, out) channel counts the unit accepts, with -1 for any | Gap G1: `LayoutOptions` | host |
| A12 | `AudioChannelLayout`, `SupportedChannelLayoutTags` | current and accepted placements | Partial: current placement is `ChannelTopology`; the accepted set is G1; ambisonic tags are G11 | types |
| A13 | `Latency` | seconds of latency | Covered: `Latency` | types |
| A14 | `TailTime` | seconds of ring-out | Partial: as C37 | types |
| A15 | `BypassEffect` | soft bypass | Covered: `UnitParam::Bypass`, `Status::Bypass` | types |
| A16 | `InPlaceProcessing` | the unit may write over its input | Covered: `Shape::with_in_place` | host |
| A17 | `ShouldAllocateBuffer` | the host provides or the unit allocates a bus buffer | Host-only: `tutti-au-ffi` `render.rs` supplies buffers | no |
| A18 | `RenderQuality` (maximum to minimum, 0 to 127) | a graded quality the host chooses | Gap G5: `Quality` | host |
| A19 | `OfflineRender` | the host is rendering without time pressure | Covered: `RenderMode::Offline` | types |
| A20 | `HostCallbacks` (beat and tempo, musical time location, transport state) | the host's time | Partial: `TransportInfo`, as C28 | types |
| A21 | `MIDIOutputCallback`, `MIDIOutputCallbackInfo`, `MIDIOutputEventListCallback` | the unit emits MIDI, and names its outputs | Covered: `Ump` out, `Features::MIDI_OUT` (named outputs are not carried) | host |
| A22 | `AudioUnitMIDIProtocol`, `HostMIDIProtocol` | MIDI 1.0 or 2.0 (UMP) | Partial: the protocol is read; not stored. Same as the CLAP dialect (§5.4) | host |
| A23 | `SupportsMPE` | the unit takes MPE | Gap G2: `ExpressionInfo` (MPE declaration) | no |
| A24 | MusicDevice API: start note (instance id, group, pitch, velocity, per-note parameters), stop note, MIDI event, sysex | notes with an instance id and per-note parameters | Partial: notes are `Ump`; per-note parameters are G2 | host |
| A25 | scopes and elements: global, input, output, group, part, note, layer | where a parameter lives | Partial: group and note scope are per-channel and per-note parameters (G2); input and output are buses | host |
| A26 | `ClassInfo`, `ClassInfoFromDocument` | the saved state, and the document variant | Covered: required state save and restore (the document variant has no counterpart) | host |
| A27 | `FactoryPresets`, `PresentPreset` | a preset list with numbers, and the current one | Partial: `PresetId::Number`, `Preset` (no user/factory distinction) | types |
| A28 | user presets (`.aupreset`, AUv3 `userPresets`) | presets the user saved | Gap G3: `PresetCatalog` | host (`aupreset.rs`) |
| A29 | `MIDIControlMapping`, `AllParameterMIDIMappings` | CC to parameter assignments, read and write | Gap G6: `MidiMap` | host (`midi_map.rs`) |
| A30 | `FrequencyResponse` | magnitude at requested frequencies | Gap G8: `CurveResponse` | no |
| A31 | `PresentationLatency` | extra latency for what the user hears | Gap G12: `PresentationLatency` | host |
| A32 | `ContextName` | the name of the track or context | Gap G10: `TrackContext` | host |
| A33 | `CocoaUI`, `IconLocation`, `NickName` | editor, icon, display name | Host-only: `tutti-au-ffi` `editor/`, `tutti-au-host` `identity.rs` | host |
| A34 | `LastRenderError`, `SetRenderCallback`, render notify (pre-render, post-render, output-is-silence) | render diagnostics and observation | Covered: `Status::Silent` for output-is-silence; the rest `host` | host |
| A35 | parameter events and listeners: schedule parameters, gesture begin and end, property change | sample-accurate edits and UI feedback | Partial: `ParameterChanges` (the AU path is `○` in the capability table); gestures are `host` | host |
| A36 | `ParameterIDName`, `ParameterHistoryInfo`, `MeteringMode`, `MeterClipping` | name by id, plot history, meter control | Host-only | no |
| A37 | Apple unit families: 3D and spatial mixer, reverb room type, distance attenuation | properties of one unit | Host-only: addressed by raw property, not abstracted | no |
| A38 | `AudioUnitReset` | flush internal state | Covered: `Node::reset` | host |
| A39 | AUv3 surface: `parameterTree`, `channelCapabilities`, `MIDIOutputNames`, `userPresets`, `musicalContextBlock`, `providesUserInterface` | the same as the v2 properties, as objects | Partial: maps onto the v2 rows above; not hosted (Objective-C only, no source here to check) | no |

### 3.5 VST2

Opcodes are `vst-tutti`'s `OpCode` and the `effXxx` names it binds;
`tutti-vst2-host/src/param_properties.rs` records that real plugins decline
most of the optional ones.

| Id | Capability | Declares | Map | Read today |
|---|---|---|---|---|
| S1 | `AEffect` flags: has-editor, can-replacing, can-double-replacing, program chunks, is-synth, no-sound-in-stop | basic abilities | Covered: `Features::EDITOR`, `F64_AUDIO`, state, `PluginRole`; no-sound-in-stop is a tail of none that nothing reads | types |
| S2 | `AEffect` fields: program count, parameter count, input and output counts, initial delay, unique id, version | sizes and latency | Covered: `LoadedPlugin`, `Latency` | types |
| S3 | `effGetParamName`, `effGetParamLabel`, `effGetParamDisplay`, `effString2Parameter`, `effCanBeAutomated` | name, a unit-like label, display text, text input | Partial: the label becomes `ParameterInfo.unit` verbatim, and is typed only over a plain range from `effGetParameterProperties` (step 1, built) | types |
| S4 | `effGetParameterProperties` (`VstParameterProperties`: step sizes, switch, integer or float steps, integer min and max, short label, display index, category and its label) | range, steps, grouping | Covered: `ParamSteps`, `ParamRange`, `group`; real plugins decline | host |
| S5 | `effGetProgram`, `effSetProgram`, program names, `effBeginSetProgram`, `effEndSetProgram` | programs | Covered: `PresetId::Number`, `Features::PRESET_LIST` | types |
| S6 | `effGetChunk`, `effSetChunk` (bank or program) | state | Covered: required state save and restore | types |
| S7 | `effGetTailSize` (0 and 1 have special meanings by convention) | ring-out | Partial: as C37; 0 cannot tell "none" from "not implemented" | types |
| S8 | `effGetPlugCategory` | category | Covered: `Vst2Category` | types |
| S9 | `effGetEffectName`, `effGetVendorString`, `effGetProductString`, vendor version | identity | Covered: `PluginDescriptor` | types |
| S10 | `effGetInputProperties`, `effGetOutputProperties` (`VstPinProperties`: label, active, stereo, speaker arrangement) | pins | Covered: `LoadedPlugin` buses | types |
| S11 | `effSetSpeakerArrangement`, `effGetSpeakerArrangement` | speaker placement, proposed and read | Gap G1: `LayoutOptions` (placement is not reported, so `LayoutSupport::None`) | host |
| S12 | `effSetSampleRate`, `effSetBlockSize`, `effMainsChanged`, `effStartProcess`, `effStopProcess`, `effSetProcessPrecision` | lifecycle | Covered: `Loaded::activate`, `Prepare` | types |
| S13 | `effSetBypass` | soft bypass | Covered: `UnitParam::Bypass` | host |
| S14 | `effProcessEvents`: MIDI event (delta frames, note length, note offset, detune in cents, note-off velocity) and sysex | events | Partial: `Ump`, `tutti_ump::SysexRef`; detune (per-note tuning) and note length are not carried | types |
| S15 | `effGetNumMidiInputChannels`, `effGetNumMidiOutputChannels` | how many MIDI channels it uses | Partial: `Features::MIDI_IN/OUT`; the counts are not stored | host |
| S16 | `effGetMidiProgramName`, `effGetCurrentMidiProgram`, `effGetMidiProgramCategory`, `effHasMidiProgramsChanged` | programs tied to bank and program-change numbers | Partial: `Preset` has a name, an id and a bank name; the MIDI bank and program numbers and the category are dropped (G3) | host |
| S17 | `effGetMidiKeyName` | a name for a key | Gap G4: `NoteNames` | host |
| S18 | `canDo` "receiveVstEvents", "receiveVstMidiEvent", "receiveVstSysexEvent", "sendVstEvents", "sendVstMidiEvent" | MIDI in and out | Covered: `Features::MIDI_IN/OUT` | types |
| S19 | `canDo` "receiveVstTimeInfo", "sendVstTimeInfo" | wants time | Covered: `Features::TRANSPORT` | types |
| S20 | `canDo` "offline", "noRealTime" | works offline; cannot run in real time | Partial: `RenderMode` for the first; "noRealTime" has no reader (G5) | host |
| S21 | `canDo` "1in1out", "2in2out", ... "Nin Mout" | accepted channel counts | Gap G1: `LayoutOptions` | no |
| S22 | `canDo` "midiProgramNames", "midiKeyBasedInstrumentControl", "midiSingleNoteTuningChange" | which MIDI metadata and per-key control it understands | Gap G2: `ExpressionInfo` (per-key control, per-note tuning); names are G4 | host |
| S23 | `canDo` "plugAsChannelInsert", "plugAsSend", "mixDryWet" | use as insert or send; has a dry-wet mix | Partial: `PluginRole`; "mixDryWet" would be `UnitParam::Wet` if a parameter said which | no |
| S24 | `canDo` "bypass", "sizeWindow", "supplyIdle", "openCloseAnyThread", "shellCategory", "asyncProcessing", "hasCockosExtensions" | editor and threading behaviours | Host-only: `tutti-vst2-host` | host |
| S25 | `VstTimeInfo`: sample position, rate, nanoseconds, quarter-note position, tempo, bar start, cycle bounds, time signature, SMPTE offset and rate, samples to next clock, flags (transport changed, playing, cycle, recording, automation writing and reading, and valid-masks) | time | Partial: `TransportInfo`; SMPTE and clock are not carried | types |
| S26 | host callbacks: I/O changed, size window, input and output latency, current process level (realtime, user, prefetch, offline), automation state, get time, process events, begin and end edit | what the plugin asks of the host | Partial: I/O changed is the `Capabilities` re-read (§5.1); input and output latency are G12; process level is `RenderMode`; automation state is `AutomationMode` | host |
| S27 | `effEditGetRect`, open, close, idle, key, wheel, `effSetEditKnobMode` | the editor | Host-only: `tutti-vst2-host` `editor.rs` | host |
| S28 | `effShellGetNextPlugin` | a shell plugin lists its children | Covered: `FileState::Probed(Vec<..>)`; `tutti-vst2-host` `shell.rs` | host |
| S29 | `effSetPanLaw`, `effGetVu`, `effVendorSpecific`, legacy offline opcodes, `effSetTotalSampleToProcess`, bank load notices | assorted legacy | Host-only: not read | no |

### 3.6 Tally

| Format | Capabilities | Covered | Partial | Gap | Host-only | Read: types | host | ffi | no |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| CLAP | 61 | 15 | 14 | 17 | 15 | 22 | 29 | 2 | 8 |
| VST3 | 59 | 11 | 17 | 16 | 15 | 25 | 19 | 6 | 9 |
| AU | 39 | 12 | 13 | 9 | 5 | 11 | 22 | 0 | 6 |
| VST2 | 29 | 13 | 9 | 4 | 3 | 14 | 12 | 0 | 3 |
| **All** | **188** | **51** | **53** | **46** | **38** | **72** | **82** | **8** | **26** |

What the tally says.

- **About a quarter is covered and a quarter is partial.** The partial rows are
  the work of section 5: the value exists in `tutti-plugin-types`, and the
  typing, the link to the graph or the corpus is missing.
- **Of the 46 gap rows, 32 are read by a format host and stop there**, one is
  answered only in an `-ffi` crate, and 13 are read by nothing (three of them
  are the CLAP extensions that are not in `clap-sys` 0.5.0). The parsing is
  mostly done; the vocabulary above it is not.
- **CLAP is the richest format and VST2 the poorest, and they agree on the core.**
  Every format carries layouts, a note channel richer than MIDI 1.0, presets and
  a parameter list with a unit or a unit-like label.
- **Eight rows are answered only in the `-ffi` crates** (the VST3 component
  handler family, CLAP's event registry and thread check). They are correct
  host plumbing and need nothing here.

## 4. The oracle: what tutti has no abstraction for

The 46 gap rows fall into 19 candidate abstractions. They are ranked by how many
of the four formats carry the concept (consensus is the evidence it is real; a
concept one format invented may be that format's habit) and, on a tie, by what
tutti would use it for. The last columns are the test of §2.4: **would a tutti
node declare it?** If so it is an engine abstraction, a value in `tutti-types` or
in the crate that owns the matching pure model. If not, it is hosting vocabulary
and stays in `tutti-plugin-types`.

| Rank | Group | Formats | What a tutti node would declare | Engine abstraction? | Home |
|---:|---|:--:|---|:--:|---|
| 1 | **G1 `LayoutOptions`**: the set of channel layouts a processor accepts | 4 (CLAP configs and configurable ports; VST3 bus arrangements; AU `SupportedNumChannels`; VST2 speaker arrangement and the `NinMout` strings) | A panner, a `Fold`, a convolver and the SoundFont player each accept a set, not one `ChannelLayout` | **Yes** | `tutti-types`; `Shape` keeps one current layout |
| 2 | **G2 `ExpressionInfo`**: per-note and per-key controls, their range and behaviour, the MPE declaration | 4 (CLAP expression events and per-note parameter flags; VST3 `INoteExpressionController` and the physical-UI map; AU note scope and `SupportsMPE`; VST2 detune and the key-based `canDo`s) | `PolySynth` already takes per-note pitch and pressure and a bend range; it can state what it understands, and a sequencer could ask before it sends | **Yes** | `tutti-types` (the description); events stay `Ump` |
| 3 | **G3 `PresetCatalog`**: presets as a typed library (factory, user, demo; creator, category, tags) | 4 (CLAP preset discovery; VST3 program lists and `.vstpreset` metadata; AU factory and user presets; VST2 programs and banks) | The SoundFont player has banks and presets (`tutti-sf2`); the sampler has patches | **Yes** | `tutti-types` (`PresetMeta`); `PresetId` stays the hosted address |
| 4 | **G4 `NoteNames`**: a name for a key, a drum map, named keyswitches | 3 (CLAP note-name; VST3 keyswitch controller and program pitch names; VST2 `effGetMidiKeyName`) | A General MIDI drum kit names its keys; a SoundFont preset has keyswitch zones | **Yes** | `tutti-types` |
| 5 | **G6 `MidiMap`**: controller to parameter assignment, and learn | 3 (VST3 `IMidiMapping`, `IMidiMapping2`, `IMidiLearn`; AU `MIDIControlMapping`; CLAP param-indication is the reverse direction); VST2 has none, by spec | The GM instruments map CC 7, 10 and 11 and the bend range to fixed parameters; today that is code, not data | **Yes** | `tutti-types` (the map); binding stays with the MIDI runtime |
| 6 | **G9 `Readout`**: a value a processor reports back (gain reduction) | 3 (CLAP gain-adjustment-metering, spec; VST3 `Comp:GainReduction` function names; AU `MeterReadOnly`); VST2 `effGetVu` returns a level only | A compressor and a limiter have a gain reduction; the plan meter (`Editor::open_meter`) is the level half. See 024 §7: 024 owns the native producer and `Describe`; 023 consumes them | **Yes** | vocabulary in `tutti-types`; the port in `tutti-graph`, like the tail report |
| 7 | **G11 `Ambisonic`**: order, ordering, normalization | 3 (CLAP ambisonic; VST3 speaker arrangements for orders 1 to 7; AU HOA channel-layout tags) | `tutti-spatial` could encode and decode; no topology can say "first order, ACN, SN3D" | **Yes** | `tutti-types` |
| 8 | **G7 `ParamPages`**: curated pages of parameters | 3 (CLAP remote controls; VST3 XML representation; AU `ParametersForOverview`) | A native node could order its `UnitParam`s in pages | Weak: the order is the node's `with_params` order | `tutti-plugin-types` |
| 9 | **G12 `PresentationLatency`**: extra delay for what the user hears, apart from what the signal path needs | 3 (VST3 `IAudioPresentationLatency`; AU `PresentationLatency`; VST2 input and output latency callbacks) | The engine knows the device latency; a node that monitors would read it | Small: a host fact more than a declaration | `tutti-plugin-types` |
| 10 | **G10 `TrackContext`**: track name, color, kind, project location | 3 (CLAP track-info and project-location; VST3 channel context; AU `ContextName`) | A node has no track | **No** | `tutti-plugin-types`; the application fills it |
| 11 | **G8 `CurveResponse`**: the magnitude response or transfer curve, queried | 2 (CLAP mini-curve-display, spec; AU `FrequencyResponse`) | **The strongest tutti use of any gap.** An `Svf`, an EQ band and a compressor knee have exact analytic responses in `tutti-dsp` (doc 022 makes the same argument for a reverb's `rt60`). See 024 §7: 024 owns the native producer and `Describe`; 023 consumes them | **Yes** | a query value in `tutti-types`; evaluation stays in `tutti-dsp` |
| 12 | **G5 `Quality`**: a graded render quality, more than realtime or offline | 2 (AU `RenderQuality` is graded; VST2 `noRealTime`); the others carry the binary mode, already `RenderMode` | The stretch unit's modes, the convolver's partitioning and the export resampler's kernel length are quality choices | **Yes** | `tutti-types`, beside `RenderMode` |
| 13 | **G14 `PrefetchSupport`**: may be run ahead of time | 2 (VST3 `IPrefetchableSupport`; the VST2 prefetch process level) | The sampler's butler prefetches its own streams | Small | `tutti-plugin-types` |
| 14 | **G13 `BusActivation` and `BusRouting`**: turn a port off; which input feeds which output | 2 (CLAP audio-ports-activation; VST3 bus activation and routing info) | An unused sidechain costs a node little (the executor skips silence); routing would refine PDC for a multi-input node | Weak | `tutti-plugin-types` |
| 15 | **G16 `IdentityAlias`**: ids a plugin replaces, and a state converter | 2 (CLAP plugin-state-converter; VST3 `IPluginCompatibility`) | None | **No** | `tutti-plugin-db`, a row field |
| 16 | **G19 `DeclaredVoices` and `VoiceEnded`**: capacity, overlap policy, a voice finishing | 1 (CLAP voice-info and note-end) | `tutti-voicing` already models all of it (`Rules`, the voice table); the only gap where the engine abstraction exists and the hosted side is missing | **Already exists** | `tutti-plugin-types` fills a `Rules` (§5.5) |
| 17 | **G17 `ParamKind::Trigger`**: a stateless trigger parameter | 1 (CLAP triggers; the other formats use a momentary boolean) | A one-shot (reset a delay line, fire a sample) | Maybe | `tutti-plugin-types` |
| 18 | **G18 `DynamicPorts`**: add a port to a running plugin | 1 (CLAP extensible audio ports) | Nothing: `Shape` is fixed at `prepare` | **No** | not planned |
| 19 | **G15 `UndoSteps`**: the plugin's undo history | 1 (CLAP undo) | None; an application concern | **No** | `tutti-clap-host`, as today |

Reading the ranking.

- **The top four are what the consensus says an audio processor is**, beyond
  audio in and audio out: it accepts some layouts, it understands notes with
  more than pitch and velocity, it ships with sounds, and it names its keys.
  tutti has the engine half of each in pieces (`Shape` and `Fold`, `Ump` and
  `PolySynth`, `tutti-sf2` and the sampler, the GM drum map). It has no shared
  *declared* value.
- **G8 and G9 are where the oracle points at tutti's own nodes.** A compressor
  that cannot say how much it is reducing, and an EQ that cannot say what its
  curve is, are missing declarations. The formats confirm the shape, and the
  node side is cheaper than the plugin side, because a native node can compute
  the answer exactly.
- **G10, G15, G16 and G18 are not engine abstractions.** They are carried as
  plain data or not at all (§5.8).
- **G1 is first although it is not on the likely list**, because it is the only
  gap that is *negotiated*, not only declared: the host proposes and the plugin
  refuses, and tutti has no value to propose with.

## 5. The prototype

Sketches, not code in crates. They name real types; a new type is marked. Each
is the smallest value that carries the capability; none adds a trait to the
audio path, and none changes `Node`.

### 5.1 `Capabilities`

One value per plugin instance, built on the control side by `Loaded` before
activation, serializable (it rides the load reply beside `LoadedPlugin`, which
it absorbs field by field as the steps land), and replaced whole on a
restart-style notice (V43, S26 "I/O changed", C17, C36, C37).

```rust
/// A value a plugin may or may not have said anything about. The sum type keeps
/// "not asked" apart from "asked, and there is none", as `FeatureReport` does.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Reported<T> {
    #[default]
    Unasked,
    Absent,
    Is(T),
}

/// What a plugin says about itself, in tutti's vocabulary.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Capabilities {
    /// As today: the per-block send gate and what was probed.
    pub features: FeatureReport,
    pub processing: Processing,
    pub layouts: Reported<LayoutOptions>,
    pub params: Vec<ParameterInfo>,
    pub notes: NoteCaps,
    pub presets: Reported<PresetCatalog>,
    pub display: Display,
    pub context: ContextNeeds,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Processing {
    pub latency: Latency,
    pub tail: Tail,
    /// What a probe measured, kept apart from the claim above (section 6).
    pub measured: Option<Measured>,
    pub render: RenderSupport,
    pub sample_widths: SampleWidths,
    pub in_place: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NoteCaps {
    pub dialects: Dialects,                    // C20, A22
    pub voices: Reported<DeclaredVoices>,      // C42, 5.5
    pub expressions: Vec<ExpressionInfo>,      // G2, 5.8
    pub names: Reported<NoteNames>,            // G4
    pub tuning: Reported<TuningSupport>,       // 5.6
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Display {
    pub pages: Reported<ParamPages>,           // G7
    pub readouts: Vec<Readout>,                // G9
    pub curve: bool,                           // G8: asking is a call, not a stored value
}
```

- **`Features` stays.** It is the best-effort send gate and must stay one
  mask-and-compare (`Features::CONSUMES`). `Capabilities` contains its
  `FeatureReport`; nothing is removed from it.
- **`LoadedPlugin` is the sub-value the graph wires from** (buses, latency,
  tail, topology). Its fields move into `Processing` and `LayoutOptions` in the
  last step, after their readers have.
- **A re-read is the `Lifecycle`'s existing rescan `Effect`**, with the VST3
  restart flags (V43) as the list of which fields to refresh. A plugin that
  changes its latency or tail at run time replaces `processing`, and the node
  re-declares through `Editor::set_latency` as it does today.
- **`PluginDb` stores a subset**: the identity it already stores, plus the
  `Measured` of section 6, because those survive between sessions. Everything
  else needs the plugin instantiated and is read at load.

### 5.2 `ParameterInfo` gains a typed unit and a kind

```rust
/// What a parameter's number measures, from a closed set. New.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParamUnit {
    Hertz, Decibels, Seconds, Milliseconds, Percent, Cents, Semitones,
    Octaves, Bpm, Beats, Degrees, Pan, LinearGain, PlaybackRate,
    MidiNote, MidiController,
    /// A choice: the value is an index into named options.
    Choice,
    Toggle,
}

pub struct ParameterInfo {
    // ... every field as today, including `unit: String` (the display text)
    /// The typed unit, only where the format declared it exactly. `None` is
    /// the common case, and the flat `unit` string is still there.
    pub measures: Option<ParamUnit>,
    /// The tutti role this parameter plays, only where the format declared the
    /// role itself (never from a name).
    pub role: Option<UnitParam>,
}

impl ParameterInfo {
    /// A plain value as a tutti unit, if the declared unit and the declared
    /// plain range allow it. `None` for a normalized-only parameter.
    pub fn typed<U: FromPlain>(&self, plain: f64) -> Option<U>;
}
```

`measures` and `role` are derived by one pure function per format host, so the
mapping is a table a test can hold:

| Source | Rule | Result |
|---|---|---|
| AU unit code | The code is a closed enumeration. Hertz, decibels, seconds, milliseconds, percent, cents, relative semitones, octaves, BPM, beats, degrees, pan, linear gain, rate, MIDI note, MIDI controller, indexed and boolean map to the same-named `ParamUnit` (`Indexed` is `Choice`, `Boolean` is `Toggle`). Generic, custom, equal-power crossfade, mixer fader curve, absolute cents, meters, phase, ratio and sample frames map to `None`: their algebra is not one of ours, or the code does not say enough | `Some` for 18 of the 27 codes (checked against Apple's header on 2026-11-11, see "As built, step 1") |
| AU pan code | also `role = UnitParam::Pan` | |
| VST3 `units` | A **closed list of exact, case-sensitive matches** after trimming ASCII space: `Hz`, `dB`, `s`, `ms`, `%`, `ct`, `st`, `BPM`. Anything else (`kHz`, `Db`, `dB(A)`, `semitones`, the empty string) is `None`. The list grows only by a corpus run (section 6) | `Some` for eight strings |
| VST3 flags | the list flag with a step count gives `Choice`; a step count of 1 gives `Toggle` | |
| VST3 `IParameterFunctionName` | `DryWetMix` gives `role = UnitParam::Wet`. `Comp:GainReduction` is not a role: it is a `Readout` (G9) | |
| VST2 label | The same closed list as VST3, **and** only when `effGetParameterProperties` answered with a plain range, because VST2's native value is normalized and a unit without a range cannot be converted. Real plugins decline it (`param_properties.rs`), so this is rarely `Some` | |
| CLAP | `clap.params` has no unit. `measures` is `None`. The is-enum flag gives `Choice` | `None` |
| Any format, bypass flag | `role = UnitParam::Bypass` | |

Rules that keep it honest.

- **A unit is accepted only with a range that fits it.** `Hertz` needs a plain
  range with a finite non-negative minimum, `Percent` a range inside `-100..=100`,
  `Pan` one inside `-1..=1`. A parameter labelled `s` that runs `0..1000` is not
  treated as seconds.
- **Percent and milliseconds are scaled once, in `typed`**, not in the value:
  `typed::<Mix>` divides a percent by 100, `typed::<Seconds>` divides
  milliseconds by 1000, and the plugin's own number is what is sent back.
- **The role set is small on purpose.** Only `Bypass`, `Wet` and `Pan` have an
  exact source. Cutoff, resonance, attack and the rest have none in any format
  and are not inferred from a title.
- **It is the `known` mask's shape one level up**: `ParamFlags` pairs flags with
  a `known` mask, and `measures` is an `Option` for the same reason.

**As built, step 1 (parameter unit and kind).**

- **The API** is in `tutti-plugin-types`: `ParamUnit` (`#[non_exhaustive]`, the
  eighteen variants above), `ParamUnit::from_label` (the closed VST3/VST2 list,
  one copy), `ParamUnit::fits` (the plausibility rule), the trait `FromPlain`
  ("reads a tutti unit out of a plain value measured in a declared `ParamUnit`"),
  `ParameterInfo::{measures, role, with_measures, with_role, typed}`. `with_measures`
  applies `fits`, so a host cannot set a unit its range does not fit. `FromPlain`
  is implemented for `Hz`, `Db`, `Seconds` (seconds and milliseconds), `Mix`
  (percent, `0..=100` only), `Pan`, `Cents`, `Semitones`, `Bpm`, `BeatDuration`,
  `ArcDegrees`, `Amplitude` and `PlaybackRate`; octaves and the MIDI numbers have
  no tutti unit, so `typed` is `None` for them.
- **Range rules chosen** (`ParamUnit::fits`; every unit but `Choice` and `Toggle`
  needs a plain range with finite bounds, `min <= max`): `Hertz`, `Seconds`,
  `Milliseconds`, `Beats`, `LinearGain` and `PlaybackRate` need `min >= 0`; `Bpm`
  `min > 0` (a `Bpm` is positive); `Percent` inside `-100..=100`; `Pan` inside
  `-1..=1`; `MidiNote` and `MidiController` inside `0..=127`; `Decibels`, `Cents`,
  `Semitones`, `Octaves` and `Degrees` any finite range. `Choice` and `Toggle`
  fit any range, a normalized one included, because VST3 declares nothing else.
- **VST3 flags**: a list flag with a step count of at least 1 is `Choice` (a
  two-entry list names its options, so it beats `Toggle`); otherwise a step count
  of 1 is `Toggle`; otherwise the `units` string. `IParameterFunctionName` was
  already read by `Vst3Loaded::param_id_for_function_name`, so `DryWetMix` gives
  `role = Wet`: `parameter_list` asks the root unit and each published unit and
  takes the first answer.
- **Bypass** is a role only where a flag exists: CLAP and VST3. VST2 has no
  parameter flag and AU's `BypassEffect` is a unit property, so neither sets it.
- **AU table, verified.** Source: Apple's `AudioUnitProperties.h`, enum
  `AudioUnitParameterUnit` (27 entries, explicit values 0 to 26), read from a
  public copy of the macOS 10.15 SDK (`phracker/MacOSX-SDKs`) on 2026-11-11;
  Apple's documentation page itself is a JavaScript shell and returned no
  content. The numbers in the "from memory" table were right. Two readings are
  corrected: *Meters* (19) is a **distance** in meters, not a level meter, and
  *Phase* (6) is degrees (-180 to 180), which is why it stays `None` beside
  `Degrees`. Codes: 0 Generic, 1 Indexed, 2 Boolean, 3 Percent, 4 Seconds,
  5 SampleFrames, 6 Phase, 7 Rate, 8 Hertz, 9 Cents, 10 RelativeSemiTones,
  11 MIDINoteNumber, 12 MIDIController, 13 Decibels, 14 LinearGain, 15 Degrees,
  16 EqualPowerCrossfade, 17 MixerFaderCurve1, 18 Pan, 19 Meters, 20 AbsoluteCents,
  21 Octaves, 22 BPM, 23 Beats, 24 Milliseconds, 25 Ratio, 26 CustomUnit.
  `tutti-au-host`'s `unit_code.rs` is not macOS-gated, so the table is tested
  everywhere; a macOS-only test cross-checks every number against
  `coreaudio-sys`.
- **Wire**: `ParameterInfo` crosses the IPC (`ParameterList`,
  `ParameterInfoResponse`), so `PROTOCOL_VERSION` went from 23 to 24 (doc 020
  stage 5b took 23). No recorded golden holds a `ParameterInfo`. `role` serializes as the `UnitParam`'s `u16`
  discriminant (`UnitParam` has no serde of its own); both fields are
  `serde(default)`, so saved JSON without them still reads.

### 5.3 The tail, flowing into `Io::report_tail`

Today the plugin's tail is the `Shape::tail` ceiling, set once from the plugin's
claim (`declared_shape`), and the silence skip parks the node on it. Three
changes, in order.

```rust
/// What a probe measured. New, in `Processing`.
pub struct Measured {
    pub tail: Option<Samples>,      // last sample above the floor, from the impulse
    pub latency: Option<Samples>,   // first sample above the floor
    pub config: ProbeConfig,        // rate, block, the floor in Db, the state hash
}

impl Processing {
    /// The ceiling `Shape::with_tail` gets. A measurement raises a finite claim
    /// and never lowers or invents one.
    pub fn ceiling(&self) -> Tail {
        match (self.tail, self.measured.as_ref().and_then(|m| m.tail)) {
            (Tail::Finite(c), Some(m)) if m > c => Tail::Finite(m),
            (t, _) => t,    // None, Unknown, Unbounded and a finite claim stand
        }
    }
}
```

1. **Ceiling.** `declared_shape` calls `Processing::ceiling()` instead of
   `controls.tail()`. A reported `Unknown` stays `Unknown` and the node is never
   skipped: one impulse at default settings proves nothing about another patch.
2. **Live report, for formats that have a live signal.** CLAP's process status
   says *continue*, *continue if not quiet*, *tail* or *sleep* (`is_sleeping`
   and `is_tailing` are read today and go no further). In the plugin node's
   `process`:

   ```rust
   // after the plugin's process call returns its status
   match status {
       PluginStatus::Sleep => { io.report_tail(Tail::None); return Status::Idle; }
       PluginStatus::Tail  => io.report_tail(self.caps.processing.ceiling()),
       _ => {}                    // reports nothing: the ceiling stands
   }
   ```

   `Status::Idle` is what lets a node with an event input (every instrument) be
   parked; `report_tail` alone does not, by its own contract. VST3, AU and VST2
   have no per-block status, so they report nothing and keep the ceiling. That
   is correct: they cannot narrow.
3. **A changed tail** (CLAP's tail-changed callback, the VST3 restart flags, an
   AU property listener) replaces `Processing`, and the node re-declares its
   ceiling the way a latency change does through `Editor::set_latency`. A tail
   setter on `Editor` is part of step 2 if one does not exist; the executor's
   record is untouched until the next commit, as the ceiling's own docs require.

`contract::assert_honest_tail` is the node-side check (built with `report_tail`) and needs the
node in the test process. A hosted plugin needs the probe (section 6), which runs
the same experiment out of process.

### 5.4 Note dialect

```rust
bitflags! {
    /// What a note port speaks: which it accepts, and (separately) which it prefers.
    pub struct Dialects: u8 { const UMP = 1; const MIDI1 = 2; const MPE = 4; const NOTE_IDS = 8; }
}
```

tutti is UMP-native. The dialect is **what to translate to** at the plugin's
edge, by `tutti-midi-protocol` (`narrow` for `MIDI1`, `midi1` for the 1.0 wire);
it is not a runtime negotiation. `NOTE_IDS` is CLAP's note events (note id, port,
choke, end); VST3 events are `MIDI1 | NOTE_IDS` (VST3 has note ids) and VST2 is
`MIDI1`; AU reads its protocol property (A22). Today the CLAP host chooses with
`dialect_to_send`; the choice becomes a function of `Capabilities`.

### 5.5 A voice description as a `Rules`-shaped value, plus note-end tracking

`tutti_voicing::Rules` has seven fields and a plugin declares at most three. So
the plugin side is a value of **options**, and a function that lays it over a
base `Rules`:

```rust
/// What a plugin says about its voices. Every field is optional because no
/// format says all of it. New.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DeclaredVoices {
    /// CLAP `voice_capacity`: a hard cap, "do not expect more to sound".
    pub capacity: Option<u32>,
    /// CLAP `voice_count`: how many it expects now (live, changes with the patch).
    pub expected: Option<u32>,
    /// CLAP `supports_overlapping_notes`.
    pub overlapping: Option<bool>,
    /// MPE zones, from the dialect or AU `SupportsMPE`.
    pub zones: Option<Zones>,
}

impl DeclaredVoices {
    /// The rules a shadow tracker runs: the plugin's declarations over `base`,
    /// which carries what no plugin declares (mode, steal order, groups, pedal).
    pub fn rules(&self, base: Rules) -> Rules {
        Rules {
            polyphony: self.capacity.map_or(base.polyphony, |n| n as usize),
            retrigger: match self.overlapping {
                Some(true) => Retrigger::Layer,     // a key struck again layers
                Some(false) => Retrigger::Replace,  // end the old one first
                None => base.retrigger,
            },
            ..base
        }
    }
}
```

The `Retrigger` mapping is the CLAP rule the CLAP host already documents: when
overlap is not supported, the host must end the sounding note before starting
another on that key, which is `Replace`. The capacity is a *hard* bound in CLAP,
so the host must not start a voice past it, which is `Steal::By(..)` at
`polyphony = capacity`: the host picks the victim deterministically and sends a
choke, instead of leaving it to a plugin that may click.

**Note-end tracking** is the other half. A `Voicing` run beside the plugin, over
a patch that starts one voice per note, gives the host the answer to "which
notes are down" that it would give for a native instrument:

```rust
struct OneVoice;                                   // a Patch: one voice per strike
impl Patch for OneVoice { /* call `spec` once: group 0, scope 0, tag 0 */ }

struct NoteTracker { voicing: Voicing, views: Vec<VoiceView> /* sorted by id */ }
// voicing.step(channel, &message, &OneVoice, &views) -> &[Command]
//   Start             -> note on, id = VoiceId (NOTE_IDS dialects)
//   Retarget, Release -> note off
//   Cut, Damp         -> choke (CLAP), or note off and all-sound-off on the channel
// the plugin's note-end event removes the VoiceId from `views`
```

What it buys, and what it cannot.

- **Hanging notes**: a stop, a seek or a deactivate asks the tracker which voices
  are held and releases exactly those, instead of an all-notes-off on 16
  channels. This works for all four formats.
- **Overlap policy and the capacity bound**, from `DeclaredVoices`, for CLAP.
- **A voice ending is knowable only in CLAP.** A note-end event removes a voice
  still ringing in its release. For VST3, AU and VST2 a voice leaves the list on
  its release: the tracker knows what is *held*, not what is *sounding*. The type
  says so (the list is built from held voices there), and such a plugin's tail
  stays the ceiling.
- **`Status::Idle` for an instrument** needs both: no tracked voice and the
  plugin's sleep status (5.3). A CLAP instrument can reach it; the others cannot,
  which is why their instruments are never parked today either.

### 5.6 Tuning, cost, render

**Tuning.** `tutti_polysynth::Tuning` is a scale model at layer 5, and
`tutti-plugin-types` is layer 3: it cannot name it. What the formats share is
smaller than a scale: a **per-key offset in cents** (CLAP tuning tables, VST3's
per-note tuning field and tuning expression, VST2 single-note tuning change, the
MIDI Tuning Standard). So the shared value is that, in `tutti-types`, and a scale
converts to it at the point of use:

```rust
/// A cent offset for each MIDI key, from equal temperament. New, `tutti-types`.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyTuning { pub offsets: [Cents; 128] }

impl Tuning {                                  // tutti-polysynth: it has the 128-entry Hz table
    pub fn key_tuning(&self) -> KeyTuning;     // derive, never mirror
}

pub struct TuningSupport { pub tables: bool, pub per_note: bool }  // what the plugin takes
```

**Cost.** No format declares that a plugin is cheap, so a hosted plugin stays
`Cost::Heavy`, the default. The two capabilities that touch cost change nothing:
CLAP `thread-pool` makes the plugin a *user* of the host's pool, so the plugin
must be given the same `WorkerPool` and run under `assert_no_alloc`, as the
parallel executor's rule says (the `clap_thread_pool` gate exists); CLAP
`render`'s hard-realtime flag says the plugin must not run off the audio thread,
which a pool cannot promise (Q5).

**Render.** `RenderSupport { offline: bool, hard_realtime: bool, quality:
Reported<QualityRange> }`; `quality` is G5 and is filled from AU only.

### 5.7 Topology

```rust
/// The layouts a processor accepts. New, `tutti-types`.
pub struct LayoutOptions {
    pub current: Vec<BusLayout>,                 // the LoadedPlugin buses, as today
    pub choices: Vec<LayoutChoice>,              // a listed set
    pub negotiable: bool,                        // the host may propose one and be refused
}
pub struct LayoutChoice { pub name: Option<String>, pub inputs: Vec<BusLayout>, pub outputs: Vec<BusLayout> }
pub enum BusLayout {
    Layout(ChannelLayout),                       // a width
    Topology(ChannelTopology),                   // speakers
    Ambisonic(Ambisonic),                        // G11
    ControlVoltage(u16),                         // the VST3 control-voltage bus
}
pub struct Ambisonic { pub order: u8, pub ordering: AmbiOrdering, pub norm: AmbiNorm }  // (order + 1)^2 channels
```

Sources: the CLAP config list, or the configurable-ports probe; VST3
`setBusArrangements` tried over a short candidate list (mono, stereo, 5.1, 7.1
and the plugin's current); AU `SupportedNumChannels` and the layout tags; VST2
the `NinMout` strings. `Fold` is the consumer: it already builds a fold for any
(from, to) pair, and `Shape` keeps exactly one current layout, as today. The list
matters at the edge (choosing a layout for a plugin whose preferred one the graph
cannot feed) and in a panner or convolver node declaring a set.

### 5.8 One sketch per top-ranked gap

```rust
// G2: per-note expression, from CLAP and VST3 descriptions. Home: tutti-types.
pub struct ExpressionInfo {
    pub kind: NoteExpressionType,              // exists, in tutti-plugin-types
    pub range: Option<(f32, f32)>,             // plain min, max
    pub default: f32,
    pub bipolar: bool,                         // centered at the default
    pub one_shot: bool,                        // once per note, at the attack
    pub absolute: bool,                        // sets the value instead of offsetting it
    pub physical: Option<PhysicalAxis>,        // X, Y, Pressure (VST3), or an MPE role
    pub param: Option<ParamId>,                // the parameter it mirrors, if declared
}

// G3: a typed library. Home: tutti-types for the meta; PresetId stays hosted.
pub struct PresetMeta {
    pub name: String,
    pub origin: PresetOrigin,                  // Factory, User, Demo
    pub creator: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,                     // CLAP features, VST3 style and character
    pub favorite: bool,
}
pub struct PresetCatalog { pub entries: Vec<(PresetId, PresetMeta)>, pub can_enumerate: bool }

// G4: names for keys. Home: tutti-types.
pub struct NoteNames {
    pub keys: Vec<(NoteRange, String)>,        // a drum map: "Kick" on 36
    pub keyswitches: Vec<Keyswitch>,           // a title and the key range that selects it
}

// G6: controller to parameter. Home: tutti-types.
pub struct MidiMap { pub entries: Vec<MidiBinding> }
pub struct MidiBinding { pub channel: Option<MidiChannel>, pub cc: CCNumber, pub param: ParamId }

// G9: what a processor reports back. Vocabulary in tutti-types, the port in tutti-graph.
pub struct Readout { pub kind: ReadoutKind, pub param: Option<ParamId>, pub unit: ParamUnit }
pub enum ReadoutKind { GainReduction, GainReductionMax, Level }

// G8: asked, not stored: a curve is a function of the parameter values and a frequency grid.
pub trait CurveResponse { fn magnitude(&self, freqs: &[Hz], out: &mut [Db]); }
pub enum Curve<'a> { Magnitude(&'a dyn CurveResponse), Transfer(&'a dyn TransferCurve) }  // doc 024 section 7.1: a static curve is dB over dB
```

Three of these have a native producer already: `NoteNames` from a GM drum map and
from `tutti-sf2` preset zones, `PresetMeta` from SoundFont presets, `Readout`
from the dynamics nodes. That is the test of §2.4 applied: write the native
producer first, and the plugin consumer is a mapping.

The non-engine groups are plain data in `tutti-plugin-types`: `ParamPages` (a
list of named lists of `ParamId`), `TrackContext` (name, color, kind, index;
filled by the application and sent through CLAP's setter, VST3's `IInfoListener`
and AU's `ContextName`), `PresentationLatency(Samples)`, `PrefetchSupport`, and
`IdentityAlias(Vec<PluginRef>)` in `PluginDb`. Undo and dynamic ports stay in
`tutti-clap-host`.

## 6. Probes

Latency and tail are claims. The probe binary of doc 021 §2.5
(`tutti-plugin-probe`, which loads a plugin, writes a result file and exits)
already isolates the first load. This design adds a second level: **instantiate,
activate, measure.**

- **Where**: `plugin-probe <path> <result-file> --measure`, after the descriptor
  is read. A plugin that crashes while rendering crashes the probe, not the
  host, and the blacklist and crash-history logic in `tutti-plugin-db` sees the
  same `ProbeFailure` cases. A hang at the timeout is the same outcome as a hang
  in load.
- **What it measures**, at 48 kHz with 512 frames and again at 44.1 kHz with 64
  (the figure depends on the configuration, so `Measured` records it):
  - **Tail**: an impulse on every input channel at frame 0, silence after,
    rendered until the output stays below a floor for a long window; the result
    is the last frame above the floor. This is `assert_honest_tail`'s
    experiment, run on a hosted plugin. The floor is not exact silence, because
    a plugin's own dither or denoise noise is not a tail: it is `Db(-100)`, a
    named constant recorded in `ProbeConfig`.
  - **Latency**: the first frame above the floor, compared with the reported
    latency. For a plain delay it equals it. For any other plugin it is only the
    *onset*, so a disagreement is recorded, never a correction.
  - **Instruments** have no audio input: the stimulus is a note-on and, after
    100 ms, a note-off, and the tail is measured from the note-off to the floor.
    The impulse test is wrong for them.
  - **Nonlinear plugins** (a limiter, a saturator) answer an impulse of 1.0
    differently from program material. The probe uses 0.1, and the result is
    a lower bound.
- **The verdict**: `Honest`, `TailLonger { reported, measured }`,
  `LatencyDiffers { reported, measured }`, `NeverSettles { level }` or
  `NotChecked { why }` (an unbounded or unknown claim, or an instrument that
  never reached the floor within the budget).
  - **`NeverSettles`** is a plugin whose output stays above the floor with
    silence in: modelled hiss, a noise generator, a free-running oscillator.
    The probe renders the same configuration with no stimulus first. If that
    output is above the floor, the verdict is `NeverSettles` with the level it
    held, and the tail is not judged.
    Without this, such a plugin reads as `TailLonger`, or runs the probe to its
    budget, and a plugin that adds noise looks like one that lies about its
    tail. The floor stays fixed (Q3); this verdict is how a plugin that cannot
    meet it is told apart from a dishonest one. `Processing::ceiling` (5.3) uses a longer measurement; the verdict is
  shown to the user, and nothing is rewritten.
- **Where it is recorded**: `PluginDescriptor` gains `measured:
  Option<Measured>` (`serde(default)`) in the row `PluginDb` already keeps
  (`FileState::Probed`). The row is stamped with the file's modification time,
  so a reinstall or an update drops the measurement with the rest of the row, as
  it drops a blacklist today. No new table.
- **What it cannot do**: a measurement is of one patch at default settings. A
  plugin whose tail follows a parameter rings differently at another setting; the
  probe records the figure for the state it loaded (`ProbeConfig` carries a hash
  of it), and therefore only ever raises a ceiling.
- **A corpus run is the other use of the probe.** The closed unit-string list of
  5.2 grows only by a run over installed plugins that counts the `units` strings
  it sees, in the manner of the measurement recorded in `param_properties.rs`.

## 7. The reverse direction (parked)

The inventory reads the other way too. A tutti node exported as a plugin must
*declare* every row above that a native node can state: channels (G1 and
`ChannelTopology` become the bus arrangement), latency and tail (`Shape`,
including the live report), parameters with units and `UnitParam` kinds (the
inverse of 5.2, and exact: a native node can fill the VST3 function name and the
AU unit code that a host could never infer), note expressions (G2), note names
and keyswitches (G4, for a drum kit or a SoundFont), presets (G3), tuning, the
render quality and the offline flag, and the gain-reduction readout (G9). The
mapping is one to one per row, and the abstractions of section 4 are what make it
a mapping instead of per-format code.

This is **parked**. No export of a node as a plugin is planned, and the plugin
format crates are hosts, not authors (the exception is the test plugin under
`testing/`, whose format-free `core` already has to declare several of these).
The point of recording it is that every abstraction in section 4 has a native
declarer, so the reverse direction needs no new vocabulary when it arrives.

## 8. ARA

ARA (Audio Random Access) is the standard for a plugin that needs the *whole
clip* beside real-time playback: Melodyne-style pitch and time editing,
stretching and analysis. It adds a document-shaped API to a host (audio sources,
regions, modifications, playback regions with a time-stretch map) and a matching
extension to the plugin. It is out of scope: it describes a host's *editing
model*, not what a processor declares, and tutti has no clip or region model for
it to attach to. If it lands, it attaches at two places: the sampler's stretch
unit (a playback region's stretch map is the same concern) and the offline
timeline in `tutti-transport` (regions). A hosted ARA plugin would then be a
`Loaded` with an extra `Controller` surface, not a row in this inventory.

## 9. Steps

Ordered; each is one reviewable PR that leaves `cargo xtask ci` green. They run after
doc 021's structure steps 1 to 8, except where a step names a narrower
dependency: step 3 needs only 021's step 4c (the probe binary), step 2 needs
`Io::report_tail`, which is built (PR #359).

| # | Step | Needs | Test and mutation |
|---|---|---|---|
| 1 | **Parameter unit and kind.** `ParamUnit`, `ParameterInfo::measures` and `role` (`serde(default)`), the pure mapping per format host, the plausibility check, `typed::<U>`; the AU host decodes all 27 codes | nothing | every code, every accepted string, every near-miss (`kHz`, `Db`, empty), a range that does not fit. Mutation: break one table row, its test fails |
| 2 | **The tail into the graph.** `Processing::ceiling`, `Measured` (empty until step 3), CLAP's process status to `report_tail` and `Status::Idle` in the plugin node, a tail setter on the editor if none exists | PR #359 | a fake CLAP processor that sleeps. Mutation: report `Sleep` while sounding, the honest-tail contract fails |
| 3 | **Tail and latency probes.** `--measure` in `tutti-plugin-probe`, `Measured` in `PluginDescriptor`, the verdict; the shared test plugin's mode word sets its tail | 021 step 4c | the plugin reports N and rings M: `Honest` when N = M, `TailLonger` when N < M; a plugin that adds noise above the floor with silence in is `NeverSettles`, not `TailLonger`. Mutations: report M - 1, the verdict changes; skip the silence-in render, the noisy plugin reads `TailLonger` |
| 4 | **The voice description.** `DeclaredVoices`, `rules`, the CLAP fill, `NoteTracker` over `OneVoice`, the note-end event, hanging-note cleanup on stop and seek | step 2 | the tracker is tested against `tutti-voicing`'s own spec tests, no other implementation; a hanging-note case per format. Mutation: drop the note-end handler, a voice stays |
| 5 | **`Capabilities` and `Reported`.** Assemble it in `Loaded`, carry it with `LoadedPlugin`, replace it on a restart notice; `Dialects` | step 4 | a re-read test through the 016 `step` core. Mutation: clear the flag-to-field mapping, the test fails |
| 6 | **`LayoutOptions` and `Ambisonic`** (G1, G11). A native node declares a set first; then the four plugin sources | step 5 | a `Fold` property test over every `LayoutChoice` pair; a refused proposal changes nothing in the graph |
| 7 | **Note expression, note names, keyswitches** (G2, G4). Native producers first (the GM drum map, `PolySynth`'s per-note controls), then CLAP, VST3, VST2 | step 5 | a description round trip per format against the shared test plugin |
| 8 | **Presets** (G3). `PresetMeta` from SoundFont presets, then the four plugin sources; CLAP preset discovery needs new FFI in `tutti-clap-ffi` | step 5 | enumeration per format |
| 9 | **Readouts, MIDI map, curve** (G9, G6, G8). The dynamics nodes and the EQ declare first. See 024 §7: 024 owns the native producer and `Describe`; 023 consumes them | step 5 | an analytic curve against the node's measured response, one test per node |
| 10 | **The rest**, each in the PR that needs it: `PresentationLatency`, `ParamPages`, `TrackContext`, `Quality`, `PrefetchSupport`, `IdentityAlias`; then `LoadedPlugin` folds into `Capabilities` and the capability table in the `tutti-plugin` README is regenerated from the inventory | steps 5 to 9 | |

Steps 1 to 3 are the commitment. Step 4 follows because the voicing core is
merged and CLAP's declaration is already read. Steps 6 to 10 are the oracle's
output, each worth doing only with a native producer in the same PR.

## 10. Decisions

Decided by the owner on 2026-11-10, as recommended. Q3 gained the
`NeverSettles` verdict (§6) and Q7 is scoped to a first reader.

| # | Question | Decision |
|---|---|---|
| Q1 | Where does a native node declare G1, G2, G3, G4, G6, G8, G9: on `Node`, in `Shape`, elsewhere? | Neither. `Shape` stays the minimum the compiler needs and `Node` is unchanged. An optional `Describe` trait, implemented by a node and read by `tutti-session` beside `SessionNode::params`, returns the values of 5.8. It is not on the audio path |
| Q2 | Is the closed unit-string list (5.2) acceptable, or should VST3 and VST2 strings never be typed? | Keep the list short and exact, and grow it from the corpus run of section 6. The cost of a wrong accept is a mislabelled parameter; the cost of never accepting is the status quo |
| Q3 | The probe floor: `Db(-100)` fixed, or per plugin? | Fixed, named, recorded in `ProbeConfig`. A per-plugin floor is a knob that hides a dishonest tail. A plugin that stays above the floor with silence in is `NeverSettles` (§6), not `TailLonger` |
| Q4 | Should the host enforce `voice_capacity` by stealing (5.5)? | Yes for CLAP, whose spec makes it a hard bound; no for a format that does not declare it. Ship the data (step 4) before the stealing |
| Q5 | CLAP's hard-realtime flag: pin such a plugin's node to the audio thread? | Not now. `Shape` has no placement, and adding one for one flag is the indirection the project rules warn against. Record and show the flag; revisit when a plugin sets it |
| Q6 | Does `Capabilities` replace `Features` and `LoadedPlugin`? | `Features` stays: it is the per-block gate. `LoadedPlugin` folds in at step 10, after its readers have moved |
| Q7 | Are `ParamPages` and `TrackContext` worth a type, given no tutti node declares them? | Yes, as plain data in `tutti-plugin-types`, so an application does not parse three formats. Not in `tutti-types`. Each is built in the PR that has its first reader (§9 step 10), never ahead of one |
| Q8 | Do `Tuning` and `KeyTuning` belong in `tutti-types`? | `KeyTuning` (cents per key) yes; `Tuning` (the scale) stays in `tutti-polysynth` and converts |
| Q9 | Is `ParamUnit` a hosting tag in `tutti-plugin-types`, or a runtime tag in `tutti-types` beside the unit types? | `tutti-plugin-types`. The unit types are compile-time; a runtime tag is a hosting concern, and adding one to `tutti-types` risks two names for one behaviour |
| Q10 | Host the AUv3 surface (A39)? | Not in this design. The v2 properties cover what `tutti-au-host` does; AUv3 is a separate hosting decision |
