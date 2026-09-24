# Audio Unit format-layer coverage — `tutti-au-host` vs AUv2

Graded 2026-08-03 against [au-v2-interface-surface.md](au-v2-interface-surface.md), which was
extracted from Apple's SDK headers **before** any of our code was read. That ordering is the
point: a checklist derived from our implementation can only ever report that we do what we
already do.

**Scope: the format layer only** — `crates/plugin/formats/tutti-au-host/`. The
crate declares itself AUv2 (`lib.rs:1-4`), which turns out to matter for grading — see the
AUv3 finding.

## Method

One survey pass, one adversarial refutation pass, plus independent re-checks at source.

| | Count |
|---|---|
| Core C entry points in the surface (section A) | **47** |
| Property constants (section B, deduplicated) | **170** |
| Host-callback surfaces (section C) | **13** |
| Entry points BOUND | **21** |
| Properties actually exercised | **38** |
| Host callbacks IMPLEMENTED | **5** (of 13; the rest are AUHAL/IAA/AUv3, out of scope) |
| Claims raised | 5 |
| **Refuted or misdescribed on adversarial review** | **5** |
| **Upheld as stated** | **0** |

Every claim was handed to a second agent instructed to **refute** it, defaulting to refuted
unless the evidence was airtight. **None survived as stated.** Three were refuted outright;
two were misdescribed — a real gap with a wrong account of it.

The dominant failure mode was distinctive and worth recording: **impact inflation on top of
accurate citations**, and **treating written-down scope boundaries as defects**. Two findings
were pre-empted by comments the crate's authors had already written.

**No stubs were found in the C section.** Every host-callback surface the crate installs does
real work — no fixed returns, no ignored state. The gaps there are clean absences.

## Confidence tags

- **[verified]** — re-checked at source independently of the agents.
- **[claimed]** — survived adversarial review, not independently re-checked.

## Real gaps, correctly accounted

### AUv3 units *with views* cannot be instantiated — **[verified]**, misdescribed as "every AUv3"

Only the synchronous `AudioComponentInstanceNew` is bound (`handle.rs:43-46`);
`AudioComponentInstantiate` appears nowhere.

`AudioComponent.h:500-502` supports the spec half: the async form *"**must** be used to
instantiate any component with `kAudioComponentFlag_RequiresAsyncInstantiation` set"* — so
sync instantiation is genuinely invalid for flagged components, not merely discouraged.

But the scope was overstated. `AudioComponent.h:198-201` sets that flag automatically for
*"v3 audio units **with views**"* — not all AUv3. Corroborated independently: an earlier
session read the same rule from Apple's headers and recorded it (memory
`au-host-silent-traps`).

**Correct account:** AUv3-with-views cannot be loaded. The crate is scoped AUv2, so this is a
feature gap for anyone expecting AUv3 support rather than a defect within stated scope.

### A crashed plugin produces silence with no diagnostic — **[verified]**, not the claimed DAW hang

`kAudioComponentInstanceInvalidationNotification` is unregistered — no `CFNotificationCenter`
reference anywhere in the crate.

The claimed impact — *"the host keeps calling `AudioUnitRender` and hangs the whole DAW"* —
is refuted by the architecture, and this is worth recording because it is the kind of claim
that reads as urgent. `AudioUnitRender` never runs on the DAW's audio thread; it runs in the
plugin subprocess (`tutti-plugin-server`). The DAW side waits on a bridge thread with a
bounded timeout — `MAX_PROCESS_TIMEOUT = 50ms` (`ipc_client/audio/dispatch.rs:42`) — and
falls back to silence via a sequence check when no reply arrives
(`dispatch.rs:155-161`: *"the host needs no notification to fall back to silence — the server
never published, so the sequence check fails"*).

**Correct account:** a missing-diagnostic and no-auto-recovery gap. The user gets silence with
no "plugin crashed" notice and no teardown-and-reload. Real, minor, and undocumented — I
found no comment claiming it as deliberate.

## Refuted — recorded so they are not re-raised

**Multi-bus rendering absent** — **REFUTED as a defect; it is a documented, tested-around
scope boundary.** **[verified]** — `AudioUnitRender` does pass literal `0` (`instance.rs:1975`)
and the input callback does discard `_in_bus_number` (`instance.rs:2119`). But
`tests/au_multibus.rs:712-716` states the limit outright: *"Only bus 0 is rendered —
`AuInstance::process` drives the primary bus, and this change adds topology *discovery*, not
per-bus rendering."* The suite asserts topology reads, and `au_multibus.rs:104-106` flags the
assumption as load-bearing: *"The single-bus render path… is correct because of this, so if it
ever stops holding the render path needs revisiting."* The consumer corroborates —
`loaders/au.rs` has zero references to `bus_count` / `bus_layout` / `BusDirection`, so nothing
downstream silently mis-renders. Sidechain input is genuinely unreachable; it is a known
written-down limit, not a discovered bug.

**`MusicDeviceSysEx` / `MusicDeviceMIDIEventList` absent** — **REFUTED; documented.** Both are
absent, and both are declared in the API docs a caller reads. `instance.rs:447-449`: *"Message
families with no legacy 3-byte form (**SysEx**, per-note MIDI 2.0 messages, system real-time)
are **skipped** — AUv2's `MusicDeviceMIDIEvent` only speaks legacy channel voice."* The MIDI
2.0 down-conversion is stated at `identity.rs:68-72`. Note the original claim said SysEx is
"silently down-converted" — it is *dropped*, which is a different fact. Patch-dump SysEx being
unavailable is a genuine limitation, but a declared one.

**`PresentationLatency` never written → "look-ahead limiters drift"** — **REFUTED; the claim
inverts the property.** **[verified]** at `AudioUnitProperties.h:514-531`: it is
`Access: write`, *"set by a host to describe **to the audio unit** the presentation
latency"*, and the header adds explicitly: *"This should not be confused with the Latency
property, where the audio unit describes **to the host** any processing latency it
introduces."* Host-side delay compensation runs off `kAudioUnitProperty_Latency`, which this
crate **does** read (`instance.rs:946-951`). Not writing property 40 cannot cause host-side
drift.

**`ShouldAllocateBuffer` never written** — **REFUTED.** `AudioUnitProperties.h:686-693`:
default is true, and setting false is an optimization for units fed by *connections* rather
than callbacks. This crate uses the callback path, where the API contract expects the unit to
provide the buffer. At most a missed micro-optimization; arguably correct as-is.

## Properties: 38 of 170 exercised, and the DAW-critical set is covered

Of 40 constants referenced, two are declared but never passed to
`AudioUnitGetProperty`/`SetProperty`: `MakeConnection` (`types.rs:169`) and
`MIDIControlMapping` (`types.rs:272`, deliberately unused per `midi_map.rs:42`).

The set that matters for a DAW is present: latency (`instance.rs:950`), tail
(`transport.rs:641`), bypass (`instance.rs:1243`), presets (`ClassInfo`, `FactoryPresets`,
`PresentPreset`), channel layout (`channel_layout.rs:462`), parameter display metadata, and
the full MIDI-mapping quartet (`midi_map.rs:451-619`).

Never-referenced constants a real plugin might plausibly want — recorded without claiming any
is a defect: `AUHostIdentifier` (46), `ParameterIDName` (34), `DependentParameters` (45),
`CPULoad` (6), `SupportsMPE` (58), `RequestViewController` (56), `LastRenderSampleTime` (61),
`RenderContextObserver` (60), and the `3020`–`3024` offline-preflight series. The remaining
~118 are Apple-unit-specific, IAA, AUHAL, or deprecated.

## Host callbacks are real, not stubs

Worth stating positively, since the equivalent CLAP and VST2 sections both found stubs.
`HostCallbacks` installs all four procs (`transport.rs:387-390`) and fills v1 **and** v2 from
the same shared state via `write_transport_common` (`transport.rs:579`) — which is exactly the
surface doc's own §C.2 warning. Every out-param is null-checked (`transport.rs:419`), and
`state_changed` is consume-once and only when the AU asked (`transport.rs:605-607`). The
render-input callback genuinely fills buffers, null-checks `mData`, honours declared capacity,
and is panic-guarded (`instance.rs:2152-2205`).

## What this document is not

A bug list. **Zero of five claims survived as stated**, and that is the finding: an unreviewed
sweep would have produced a document asserting a DAW-hanging crash bug, a broken
latency-compensation path, and two "missing features" the authors had already documented and
tested around.

The two lessons, both paid for here: **verify the direction of a property before asserting an
impact** (`PresentationLatency` is host→plugin, so a missing write cannot cause host-side
drift), and **read the tests before calling something absent** — `au_multibus.rs` names its
own limit in prose.
