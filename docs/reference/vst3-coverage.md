# VST3 format-layer coverage — `tutti-vst3-host` vs VST3 3.8.0

Graded 2026-08-03 against [vst3-3.8.0-interface-surface.md](vst3-3.8.0-interface-surface.md),
which was extracted from Steinberg's SDK headers **before** any of our code was read.
That ordering is the point: a checklist derived from our implementation can only ever
report that we do what we already do.

**Scope: the format layer only** — `crates/plugin/formats/tutti-vst3-host/`.
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

## Correction to the confirmation pass

The confirmation agent could not corroborate the *"overwrite vendor information from
factory info"* quote and marked it unverified. **It is real** — `ipluginbase.h:357`,
directly above the `vendor` field. The search had been run against the extracted spec
table rather than the SDK header, which does not carry per-field comments. The original
finding was right about intent; only its account of the mechanism was wrong (the fields
are never *read*, rather than read and discarded).

Worth recording as a limit of the method: the extracted tables are complete on
*signatures* and lossy on *field-level prose*. For a claim resting on a header comment,
go to the header.

## Fixed so far

**All sixteen** upheld findings are fixed, each mutation-verified:

| Finding | Observable it needed |
|---|---|
| Event buses never activated | new probe mode — invisible to host *and* to every lenient plugin |
| Unstable `IParameterChanges` sort | ≥32 points, descending — smaller fixtures pass against the bug |
| `IAttributeList::getString` underflow | canaries either side of a one-byte buffer (PR #132) |
| `kIoChanged` skipped the restart cycle | activation counter — bus counts read the same either way |
| `kLatencyChanged` re-read out of order | same cycle |
| `kCycleValid` on the wrong gate | the flag/field pairing table it was missing from |
| Half-refused `connect` left dangling | connect-balance counter — the SDK keeps only the current pointer |
| `version` hardcoded `"1.0.0"` | real corpus versions (`5.0.6`, `3.8.0.0`) |
| 6 of 12 `RestartFlags` dropped | whole-mapping test, so a 13th flag cannot join them |
| Controller state never persisted | probe parameter reachable *only* via the controller's own stream |
| `IUnitInfo` never called | the real corpus — `mda-vst3`'s dangling list id, host-checker's 3-level tree |
| Editor input never delivered | a stub view that answers a *chosen* `tresult`, so the consumed mapping is visible |
| `kDefaultActive` never read | a probe bus-activation mask — no corpus plugin distinguishes the policies |
| `getProcessContextRequirements` asked too early | a probe that answers `0` before `initialize` and `kNeedTempo` after |
| Note-expression values unguarded | NaN and out-of-range inputs, each with a distinct verdict |
| `PFactoryInfo::flags` discarded | the corpus's universal `kUnicode` — no plugin sets the flag actually at issue |

The pattern is worth stating plainly: **in twelve of sixteen cases the bug was unobservable
with the tests that existed**, and the work was building something that could see it —
not writing the fix. Four times a test passed against the code it was meant to catch and
had to be rewritten.

All four are worth recording.

The `kDefaultActive` case needed the **fixture** changed, not the test. The corpus caught
"activate everything" immediately, but the opposite error — honouring the flag strictly,
so an unflagged main bus goes silent — passed every test, because *no plugin in the corpus
has an unflagged main bus*. They all take `addAudioOutput`'s default. Giving the probe a
main output with an explicit `flags = 0` created the case the real world had not supplied;
the strict mutation then failed with mask 1 instead of 5. When no available input can
distinguish two behaviours, the test fixture has to manufacture one.

`getProcessContextRequirements` hit the same wall, and there it is nearly a proof rather
than an observation: `hostchecker` fills its flags inside the getter and `dataexchange` in
its constructor, so *neither SDK sample can distinguish a conformant host from one that
asks before `initialize`*, however carefully the test is written. The probe had to become
the plugin that can — answering `0` pre-init and `kNeedTempo` after.

The controller-state fix hit it from the other side. It needed a *legacy* blob —
one saved before the container existed — to still restore. The obvious fixture,
`b"saved-by-an-older-build"`, passed against a build with the compatibility check
deleted: its first byte is `'s'`, which fails the version check by luck rather than by
the guard under test. Replacing it with a binary blob whose leading bytes parse as a
*valid* header killed the mutation immediately — the component then received 3 bytes of
a 20-byte stream. A backward-compatibility fixture has to be one the broken code would
actually mis-handle.

The `IUnitInfo` case is the mirror image: **every assertion was conditional, so all of
them passed vacuously.** The corpus test checked "if a unit resolved a program list,
that list is published" — true, and worthless, when a build that never runs resolution
reports `None` for every unit. Deleting the `resolve_program_list` call left it green.
The fix was a positive count (`19 units resolved a list`) alongside the conditional
checks. Any test built from `if let Some(..)` needs a companion asserting the `Some`
arm is reached at all.

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

**`tests/integration_tests.rs` took no `PLUGIN_LOCK`**, though the rule is documented on
the lock itself: *"Every test that constructs a `Vst3Instance`/`Vst3Loaded` must hold
this."* Two tests load real bundles concurrently; the suite intermittently died with
SIGTRAP under the default parallel runner while passing single-threaded, which is what
made it read as flaky rather than as a missing lock. Fixed — but note the evidence is
weak by nature: five consecutive clean parallel runs do not prove a race is gone. What is
solid is that the file now follows the rule its siblings document.

**`test_load_tal_noisemaker` fails when un-ignored** — it passes the bundle directory to
`Vst3Instance::load` instead of resolving to the inner binary, so `dlopen` fails. Every
sibling test calls `resolve_bundle` first. Pre-existing, `#[ignore]`d, not fixed here.

**~~`BusFlags::kDefaultActive` is never read.~~ FIXED** — `host/mod.rs::wants_activation`.
**[verified]**

**Not a conformance defect, and the original framing overstated it.** `ivstcomponent.h:54-55`
says the flag is *"only a wish, the host is allow to not follow it, and only activate the
first bus for example"* — activating everything was explicitly permitted. The SDK's own
validator requires the converse to work too: `busactivation.cpp:66-71` fails any plugin
that refuses activation of an unflagged bus. So this stops overriding a plugin's stated
preference; it does not fix a bug.

The policy is **`kMain` unconditionally, `kAux` only when flagged**. Steinberg's VST2/AU
wrapper reaches the same split from the other direction — `basewrapper.cpp:1061-1085`
activates every main bus without consulting the flag at all.

Honouring the flag on main buses too is what `auwrapper.mm:553` does behind
`SMTG_AUWRAPPER_ACTIVATE_ONLY_DEFAULT_ACTIVE_BUSES`, and its CMake note says why it is
off by default: *"This may not work on some hosts because they never activate a bus
later."* That is this host — there is no public per-bus activation API, so a bus skipped
at load is unreachable for the instance's lifetime rather than merely inactive.

Two constraints worth keeping. Bus **geometry stays on declared buses**: `SlabLayout`, the
batcher and fundsp port arity all index positionally off declared widths, so filtering the
scratch resolve as well would silently take a multi-out instrument from N stem ports to 2
and shift every `connect()` in a user's graph. And a bus whose `getBusInfo` fails is
activated anyway — that restores the old behaviour for exactly the plugins that cannot
answer the question the policy asks.

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
| ~~Keyboard, wheel and focus are never delivered to plugin editors.~~ **FIXED (host layer)** — `send_key_down`/`send_key_up`/`send_wheel`/`set_editor_focus` on `Vst3Loaded`, returning *consumed* so the plugin arbitrates. The Bevy-side `KeyCode` → `VirtualKeyCodes` table is not written; nothing calls these yet. | `host/loaded.rs` | **[verified]** |
| ~~`IUnitInfo` is never called.~~ **FIXED** — `units()` / `program_lists()` / `program_name()` / `selected_unit()` / `select_unit()` / `unit_by_bus()` bind the interface. Returned flat, in the plugin's order, each unit naming its parent; assembling a tree is the caller's job. Measured: host-checker 54 units, 48 nested. | `types/info.rs`, `host/loaded.rs` | **[verified]** |
| ~~`kIoChanged` mutates bus counts on a live active instance.~~ **FIXED** — `Vst3Instance::restart_bus_configuration` owns the cycle; `Vst3Loaded` surfaces the flag instead of acting on it. | `host/instance.rs` | **[verified]** |
| ~~`kCycleValid` is set under the wrong requirement gate.~~ **FIXED** — gated on `NEED_CYCLE_MUSIC` + finiteness, paired with the fields it advertises. | `types/transport.rs` | **[verified]** |

### Medium

| Finding | Where | Tag |
|---|---|---|
| ~~6 of 12 `RestartFlags` are decoded and then dropped.~~ **FIXED** — all 12 now reach `RestartOutcome`, pinned by `every_decoded_restart_flag_reaches_the_outcome`. Deliberately stops at the format layer: carrying them further needs new `AsyncEvent` variants and a `PROTOCOL_VERSION` bump for a signal nothing yet consumes — `AsyncEvent::IoChanged` already crosses the wire to no reader. | `host/loaded.rs` | **[verified]** |
| **`disconnect()` is called from `Drop`**, which the code itself documents as possibly running on the audio thread. The spec marks `disconnect` `[UI-thread & Connected]`. Self-documented thread-contract violation. | `host/loaded.rs:1532` | **[verified]** |
| **`resizeView` does not call `onSize` in the same callstack.** Deliberate and documented — reentrant `onSize` causes feedback loops with plugins that re-issue `resizeView`. But the spec is explicit (*"Afterwards, in the same callstack, the host has to call IPlugView::onSize()"*) and the SDK's own `editorhost.cpp:361` does it inline. A known trade-off, not an oversight. | `com/plug_frame.rs:70` | **[verified]** |
| **`process()` never clamps `numSamples`** to the negotiated `maxSamplesPerBlock`. Only a zero-check exists. Severity held at medium: reachability depends on caller guarantees outside this crate. | `host/instance.rs:536` | **[verified]** |
| ~~`isPlugInterfaceSupported` denies interfaces we consume.~~ **REFUTED** — every interface named is *plug-in*-side; the host queries them off the plugin rather than providing them. The code documents this deliberately and pins it with `does_not_claim_uninstalled_interfaces`. | `com/host_application.rs:186-215` | — |
| ~~`version` is hardcoded `"1.0.0"` for every plugin.~~ **FIXED** — `PClassInfoW`'s `vendor`/`version` are now read and carried through `ClassInfo` *and* `AudioClass`, which both dropped them; the class vendor takes precedence per `ipluginbase.h:357`. Measured after: TAL-NoiseMaker `5.0.6`, Note Expression Synth `3.8.0.0`; ADelay and mda genuinely are `1.0.0`, so the fallback is indistinguishable and correctly so. | `host/library.rs`, `host/loaded.rs` | **[verified]** |
| ~~Gesture bracketing is not tracked or validated.~~ **REFUTED** — spec `:565-567` places the ordering duty on the plug-in ("*before* a performEdit", "*between* beginEdit and endEdit"). The host is the callee. Contrast `IEditControllerHostEditing` `:289-290`, where the host *is* the caller and does bracket. | `com/component_handler.rs:168-192` | — |
| ~~`kLatencyChanged` re-reads latency without the required deactivate/reactivate.~~ **FIXED** — folded into the same cycle as `kIoChanged`, which the header specifies identically (`:137-138`); latency is re-read after reactivation. *Corrected:* it was never "surfaced and ignored" — it was consumed, just in the wrong order. | `loaders/vst3.rs` | **[verified]** |
| ~~`kReloadComponent` is surfaced but no reload path exists.~~ **REFUTED** — `loaders/vst3.rs:325-333` calls `reload()`, which saves state, rebuilds, and restores. The original finding read only `host/loaded.rs`, whose doc comment says the reload is the owner's job, and never checked the owner. | `loaders/vst3.rs:264-277` | — |
| ~~`IEditController::setState`/`getState` are never called.~~ **FIXED** — `state()` now carries both streams in a magic-prefixed container, and `set_state` drives component → `setComponentState` → controller in spec order. A blob saved by an older build has no header and still restores. | `host/loaded.rs` | **[verified]** |
| ~~Both `connect()` return values are discarded.~~ **FIXED** — the first return is checked and a failed second call unwinds the first. A refusal stays non-fatal. *Citation corrected:* `1334-1335` is `disconnect`, where discarding is harmless; the real site was `1313-1314`. | `host/loaded.rs` | **[verified]** |

### Low

Four remained after confirmation. **All four are now fixed**, though the last was first
declined for a reason worth recording.

`getProcessContextRequirements` was queried before `IComponent::initialize` **[fixed]**.
`ivstaudioprocessor.h:456` marks it `[UI-thread & Setup Done]`. The failure is silent in
the worst direction — the early answer is a *subset*, so the host quietly withholds
fields the plugin asked for and a tempo-driven plugin free-runs with no error anywhere.

Note-expression values were staged with no normalized-range or NaN guard **[fixed]**.
`ivstnoteexpression.h:89` states expression events are *"always absolute normalized values
[0.0, 1.0]"* and addresses the **host**, so normalizing is our duty. The two out-of-range
cases are not the same fact: a NaN is dropped (no nearest legal value exists, and
`f64::clamp` returns NaN for a NaN input, so a clamp alone is not a guard), a finite
out-of-range value is clamped (dropping it would freeze the dimension at whatever the
plugin last saw).

A `UnitEvent::ProgramListChanged` doc comment asserted a contract the header does not
state **[fixed]** — it means "this program info is stale", not "the selection changed",
and did not mention that `-1` is the `kAllProgramInvalid` sentinel a consumer would
otherwise spend as an array index.

`PFactoryInfo::flags` was dropped, so `kClassesDiscardable` never reached the host
**[fixed]**. `get_factory_info` copied vendor, url and email and did not mention the fourth
field. It is now carried raw, with `classes_discardable()` / `component_non_discardable()`
/ `unicode_strings()` decoding by mask.

**Nothing consumes it yet, and that is a separate gap.** The scan cache
(`tutti_plugin::host::discovery`) is keyed on mtime alone (`catalog.rs:105-110`) and
stores **one** `PluginDescriptor` per bundle path, while `find_audio_class`
(`loaded.rs:2047`) is a `find_map` taking the first audio class — `mda-vst3`'s 34 classes
already collapse to a single record. So a bundle's class *list* is not something the
catalog can currently represent, let alone re-derive on demand. Acting on the flag becomes
meaningful when that changes; it needs a field on `PluginDescriptor` (a bincode wire type
crossing the plugin-server IPC) and the other three format loaders answering it.

Worth separating carefully, because the first attempt at this finding got it wrong:
**a downstream consumer's limitation is not a reason for the boundary layer to discard
data.** The audit is of `tutti-vst3-host`, and "what did the plugin tell us" is entirely
inside it — four lines. The `PluginDescriptor` / catalog work is `tutti-plugin`'s gap, and
scoping it into this finding inflated the cost of a small fix into a reason to skip it.

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

**High:** none outstanding — `IUnitInfo` is bound (above).

Two things the corpus taught that the spec did not. **The root unit is implicit**:
`ivstunits.h:144-145` says `getUnitCount` "must return 1 at least" and that the root's id
is 0, but the SDK's own `EditControllerEx1` never adds a unit *for* the root, and
host-checker only ever attaches its units *to* `kRootUnitId`. A host demanding an
explicit root entry would reject the SDK's reference plugin. That asymmetry is the
reason `units()` stays flat — there is no root node to hang a tree off without inventing
one. **And a `programListId` can name nothing**: `mda-vst3` unit 0 points at list
`1886548852`, which it never publishes. `Vst3UnitInfo::program_list` is therefore `Some`
only when the id resolves, with `has_dangling_program_list()` to tell that apart from
having no list at all.

**Medium:** `IMidiMapping2` and `IMidiLearn2` — both tagged `[replaces …]` in 3.8.0, both absent while their v1 forms are used. MIDI 2.0 controller assignments are unreachable. Also `IProgramListData`, `IComponentHandlerSystemTime`, `IInfoListener`, `IStreamAttributes`, `IPluginFactory2`.

`IMidiMapping2` is **blocked on a binding defect**, not merely unwritten — tracked as
issue #140. (An earlier revision of this document said the opposite: *"the binding already
ships them, so this is unwritten code, not a dependency limit."* The binding does ship
them, but its `Midi2Controller` is the wrong size. Measured by compiling both: C gives 12
bytes with `offsetof(controller) == 9`, Rust gives 16. The cause is that `Midi2Controller`
uses C bitfields and `com-scrape` has no bitfield handling at all — it is the only
bitfield struct in the entire `pluginterfaces/` tree, which is why nothing else is
affected. Verified upstream: `vst3-0.3.0` is the latest release (2025-12-07), the bug is
still on `main` (`b2a45f2`), and no issue among 8 open issues / 20 PRs reports it.)

`IMidiLearn2` is **not** blocked — it passes `Midi2Controller` by value, with no array and
no stride, so the size defect does not reach it.

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
host-duty inversions came from not asking who the spec addresses. And most `[verified]`
fixes needed a *new observable* before they could be tested at all — the event-bus
omission is invisible to both the host and every lenient plugin, and the unstable sort is
invisible below ~32 points.

The corollary took the whole audit to state: **a spec rule no available plugin exercises
is not thereby untestable — it means the fixture is the deliverable.** Three fixes ended
with a change to `audio-probe` rather than to a test file, and in one case the SDK's own
samples are provably incapable of witnessing the rule at all.

The last finding to close taught a different lesson, by first being closed wrongly.
`kClassesDiscardable` was initially declined on the grounds that nothing downstream could
act on it — the scan cache cannot represent a bundle's class list, so honouring the flag
would change no behaviour. All of that is true, and none of it was a reason to keep
*discarding* the flag. **A downstream consumer's limitation does not license the boundary
layer to drop data.** Reading what the plugin said is four lines and entirely inside the
crate under audit; the catalog work is a different crate's gap. Scoping the consumer into
the finding inflated a small fix into a reason to skip it.

The general form: when declining a finding, check that the cost being weighed is the cost
of *the finding*, and not the cost of everything one would want to build on top of it.

Nothing here is a regression: these are gaps and deviations that have been present, not
recent breakage. The two dead conformance suites are the exception — those *were* a
regression, silent because the feature flag kept them out of the default test run.
