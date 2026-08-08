# Plugin-format interface surfaces

Four reference documents, one per hosted plugin format, each extracted **from that
format's own spec** rather than from our code. They describe the formats, not our
coverage of them — which is what makes them usable as a checklist to grade our
implementation against, instead of a mirror of the gaps we already have.

Extracted 2026-08-03.

| Format | Document | Source | Authority |
|---|---|---|---|
| **CLAP 1.2.10** | [clap-1.2.10-interface-surface.md](clap-1.2.10-interface-surface.md) | `free-audio/clap` C headers (MIT) | vendor-authoritative |
| **VST3 3.8.0** | [vst3-3.8.0-interface-surface.md](vst3-3.8.0-interface-surface.md) | Steinberg VST3 SDK (MIT) | vendor-authoritative |
| **AU (AUv2 C API)** | [au-v2-interface-surface.md](au-v2-interface-surface.md) | Apple AudioToolbox SDK headers | vendor-authoritative |
| **VST 2.4** | [vst2.4-interface-surface.md](vst2.4-interface-surface.md) | vendored `vst-rs` bindings | **third-party transcription** |

## Scale

| Format | Surface |
|---|---|
| CLAP | 27 stable + 18 draft extensions → **51 distinct extension IDs**, 4 factories, 13 event types |
| VST3 | **76 interface classes** across 31 headers; ~30 optional plugin-side, 12 `RestartFlags` |
| AU | **~150 properties**, 29 error codes, 8 scopes, 28 parameter units |
| VST2 | **80 plugin opcodes** + **49 host opcodes** |

The headline: for VST3 the "famous five" (`IComponent`, `IAudioProcessor`,
`IEditController`, `IPlugView`, `IComponentHandler`) are under 10% of the surface.
Same for AU's property list. Assume uncovered ground until a cross-reference says
otherwise.

## Signatures are inlined, verbatim

Every entry carries its **full signature copied from the spec**, not just a method
name — that is what makes these implementable rather than merely a checklist. Each
format keeps its own native spelling, because the spelling *is* part of the contract:

| Format | What a row carries |
|---|---|
| CLAP | the C function-pointer declaration, `uint32_t(CLAP_ABI *count)(const clap_plugin_t *plugin, bool is_input)`, plus its `[main-thread]` / `[audio-thread]` annotation |
| VST3 | the C++ declaration including `PLUGIN_API` and the SDK's inline `/*in*/` `/*out*/` direction comments |
| AU | the full C prototype with Apple's nullability annotations and `CA_REALTIME_API` markers, plus Scope / Value Type / Access per property |
| VST2 | per-opcode meanings for `index`, `value`, `ptr`, `opt` and the return — since all 129 opcodes share one C signature, the *arguments* are the real interface |

Struct field lists are inlined the same way for anything a host must fill or read
(`clap_process`, `ProcessData`, `AudioUnitParameterInfo`, `AEffect`, `TimeInfo`, …).

## Coverage grading

The spec documents above describe the **formats**. Grading our implementation against them
is a separate pass, deliberately run after the specs were extracted so a checklist could not
simply mirror what we already do.

| Format | Coverage doc | Status |
|---|---|---|
| VST3 | [vst3-coverage.md](vst3-coverage.md) | graded — 37 interfaces contract-verified, 34 absences classified, 16 findings upheld across three passes; **all 16 fixed** |
| CLAP | [clap-coverage.md](clap-coverage.md) | graded — 28 stable extension IDs, **0 absent** (24 bound, 4 partial); 5 claims → **1 upheld**, since fixed |
| AU | [au-coverage.md](au-coverage.md) | graded — 47 entry points, 170 properties, 13 callback surfaces; 5 claims → **0 upheld as stated**, 2 misdescribed |
| VST2 | [vst2-coverage.md](vst2-coverage.md) | graded — 59 live + 31 live audioMaster opcodes; 7 claims → **1 upheld**, since fixed |

The old Phase-1 numbers (CLAP 27/27, AU 40/107, VST2 35/80) were **symbol-presence counts only** and overstate coverage: VST3's
raw count scored `IUnitInfo` as covered when it appears solely in doc comments. Presence
also says nothing about correctness — AU's `HostCallbacks` would score covered and still
lose transport silently.

## Read the VST2 row differently

Three of these come from the vendor. **VST2 does not, and cannot** — Steinberg
withdrew the SDK, and no `aeffect.h` / `aeffectx.h` exists on this machine. JUCE
does not ship one either; it `#include`s `<pluginterfaces/vst2.x/aeffect.h>` from an
SDK it expects you to supply. So the vendored `vst-rs` bindings *are* our spec.

Consequence: for the other three, a transcription error shows up as a mismatch
against the vendor header. For VST2 nothing would ever contradict it. A gap in that
document means **"our transcription may be wrong"**, not "the format lacks it".

Reading it exhaustively already found three defects, one of them a live crash —
`MidiEventFlags::from_bits().unwrap()` panicking the host on a legal MIDI event
(fixed in PR #131). Treat further findings there as plausible bugs, not as spec.

## Traps worth knowing before implementing

Each of these fails **silently** — the call succeeds and the wrong thing happens.

**CLAP.** Thread annotations are the contract, and this is why the C headers were
used rather than a Rust binding: `clap-sys` strips every doc comment, losing all
123 `[main-thread]` and 25 `[thread-safe]` markers. Nine sites are genuinely
unannotated — most notably `clap_plugin_thread_pool::exec`, whose realtime nature is
implied only by prose. `events.h` carries no annotations at all. Compound forms gate
on *state*, not just thread: `[active ? audio-thread : main-thread]`,
`[main-thread & !active]`. The `_COMPAT` alias IDs are not uniformly formatted
(`clap.configurable-audio-ports.draft1` has a dot and no slash) — hardcode them,
never derive.

**VST3.** `IMidiMapping2` and `IMidiLearn2` (both 3.8.0) carry `[replaces …]` tags:
a MIDI 2.0 host queries the `2` variant **first** and falls back, advertising support
via `IPlugInterfaceSupport`. Neither is marked deprecated, so the v1 path still
works — the gap is that MIDI 2.0 controllers are simply unreachable through it.
`kParamIDMappingChanged` (`1<<11`) fires during *project load*; ignoring it loses
automation on plugin replacement. `ParamID`'s valid range is `[0, 0x7FFFFFFF]` — the
top half is reserved for the host.

**AU.** `AudioUnitParameterInfo.name[52]` is documented `"UNUSED - set to zero"`; the
real name is `cfNameString`, gated on `..._HasCFNameString` and requiring `CFRelease`
when `..._CFNameRelease` is set. `kAudioComponentFlag_RequiresAsyncInstantiation`
makes `AudioComponentInstanceNew` invalid — set automatically for v3 units with
views, so a synchronous-only host cannot load that whole class. `HostCallbackInfo`
declares `transportStateProc` and `transportStateProc2` as **separate nullable
pointers**, not a versioned union: fill both. Latency and tail are dynamic `Float64`
*seconds*, needing property listeners rather than a load-time read.

**VST2.** `effGetTailSize` is inverted against every other format — `0` means
"unknown/default" and `1` means "no tail". Boolean returns have no single rule
(`> 0` for some opcodes, deliberately `== 1` for others, because plugins return `-1`).
Argument placement is inconsistent by design: `effSetSampleRate` uses `opt` while
`effSetBlockSize` uses `value`.

## What these documents are not

They record **what each format defines**. They say nothing about what we implement —
that cross-reference is a separate pass, and it is only trustworthy because these
were built blind to our code. Do not edit them to match our implementation; if one
disagrees with the vendor header, the header wins and the document is the bug.
