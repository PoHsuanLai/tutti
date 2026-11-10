# VST3 V1 — multi-bus / sidechain (full, including protocol change)

**Goal:** route distinct audio to a VST3 plugin's secondary input bus (sidechain),
end-to-end, so e.g. a sidechain compressor keyed off a kick works. This spans all
four layers and **breaks the IPC wire format** — it is the largest single piece of
the hosting work. Execute it in **staged commits with hard stop points**; if any
stage balloons beyond its description, STOP and report rather than forcing it.

Branch base: `vst3-completion` HEAD (includes V2/V3/V4 + their review fixes).
Work in the worktree you're given. Run cargo from the tutti sub-workspace root.

Testing policy (CLAUDE.md): never weaken tests; fix root causes; add tests per
stage. Keep the RT path allocation-free — the existing `assert_no_alloc` tests in
tutti-vst3-host and `tutti-plugin-server/src/audio_pipeline.rs`
(`process_is_alloc_free`) MUST stay green; add multi-bus no-alloc coverage.

Reference (read-only, do NOT copy — AGPL): JUCE VST3 host
`JUCE's `juce_VST3PluginFormatImpl.h` (reference reading, not vendored here)`
(`associateWith`, `syncBusLayouts`, per-bus `AudioBusBuffers`).

---

## What already exists (don't rebuild)

- **The DAW already models sidechain at the graph layer.** bevy-tutti has
  `SidechainOf`/`SidechainSources` + `reconcile_sidechain_links`, which calls
  `graph.connect(src, 0, target, 1)` — **port 1 is the sidechain input**
  (`crates/bevy-tutti/src/graph/sidechain.rs`). So the DAW-side wiring exists; the
  gap is that this never reaches a VST3 second bus.
- **VST3 host already enumerates+activates all buses** in `activate_buses`
  (`host/loaded.rs:477`) — it just only *reconciles channel counts* for bus 0.
- The Explore boundary map is in this directory's sibling analysis; key file:line
  anchors are inlined below.

## The end-to-end chain that must light up

fundsp `PluginClient::inputs()` must report main+sidechain channels and keep them
separated (`audio_node/audio_unit.rs:10`, currently one flat `io_ref().inputs`) →
the node must carry the bus split across the shm slab + wire protocol → the
subprocess `AudioPipeline` must place each bus's channels into the right scratch →
the VST3 host `process()` must build a per-bus `AudioBusBuffers` array
(`host/instance.rs:337` currently hardcodes `numInputs:1, numOutputs:1`).

---

## Stages (commit each; stop if a stage exceeds its description)

### Stage 1 — VST3 host: per-bus enumeration + ProcessData (Category A, no wire change)
Make the VST3 host *capable* of multi-bus with the data it already gets.
- Store a `Vec<BusInfo{ direction, channel_count }>` on `Vst3Loaded` (extend
  `types/mod.rs` PluginInfo or add a sibling field). Enumerate ALL buses in
  `reconcile_bus_counts`/`activate_buses` (`host/loaded.rs:62,477,794`), not just
  bus 0 — loop `0..getBusCount(K_AUDIO, dir)`.
- In `host/instance.rs`: generalize `make_audio_bus` (line 44) + the `AudioIO`
  scratch (line 76) to hold a per-bus `AudioBusBuffers` array; set
  `numInputs`/`numOutputs` to the real bus counts (replace the hardcoded `1`s at
  337-354). Pre-allocate the per-bus pointer tables in `from_loaded` so the RT
  `process` path stays allocation-free. For now, map the existing flat input onto
  bus 0 and feed silence to extra input buses (no sidechain data yet).
- **Test:** unit-test bus enumeration (counts per bus). If a multi-bus VST3 is
  installed under `/Library/Audio/Plug-Ins/VST3/`, add an `#[ignore]` integration
  test that a 2-input-bus plugin activates and processes without error. Add a
  no-alloc test for the multi-bus process path.
- **Commit.** This is self-contained and reviewable. STOP and report if the
  ProcessData/AudioBusBuffers FFI proves fiddlier than expected (zeroed structs,
  `__field0` union for channelBuffers32/64).

### Stage 2 — Wire/shm protocol: carry a bus layout (Category B, BREAKS protocol)
Extend the protocol so the slab + messages describe buses, not one flat channel set.
- `protocol/metadata.rs`: add `buses: Vec<BusInfo>` to `PluginInfo` with
  `#[serde(default)]` (old peers still deserialize → empty = single-bus legacy).
- `protocol/shm.rs` `SlabLayout`: keep `channels` as the flat total but add a
  `#[serde(default)]` bus descriptor list (offset + count per bus) so both sides
  agree which flat channels map to which bus. `byte_size` stays `channels * …`.
- `protocol/envelope.rs` `SetupSharedMemory`: include the bus layout (or send it in
  `PluginLoaded` and have the server cache it). Prefer caching at load — fewer
  message changes.
- `protocol/process.rs`: ProcessAudio messages can stay layout-free IF the layout
  is negotiated once at load and cached server-side. Do that; don't put bus layout
  on every block.
- **Compat:** every new field `#[serde(default)]`; empty bus list == today's
  single-bus behavior. Verify old serialized round-trips still parse (add a test).
- **Test:** serialize/deserialize round-trip of `PluginInfo`/`SlabLayout` with and
  without buses; assert legacy (no-buses) still works.
- **Commit.** STOP and report after this stage regardless — it's the irreversible
  format decision and worth a checkpoint before wiring audio through it.

### Stage 3 — subprocess + host-side bridge: split flat channels into buses
- `tutti-plugin-server/src/audio_pipeline.rs` (`process`, lines ~150-207): use the
  cached bus layout to read each bus's channels from the slab into the right
  scratch region and hand the VST3 host a per-bus view. Keep the clear-in-place /
  no-alloc discipline (the `process_is_alloc_free` test must extend to cover it).
- `tutti-plugin` audio_node (`audio_unit.rs`): `inputs()` must report
  main+sidechain channel total; `tick`/`process` must place port-1 (sidechain)
  input into the sidechain bus region of the slab. This is where fundsp's port-1
  convention meets the bus split.
- **Test:** drive `AudioPipeline::process` with a 2-bus layout via the hermetic
  fake plugin (extend the existing `NanPlugin` harness in audio_pipeline tests) and
  assert each bus receives the right channels; no-alloc test.
- **Commit.** STOP and report if the audio_node ↔ slab bus mapping gets hairy.

### Stage 4 — bevy-tutti graph: connect SidechainOf to the plugin's bus 1
- Confirm `PluginClient` (fundsp node) now reports inputs that include the
  sidechain port so `reconcile_sidechain_links`'s `connect(src, 0, target, 1)`
  actually lands on a real port-1.
- This is the FUZZIEST stage (touches the outer dawai/bevy-tutti, not just tutti).
  Likely just verifying the existing `SidechainOf` wiring now reaches through. If
  it requires real changes in bevy-tutti/dawai-frontend, STOP and report scope —
  that may be a separate PR in the outer repo.
- **Commit / report.**

---

## Sequencing & guardrails
Do stages in order; **commit each**; **hard stop + report after Stage 2** (the
protocol break) before proceeding, and stop early on any stage that exceeds its
description. After each stage: `cargo test -p tutti-vst3-host -p tutti-plugin
-p tutti-plugin-server` + `cargo clippy … -- -D warnings`. Keep all no-alloc tests
green. Don't push/merge/PR; commit to the working branch.

## Out of scope (note, don't build)
`setBusArrangements`/`getBusArrangement` negotiation (accept plugin defaults for
now); full arbitrary multi-OUTPUT routing (focus: main + one sidechain INPUT);
multi-bus for VST2/AU/CLAP.

## Done criteria
- Stage 1: VST3 host builds per-bus ProcessData, all buses enumerated/activated,
  RT path alloc-free, tests green. Mergeable on its own.
- Stage 2: protocol carries an optional bus layout, back-compatible
  (`#[serde(default)]`), round-trip tested. Checkpoint for review.
- Stage 3: subprocess splits flat channels into buses; sidechain audio reaches the
  VST3 second bus through the bridge; no-alloc preserved.
- Stage 4: a `SidechainOf` link feeds a VST3 sidechain compressor's key input
  (or scoped-out as an outer-repo follow-up with a clear report).
