# VST3 format-layer coverage — `tutti-vst3-host` vs VST3 3.8.0

Graded 2026-08-03 against [vst3-3.8.0-interface-surface.md](vst3-3.8.0-interface-surface.md),
which was extracted from Steinberg's SDK headers **before** any of our code was read.
That ordering is the point: a checklist derived from our implementation can only ever
report that we do what we already do.

**Scope: the format layer only** — `crates/tutti/crates/plugin/formats/tutti-vst3-host/`.
It says nothing about whether the engine above (`tutti-plugin`, `tutti-plugin-server`)
can reach these capabilities. That seam is 13 trait methods wide and is a separate audit;
a capability can be perfectly bound here and still be unreachable from the DAW. See the
`PluginTail` precedent, which was populated by all four loaders and read by nothing for a
full release cycle.

## Method

Two workflow passes, 119 agents, 0 errors.

| | Pass 1 | Closeout | Confirmation | Total |
|---|---|---|---|---|
| Interfaces contract-verified | 8 | 29 | — | **37** |
| Absences classified | 34 | — | — | **34** |
| Contract rules evaluated | — | 243 | — | **243+** |
| Claims raised | 24 | 24 | — | 48 |
| Refuted on adversarial review | 14 | 15 | 3 | **32** |
| Misdescribed (right bug, wrong account) | — | — | 3 | **3** |
| **Upheld** | 10 | 9 | — | **16** |

The confirmation pass re-checked all nine `[claimed]` findings at source. It refuted three
more, corrected three descriptions, and **upgraded one** — so the tags were doing real
work in both directions, not just filtering noise downward.

Every claimed defect was handed to a second agent instructed to **refute** it, defaulting
to refuted unless the evidence was airtight. Three in five did not survive. The refutations
were rarely about bad facts — the recurring shape was *"the code citations are all accurate,
but the spec contract it asserts does not exist"*, or the claim inverted the header it
quoted. Misattributing a **plug-in-side** duty to the **host** was the single largest source.

That 60% is the headline methodological result: an unreviewed sweep of this kind would have
produced a document that was mostly wrong while looking authoritative.

## Confidence tags

Findings below are marked:

- **[verified]** — I re-checked this at source myself, independent of the agent.
- **[claimed]** — survived adversarial review, not independently re-checked.

`[claimed]` is not a lower standard of evidence so much as a shorter chain of it. One
`[claimed]` finding in the first pass turned out to be **correctly diagnosed but wrongly
described** (see RestartFlags) — right bug, wrong file. Treat `[claimed]` as "investigate",
not "fix".

## Status after the confirmation pass

Every `[claimed]` finding was re-checked at source. Of the nine, **four were upheld,
two were misdescribed, one was refuted, and two were downgraded to cosmetic**. Two
findings changed materially:

- **`kIoChanged` (high)** — upheld, and the mechanism is worse than stated. It is
  reachable on a *live active* instance because `Vst3Instance` `DerefMut`s to
  `Vst3Loaded` (`instance.rs:812-821`), so the typestate meant to prevent it does not.
  `kReloadComponent` was given the correct treatment — surfaced to an owner that can
  deactivate — and `kIoChanged` has the identical constraint and was not.
- **`kCycleValid` (was low)** — upheld and **upgraded**. `transport.rs:98` sets the flag
  under `NEED_TRANSPORT_STATE` while the fields it advertises are written at `:183-185`
  under `NEED_CYCLE_MUSIC`. Two gates on a flag and its payload gives two failures: a
  plugin asking only for transport state gets `kCycleValid` over zeroed bounds, and one
  asking only for cycle music gets bounds with no validity bit. The spec maps the flag
  to `kNeedCycleMusic` (`:1078`) and lists only `kPlaying`/`kCycleActive`/`kRecording`
  under `kNeedTransportState` (`:1084`).

Two were refuted by reading past the crate boundary — `kReloadComponent` **is** handled
(`loaders/vst3.rs:325-333`), and `kLatencyChanged` **is** consumed (`:302-307`), leaving
only its missing deactivate/reactivate. Both original findings stopped at
`host/loaded.rs` and never checked the server that consumes the flags.

`isPlugInterfaceSupported` and gesture bracketing were refuted outright as
plug-in-duty/host-duty inversions — the code already documents the former deliberately
and pins it with a test. The per-class `vendor` finding was misdescribed: those fields
are never *fetched*, `getClassInfo2` is unreferenced repo-wide, and the quoted "overwrite
vendor information from factory info" could not be corroborated in the spec. What does
hold is `loaded.rs:1742` hardcoding `.version("1.0.0")` for every plugin.

## Fixed in this pass

**Event buses were never activated** — `host/instance.rs:738`. **[verified]**

`activateBus` was called at two sites, both `K_AUDIO`; `K_EVENT` was counted only to
detect MIDI capability. `ivstcomponent.h:52` is unqualified — *"All busses are initially
inactive"* — and `kEvent` is a `MediaTypes` value beside `kAudio`.

Making this observable took a new plugin-side mode. The omission is invisible from both
ends: `BusInfo` carries no active field, so a host cannot read its own activation back,
and HostChecker validates the `ProcessData` handed over rather than the lifecycle before
it. Every Steinberg sample and every existing probe mode is lenient — they read
`data.inputEvents` regardless of bus state — so all nine pre-existing MIDI tests passed
with the fix reverted. `kModeEventBusActive` asks the plugin directly, and fails with the
fix reverted.

**`IParameterChanges` merge used an unstable sort** — `host/midi_mapping.rs:241`.
**[verified]**

VST3 encodes a discontinuity as two points sharing a `sample_offset`, and their order is
the jump's direction. The pre-existing test could not catch this: its offsets are all
distinct, and with no equal keys the two sorts are indistinguishable. Nor is a small
fixture enough — Rust's unstable sort runs insertion sort on short slices and preserves
order there, so the first replacement test passed against the bug. The two diverge from
~32 points with descending offsets, which is what the test now builds; reverted, it
transposes 11 of 40 jumps.

## Also found while fixing

**Two conformance suites did not compile**, and had not since `4831c6115` keyed automation
by `ParamAddress` without updating them. `vst3_conformance.rs` (32 tests, wrapping ~185
Steinberg checks) and `vst3_audio_correctness.rs` (9 tests, the audio oracle) are both
behind the `conformance` feature, so a default `cargo test` never saw them. Restored here.

**`tests/integration_tests.rs` takes no `PLUGIN_LOCK`**, though the rule is documented on
the lock itself: *"Every test that constructs a `Vst3Instance`/`Vst3Loaded` must hold
this."* Two tests load real bundles concurrently; the suite intermittently dies with
SIGTRAP under the default parallel runner and passes single-threaded. Pre-existing on
`main`, not fixed here.

**`BusFlags::kDefaultActive` is never read.** `types/info.rs:44` stores the flags bitfield
and nothing consults it. Our fix activates every bus unconditionally, which is defensible
for a host that intends to use them, but the flag exists to carry plugin preference.

## Memory safety — fixed

**`IAttributeList::getString` wrote through a wild pointer** — `com/attr_list.rs:133`.
**[verified]** · PR #132

`size_in_bytes` is a **byte** count; `TChar` is 2 bytes. The guard rejected `0` but not `1`,
which divides to `max_chars == 0`, and `max_chars - 1` wraps to `usize::MAX`. Release builds
wrap silently. This is the byte-vs-character trap the spec table already flagged on this
boundary — an odd size is legal to pass and impossible to satisfy.

Not a conformance gap: reachable from plugin-controlled input, and independent of any
spec interpretation being right.

## Contract findings

### High

| Finding | Where | Tag |
|---|---|---|
| ~~Event (MIDI) buses are never activated.~~ **FIXED** — see above. | `host/instance.rs:738` | **[verified]** |
| ~~`IParameterChanges` merge uses an unstable sort.~~ **FIXED** — see above. | `host/midi_mapping.rs:241` | **[verified]** |
| **Keyboard, wheel and focus are never delivered to plugin editors.** `onKeyDown` / `onKeyUp` / `onWheel` / `onFocus` have zero non-doc call sites. | — | **[verified]** |
| **`IUnitInfo` is never called.** We store `unitId` on parameters, note expressions and keyswitches — three separate structs — and never call the interface that gives those IDs meaning. A raw symbol grep scores this "covered"; it is doc-comment mentions only. | `types/info.rs:81,264,395` | **[verified]** |
| **`kIoChanged` mutates bus counts on a live active instance** without the spec-required deactivate/reactivate cycle (spec `:975` — "Deactivate, re-ask bus configs, adapt the graph, reactivate"). Only the re-ask third is done. Reachable while active because `Vst3Instance` `DerefMut`s to `Vst3Loaded`. | `host/loaded.rs:822-828` | **[verified]** |
| **`kCycleValid` is set under the wrong requirement gate**, and separately from the fields it advertises. Upgraded from low after re-checking. | `types/transport.rs:98` vs `:183-185` | **[verified]** |

### Medium

| Finding | Where | Tag |
|---|---|---|
| **6 of 12 `RestartFlags` are decoded and then dropped.** *Correcting the finding's own wording:* all 12 **are** decoded into struct fields; only 6 are forwarded to `RestartOutcome`. The other six — `note_expression_changed`, `io_titles_changed`, `prefetchable_support_changed`, `routing_info_changed`, `keyswitch_changed`, `param_id_mapping_changed` — are consumed at **zero** sites. `param_id_mapping_changed` (3.7.11) fires during project load; losing it loses automation on plugin replacement. | `host/loaded.rs:121-126` | **[verified]** |
| **`disconnect()` is called from `Drop`**, which the code itself documents as possibly running on the audio thread. The spec marks `disconnect` `[UI-thread & Connected]`. Self-documented thread-contract violation. | `host/loaded.rs:1532` | **[verified]** |
| **`resizeView` does not call `onSize` in the same callstack.** Deliberate and documented — reentrant `onSize` causes feedback loops with plugins that re-issue `resizeView`. But the spec is explicit (*"Afterwards, in the same callstack, the host has to call IPlugView::onSize()"*) and the SDK's own `editorhost.cpp:361` does it inline. A known trade-off, not an oversight. | `com/plug_frame.rs:70` | **[verified]** |
| **`process()` never clamps `numSamples`** to the negotiated `maxSamplesPerBlock`. Only a zero-check exists. Severity held at medium: reachability depends on caller guarantees outside this crate. | `host/instance.rs:536` | **[verified]** |
| ~~`isPlugInterfaceSupported` denies interfaces we consume.~~ **REFUTED** — every interface named is *plug-in*-side; the host queries them off the plugin rather than providing them. The code documents this deliberately and pins it with `does_not_claim_uninstalled_interfaces`. | `com/host_application.rs:186-215` | — |
| **`version` is hardcoded `"1.0.0"` for every plugin.** *Corrected:* per-class `vendor`/`version` are not "fetched then discarded" — they are never fetched. `class_info_unicode` reads only `cid`/`category`/`name`, `ClassInfo` has no fields for them, and `getClassInfo2` is unreferenced repo-wide. The "overwrite vendor information from factory info" quote could not be corroborated. | `host/loaded.rs:1742`, `host/library.rs:220-238` | **[verified]** |
| ~~Gesture bracketing is not tracked or validated.~~ **REFUTED** — spec `:565-567` places the ordering duty on the plug-in ("*before* a performEdit", "*between* beginEdit and endEdit"). The host is the callee. Contrast `IEditControllerHostEditing` `:289-290`, where the host *is* the caller and does bracket. | `com/component_handler.rs:168-192` | — |
| **`kLatencyChanged` re-reads latency without the required deactivate/reactivate.** *Corrected:* it is not "surfaced and ignored" — `loaders/vst3.rs:302-307` consumes it and re-reads. What is missing is the cycle around the read, so a plugin that recomputes group delay in `setActive(true)` returns a stale figure. | `loaders/vst3.rs:302-307` | **[verified]** |
| ~~`kReloadComponent` is surfaced but no reload path exists.~~ **REFUTED** — `loaders/vst3.rs:325-333` calls `reload()`, which saves state, rebuilds, and restores. The original finding read only `host/loaded.rs`, whose doc comment says the reload is the owner's job, and never checked the owner. | `loaders/vst3.rs:264-277` | — |
| **`IEditController::setState`/`getState` are never called** — controller-only UI state is not persisted, separately from component state. | — | [claimed] |
| **Both `connect()` return values are discarded**, so a half-succeeded connect leaves the pair asymmetrically wired with no unwind and `initialize()` proceeds regardless. *Citation corrected:* `1334-1335` is `disconnect`, where discarding is harmless — the real site is `1313-1314`. | `host/loaded.rs:1313-1314` | **[verified]** |

### Low

After confirmation, four remain: `getProcessContextRequirements` queried before
`IComponent::initialize`; `PFactoryInfo::flags` dropped so `kClassesDiscardable` cannot
suppress mtime-gated rescans (a host-policy cost, not a spec violation);
note-expression values staged without a normalized-range or NaN guard, where the
contract *is* stated (`:1337`, "normalized [0.0, 1.0]") and `transport.rs` already gates
every field on `is_usable` for the same reason; and a `UnitEvent::ProgramListChanged` doc
comment asserting a contract the header does not state — it means "this program info is
stale", not "the selection changed", and does not mention that `-1` is the
`kAllProgramInvalid` sentinel a consumer would otherwise use as an array index.

The fifth, `kCycleValid`, was upgraded to the High table. The sixth — a `u32→i32` wrap on
CC frame offsets — was **refuted**: the cast needs an offset ≥ 2³¹ to wrap, which is 13.5
hours of samples inside one process block against a real block of ≤ 8192. Unreachable by
construction, and the spec types `sampleOffset` as `int32` anyway.

## Absent interfaces — 34 classified

| Verdict | Count | Notes |
|---|---|---|
| Deliberately inapplicable | 20 | iOS-only (InterAppAudio ×3), validator-only (`ITestPlugProvider` ×2), wrapper markers (`IVst3To*` ×3), base-layer services a Rust host has no use for |
| **Real gap** | **13** | below |
| Actually present | 1 | `IPluginBase` — a false negative from the scripted inventory, caught by the "show your negative search" rule |

**High:** `IUnitInfo` (above).

**Medium:** `IMidiMapping2` and `IMidiLearn2` — both tagged `[replaces …]` in 3.8.0, both absent while their v1 forms are used. MIDI 2.0 controller assignments are unreachable. The binding already ships them (`vst3-0.3.0`), so this is unwritten code, not a dependency limit. Also `IProgramListData`, `IComponentHandlerSystemTime`, `IInfoListener`, `IStreamAttributes`, `IPluginFactory2`.

**Low:** `IEditControllerHostEditing`, `IUnitData`, `IEditController2`, `IParameterFinder`, `ISizeableStream`.

## Clean

Nine interfaces were checked and found to honour every rule evaluated: `IPluginFactory3`,
`IHostApplication`, `IMessage`, `IEventList`, `IComponentHandler3`, `IProgress`,
`IAutomationState`, `INoteExpressionPhysicalUIMapping`, `IParameterFunctionName`.

Two — `IContextMenu`, `IDataExchangeHandler` — proved to be type imports with no real call,
and were reported as such rather than scored as violations.

The `IComponent` verifier also listed nine rules as explicitly correct, including that
accepting `kNotImplemented` from `getState` is deliberate and matches the SDK's own
`vstpresetfile.cpp:53`. Worth recording: the audit was judging, not pattern-matching for
defects.

## What this document is not

A bug list. It is a **work-list with citations**. Sixteen findings survived three passes,
the last of which re-checked every `[claimed]` item at source and still refuted three,
corrected three, and upgraded one.

The recurring lesson across all three passes is the same: **a finding is only as good as
the boundary it was checked across.** The two refutations in the last pass both came from
reading past `host/loaded.rs` into the server that consumes its output. The two
host-duty inversions came from not asking who the spec addresses. And two of the three
`[verified]` fixes needed a *new observable* before they could be tested at all — the
event-bus omission is invisible to both the host and every lenient plugin, and the
unstable sort is invisible below ~32 points.

Nothing here is a regression: these are gaps and deviations that have been present, not
recent breakage. The two dead conformance suites are the exception — those *were* a
regression, silent because the feature flag kept them out of the default test run.
