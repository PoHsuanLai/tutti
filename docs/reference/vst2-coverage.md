# VST 2.4 format-layer coverage — `tutti-vst2-host`

Graded 2026-08-03 against [vst2.4-interface-surface.md](vst2.4-interface-surface.md).

**Read this row differently from its siblings.** Three of the four formats have a vendor
spec. VST2 does not and cannot — Steinberg withdrew the SDK, no `aeffect.h` / `aeffectx.h`
exists on this machine, and JUCE does not ship one either. **The vendored `vst-rs` bindings
*are* our spec**, so a "gap" here is measured against bindings someone else wrote, not
against Steinberg's intent. Two findings below turn on a binding bug rather than on ours.

**Scope: the format layer only** — `crates/plugin/formats/tutti-vst2-host/`
plus `crates/plugin/vendor/vst-tutti/`.

## Method

One survey pass, one adversarial refutation pass, plus independent re-checks at source.

| | Count |
|---|---|
| Host→plugin opcodes (`eff*`) | **80** (59 live, 21 deprecated slot-holders) |
| Plugin→host opcodes (`audioMaster*`) | **49** (31 live, 18 deprecated) |
| Live host→plugin opcodes dispatched | **27** |
| Live `audioMaster` handled with real data | **11** |
| Live `audioMaster` falling through `_ => 0` | **13** |
| Claims raised | 7 |
| **Refuted or misdescribed on adversarial review** | **6** |
| **Upheld** | **1** |

**One of seven survived.** The dominant failure mode was specific and instructive: three
findings correctly observed *"a vendor wrapper exists and nobody calls it"* and then assumed a
starved consumer — without checking `tutti-plugin-types/src/format_host.rs`, where the
capability is **not modeled for any format**. Absence of a call is not absence of a
capability if nothing upstream could ask for it.

## Confidence tags

- **[verified]** — re-checked at source independently of the agents.
- **[claimed]** — survived adversarial review, not independently re-checked.

## Upheld findings

### `audioMasterUpdateDisplay` (42) is never decoded — **[verified] · FIXED on main**

> **Resolved.** `host_dispatch` has an `OpCode::UpdateDisplay` arm routing to
> `Host::update_display`. Main went further than this survey proposed and also
> added `CurrentId` and `GetLanguage` arms. The analysis below is kept as the
> record of why — line numbers refer to the code as it was when surveyed.

Every `OpCode::` arm in `host_dispatch` (`vendor/vst-tutti/src/interfaces.rs:386-434`) was
enumerated: `Version`, `Automate`, `BeginEdit`, `EndEdit`, `Idle`, `CanDo`,
`GetVendorVersion`, `GetVendorString`, `GetProductString`, `ProcessEvents`, `GetTime`,
`GetBlockSize`, `GetSampleRate`, `SizeWindow`, `IOChanged`, `GetInputLatency`,
`GetOutputLatency`, `GetCurrentProcessLevel`, `GetAutomationState`. **There is no
`UpdateDisplay` arm**; it falls to `_ => 0` at `:436`.

The binding is asymmetric: the plugin side *sends* it (`vendor/vst-tutti/src/plugin.rs:1115-1118`),
the host side never receives it. The surface doc already names this
(`vst2.4-interface-surface.md:225-228`): *"That is a bug in the vendored bindings, not in the
format."*

**Unlike the refuted findings, the sink exists.** `AsyncEvent::ParamValuesChanged`
(`tutti-plugin-server/src/plugin.rs:64-66`) is documented as exactly this signal — *"Plugin
changed its own parameter values at runtime (e.g. preset load). The client should re-read
parameter values."* It is fully plumbed for VST3: `plugin.rs:248` → `session.rs:212` →
`BridgeMessage::PluginParamValuesChanged` → handled at `ipc_client/audio/dispatch.rs:232`.
VST2's arm (`plugin.rs:235-237`) emits only `ParameterChanged` from the `automate` channel.

**What breaks:** when a VST2 plugin changes many parameters internally without per-parameter
`audioMasterAutomate` — the classic case being its own preset menu — the host's parameter view
goes stale. A VST3 plugin in the identical situation refreshes.

## Refuted — recorded so they are not re-raised

**`audioMasterCanDo` "answers no to everything"** — **MISDESCRIBED; the claim inverts the
tri-state.** **[verified]** at `vst2.4-interface-surface.md:128`: the VST2 canDo convention is
**`1 = yes, 0 = maybe/don't know, -1 = no`**. Falling through to `0`
(`interfaces.rs:394-396`, which logs then returns nothing) means *"don't know"*, not *"no"*.
The consequential claim — that plugins stop calling `audioMasterGetTime` — is unsupported:
`GetTime` is unconditionally serviced at `interfaces.rs:409-423` with a real `TimeInfo`.
**What survives as a genuine gap:** the host cannot *affirmatively* advertise
`sendVstMidiEvent`, `receiveVstMidiEvent`, `sizeWindow` etc. Worth fixing; not what the
headline said.

**"No `BeginEdit`/`EndEdit` gesture brackets"** — **MISDESCRIBED; the dispatch exists.**
**[verified]** at `interfaces.rs:388-389`: both opcodes are decoded and routed to
`host.begin_edit(index)` / `host.end_edit(index)`. What is true is narrower: `HostState`
(`src/host.rs`) overrides twelve `Host` methods but not these two, so they inherit the no-op
default (`vendor/vst-tutti/src/host.rs:199-202`) and the gestures are dropped. **But the
claimed break — "automation touch/latch recording cannot work" — requires a consumer with a
gesture concept, and there is none.** `AsyncEvent`
(`tutti-plugin-server/src/plugin.rs:46-76`) has `ParameterChanged`, `LatencyChanged`,
`TailChanged`, `ParamValuesChanged`, `ParamTitlesChanged`, `IoChanged`, `Reloaded` — **no
gesture variant at all.** Even VST3, which *does* capture gestures
(`com/component_handler.rs:103,108`), has nowhere to forward them.

**"No preset/program access at all"** — **MISDESCRIBED.** The absence is real: wrappers exist
at `vendor/vst-tutti/src/host.rs:1292-1312` and nothing in the crate calls them. But
*"a user cannot select a factory preset"* requires a consumer that would call it.
`PluginFormatHost` models state as **opaque chunks only** (`get_state`/`set_state`) — there is
no `select_preset`, `preset_count`, or `preset_name` on any of its traits. **[verified]:**
VST3 (`loaded.rs:709,767,795`), CLAP (`instance/state.rs:140`), and AU
(`instance.rs:1088,1165`) all implement presets at the format layer and none can expose them
either. The format-level asymmetry is real — VST2 is the only one that cannot *enumerate*
presets — but it terminates at a boundary that models presets as chunks for everyone.

**"`effGetParameterDisplay` never called → params read as raw floats"** — **REFUTED on
impact.** The absence is true (zero hits for `get_parameter_text` across the crate, the
server, and `tutti-plugin`). But no display-text surface exists at the boundary for *any*
format — grep for `value_to_text|param_text|display_value` across `tutti-plugin-types/src/`
and every loader returns nothing, even though CLAP binds `value_to_text`. The host also does
better than the claim allows: `parameter_list` reads `effGetParameterProperties` (opcode 56)
for real integer ranges and step counts, so a host-side formatter can render plain values
from `unit` + `ParamRange::Plain` today.

**"`effGetTailSize` never called → reverb tails truncated"** — **MISDESCRIBED, and the
truncation is provably prevented.** The gap is real and the loader already documents it
(`loaders/vst2.rs:80-92`). But the same comment explains the deliberate choice:
*"`Unknown`, not `None`: nothing here has asked. Claiming `None` would tell a bounce to add
nothing, which is wrong for every VST2 reverb."* The tail algebra honours that —
`tutti-types/src/tail.rs:209-212` counts `Unknown` nodes separately and `GraphTail::samples`
(`:101-102`) **refuses to return a frame count** when any node is unknown, forcing an explicit
caller decision. CLAP reports `Unknown` too (`loaders/clap.rs:188`), so this is not
VST2-specific. **Trap for whoever wires it:** VST2 inverts the encoding — `0` = unknown,
`1` = no tail (`vst2.4-interface-surface.md:616-628`). A naive `0 => None` mapping would
*introduce* the very truncation this finding alleged.

**"`process_f64` is a lie by omission"** — **REFUTED.** The downcast is real
(`src/scratch.rs:75-90`, `:121`) but disclosed at four layers: the method's own doc comment
(`src/process.rs:47-50`), the module header (`scratch.rs:9-11`), **the consumer**
(`loaders/vst2.rs:57-60` — *"VST2's advertised f64 is informational only"*), and the surface
doc (`:789-791`). `supports_f64` reports what the plugin *declares* via
`CAN_DOUBLE_REPLACING`, which is what it says it is.

## Is the crate small because VST2 is small, or because coverage is thin?

Both, but coverage is the larger term.

**Genuinely a smaller format:** 59 live host→plugin opcodes with no per-bus configuration, no
note expression, no polyphonic modulation. Parameter values bypass the dispatcher entirely
(two function pointers). Editor and processor fuse into one `AEffect` (`src/lib.rs:16-22`),
removing an axis the siblings must model. And **12 of 59 live opcodes are unimplementable at
any effort** because the vendored bindings never define the structs
(`vst2.4-interface-surface.md:813-818`) — ~20% struck off before any code is written.

**But thinner than the format warrants:** 27 of 47 reachable live opcodes = **57%**. Nine
unreached opcodes already have working wrappers and cost one call each. Test ratio is
0.57 test-lines per src-line against CLAP's 0.78 and AU's 1.28.

Where it is strong it is genuinely strong: the `effCanDo` tri-state handling
(`src/instance.rs:171-184`), the `effGetCurrentMidiProgram` zero-ambiguity guard
(`src/param_properties.rs:444-481`), the `TransportCell` seqlock replacing an allocating
`ArcSwap` on the audio thread, and the chunk-failure-vs-empty-chunk distinction
(`src/state.rs:102-119`) all match the surface doc's stated traps precisely.

## Biggest untested area

**The entire plugin→host callback direction.** `src/host.rs` has two tests, both trivial
getters. Nothing asserts what a plugin *hears* for any opcode. Most concerning:
`process_events` inbound decoding (`src/host.rs:92-106`) is **`unsafe` pointer arithmetic
walking a flexible array past its declared bound of 2** (`vst2.4-interface-surface.md:345`)
with zero direct tests — the probe tests exercise MIDI *out* through the wrapper, not this
function's array walk.

## What this document is not

A bug list. One of seven claims survived. The recurring error — and the reason the refutation
pass earns its cost — was **stopping at the vendor-wrapper boundary**: observing that a
wrapper exists and nobody calls it, then inferring a broken user-facing feature without
checking whether anything upstream models the capability at all.
