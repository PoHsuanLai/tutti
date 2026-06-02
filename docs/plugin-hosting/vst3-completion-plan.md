# VST3 hosting completion plan

**Goal:** take tutti's VST3 host from "solid single-bus" to "hosts VST3 properly."
The single-bus core (params, sample-accurate automation, MIDI, editor, state,
transport, IConnectionPoint) is real and RT-safe. Four gaps stand between it and
correct hosting of the full plugin population.

Repo: `/Users/pohsuanlai/Documents/dawAI/dawai` (tutti sub-workspace at
`crates/tutti/`; run cargo from there). Crates: `tutti-vst3-host`,
`tutti-plugin-server` (subprocess driver `src/loaders/vst3.rs`), `tutti-plugin`
(GUI bridge `src/bridge/gui/vst3.rs`).

Testing policy (from CLAUDE.md): never weaken tests; fix root causes; add tests
for new behavior. Keep the RT path allocation-free — there are `assert_no_alloc`
tests in `tutti-vst3-host` (`com/param_changes.rs`, `com/param_queue.rs`,
`com/event_list.rs`, and `tests/vst3_process_no_alloc.rs`); they must stay green.

Reference (read-only, do NOT copy code — AGPL): JUCE's VST3 host at
`/Users/pohsuanlai/Documents/dawAI/dawai-refs/JUCE/modules/juce_audio_processors_headless/format_types/juce_VST3PluginFormatImpl.h`
and `juce_VST3Common.h`. Use it to understand the *VST3 contract* (which is public
Steinberg spec), not as source to lift.

---

## Work items (priority order)

### V1 — [THE BIG ONE] Multi-bus processing + setBusArrangements
**Problem.** `tutti-vst3-host/src/host/instance.rs:309-310` hardcodes
`numInputs: 1, numOutputs: 1` in `ProcessData`, and `make_audio_bus` builds a
single bus from `buffer.inputs`/`buffer.outputs` (`instance.rs:44-52, 282-283`).
Bus enumeration helper `get_bus_channel_count` (`loaded.rs:61`) only ever reads
bus 0. **Consequence:** any plugin with >1 audio bus (sidechain compressor,
multi-out instrument, surround, aux returns) gets silence/garbage on buses ≥1.
Sidechain plugins are unusable.

**Fix (scope carefully — this is L effort, land incrementally):**
1. **Enumerate all buses.** Generalize `get_bus_channel_count` / add a helper that
   returns per-bus channel counts for every input and output bus
   (`getBusCount(K_AUDIO, dir)` then `getBusInfo` per index). Store the full bus
   layout on `Vst3Loaded`/`Vst3Instance`, not just a single in/out count.
2. **setBusArrangements negotiation.** Before activation, call
   `IAudioProcessor::setBusArrangements` with a `SpeakerArrangement` per bus
   derived from the channel counts (mono=kMono, stereo=kStereo, etc.), then
   read back with `getBusArrangement` and verify; fall back to the plugin's
   default if it rejects. (JUCE pattern: `syncBusLayouts`.)
3. **Per-bus ProcessData.** Build `AudioBusBuffers` arrays sized to the real bus
   count; set `numInputs`/`numOutputs` to the true counts; wire each bus's
   channel pointers. The RT process path (`instance.rs:276-320`) must stay
   allocation-free — pre-allocate the per-bus pointer tables in `from_loaded`
   (alongside the existing `BufferPtrs`), not per block.
4. **Bus routing at the bridge boundary.** The subprocess audio protocol and
   `AudioPipeline` currently assume one flat in/out channel set. Decide: either
   (a) extend the shm/protocol to carry multiple buses, or (b) for the first
   landing, support a **main bus + one sidechain input bus** (covers the
   overwhelmingly common case) and document multi-out as a follow-up. **Prefer
   (b) first** — full arbitrary multi-bus is a deep protocol change; the
   sidechain case is what users actually hit.

**Tests.** Unit-test bus enumeration + arrangement selection (counts → speaker
arrangements). Integration test (gated on a real multi-bus VST3 if one is
installed — e.g. a sidechain compressor; check
`/Library/Audio/Plug-Ins/VST3/`) that a sidechain bus receives audio. Add a
no-alloc test proving the multi-bus process path doesn't allocate per block.

**Effort: L. Impact: CRITICAL (sidechain/multi-out/surround). Land in stages:
enumeration → arrangement negotiation → sidechain-input process → (later) full
multi-out.**

### V2 — restartComponent flag handling
**Problem.** `restartComponent(flags)` is captured as
`ParameterEditEvent::RestartComponent(flags)` (`com/component_handler.rs:122-125`)
but **no consumer acts on it** — the GUI bridge filters it out
(`tutti-plugin/src/bridge/gui/vst3.rs`, the event match). Flags are never applied.
**Consequence:** latency changes break PDC, IO changes aren't applied, param
value/title refreshes don't propagate, in-plugin preset loads don't update host
state.
**Fix.** Add a consumer that decodes the `RestartFlags` bitmask and reacts:
- `kLatencyChanged` → re-read `getLatencySamples`, surface to PDC (ties to V4).
- `kParamValuesChanged` → re-read parameter values into host state.
- `kParamTitlesChanged` → re-read parameter info.
- `kIoChanged` → re-run bus enumeration/arrangement (ties to V1).
- `kReloadComponent` → deactivate/reload.
Wire it where `ParameterEditEvent`s are drained (find the consumer that handles
`PerformEdit`; route `RestartComponent` there instead of dropping it).
JUCE pattern: `ComponentRestarter` in the Impl header.
**Test.** Feed a `RestartComponent(kLatencyChanged)` event through the consumer
and assert latency is re-read. Use a test double for the component.
**Effort: M. Impact: high (PDC correctness, in-plugin preset sync).**

### V3 — IMidiMapping (MIDI CC → parameter)
**Problem.** No `IMidiMapping` query anywhere (grep: 0 hits in tutti-vst3-host).
CC is sent as a raw `Data` event (`com/events.rs` / `types/events.rs`).
**Consequence:** VST3 instruments that expose mod-wheel/expression/sustain *only*
via IMidiMapping (the spec's intended path) ignore those CCs — mod wheel, breath,
sustain silently do nothing on many synths.
**Fix.** Query `IMidiMapping::getMidiControllerAssignment` per channel/CC at load,
build a per-channel CC→ParamID table, and at process time route mapped CCs into
`inputParameterChanges` (as param value points at the right sample offset)
*instead of* (or in addition to) the Data event. Re-read the map on
`kMidiCCAssignmentChanged` restart (ties to V2). JUCE pattern: `StoredMidiMapping`
in `juce_VST3Common.h`.
**Test.** With a real VST3 instrument that implements IMidiMapping, assert a CC
moves the mapped parameter. Unit-test the CC→ParamID table lookup.
**Effort: M. Impact: high (mod wheel/sustain/expression on VST3 synths).**

### V4 — Runtime latency tracking
**Problem.** Latency is read once at load (`loaded.rs:264-266`, surfaced in
`loaders/vst3.rs:67`) and never re-read. **Consequence:** PDC goes wrong if the
plugin changes latency at runtime (oversampling/lookahead toggle).
**Fix.** Re-read `getLatencySamples()` on `kLatencyChanged` (the V2 consumer) and
surface the new value out through the bridge to whatever owns PDC. Small once V2
exists.
**Test.** Covered by the V2 `kLatencyChanged` test plus an assertion the new
latency propagates.
**Effort: S (couples to V2). Impact: medium.**

---

## Sequencing for the agent
1. **V2 first** (restartComponent consumer) — it's the backbone V3/V4 plug into,
   and standalone-valuable. 2. **V4** (latency, trivial once V2 lands). 3. **V3**
   (IMidiMapping). 4. **V1 last** and **in its own stages** (enumeration →
   arrangement → sidechain process) — it's the largest and touches the subprocess
   protocol; don't let it block V2–V4.
Land each item (and each V1 stage) as its own commit. Run
`cargo test -p tutti-vst3-host -p tutti-plugin-server` and
`cargo clippy -p tutti-vst3-host -p tutti-plugin-server -p tutti-plugin
-- -D warnings` after each. Keep no-alloc tests green.

## Out of scope (note as follow-ups, don't build)
value→text/text→value (UX), getTailSamples, bypass param wiring, IUnitInfo
program enumeration, content-scale (HiDPI), interchange `.vstpreset` format,
INoteExpressionController. These are quality/polish, not "host properly."

## Done criteria
- restartComponent flags are decoded and acted on (V2); latency re-reads (V4).
- IMidiMapping routes CC→param so mod wheel/sustain work on VST3 synths (V3).
- At minimum sidechain-input multi-bus works end-to-end (V1 stage 1–3), with the
  RT path proven allocation-free; full multi-out documented as follow-up.
- All tutti-vst3-host + tutti-plugin-server tests pass; clippy -D warnings clean.
