# tutti-vst2-host

VST2 plugin hosting — audio, MIDI, parameters, state, and the native editor.

## What this is

Loads VST2 plugins (`.vst`, `.dll`, `.so`), drives audio + MIDI processing,
exposes parameters and state save/restore, and embeds the plugin's native editor
into a host-supplied window. Mirrors the architecture of its `tutti-vst3-host`,
`tutti-clap-host` and `tutti-au-host` siblings.

Built on the vendored `vst-tutti` fork of the [`vst`](https://docs.rs/vst)
crate, which handles the AEffect-level FFI and adds back the host-side
`audioMaster` callbacks upstream swallowed. This crate adds what a host actually
needs on top: pre-allocated render scratch buffers, the MIDI codec, callback
wiring, transport-info bookkeeping, state save/restore, and editor lifecycle.

## Why it is its own crate

One per format, all four under the same shared vocabulary — see
[`tutti-plugin-types`](../../tutti-plugin-types). Splitting them means a build
that only wants CLAP does not compile a VST2 SDK, and
[`tutti-plugin-server`](../../tutti-plugin-server) gates each behind its own
feature.

**VST2 is also the odd one out in two ways that matter.**

First, it is the **positional-index** format. `ParamAddress` exists precisely to
keep that distinction in the type system: `Opaque(ParamId)` is a plugin-chosen
handle (VST3 `ParamID`, CLAP `clap_id`, AU `AudioUnitParameterID`), while
`Index(i32)` is a dense position in `[0, numParams)` — **VST2 only**. The two
models do not silently interconvert, so a bare number cannot become an address
and an address cannot collapse back to one.

Second, VST2 **fuses the editor and the audio processor into a single `AEffect`
instance**. You cannot host the editor in one process and audio in another
against the same plugin, so callers must accept in-process hosting. Subprocess
isolation, where useful, is the caller's job — that is what
`tutti-plugin-server` does by wrapping this crate, keeping a crashing VST2 from
killing the host.

## Where it sits

Depends on `tutti-plugin-types`, `tutti-midi-types` and the vendored
`vst-tutti`. `tutti-plugin-server` and `tutti-plugin` depend on it, both behind
a `vst2` feature.

## Features

None. The crate is the VST2 loader.

## Parameter and MIDI metadata

`ParameterProperties` (`effGetParameterProperties`) plus the MIDI-metadata family
(`MidiProgram`, `MidiKeyName`, `MidiProgramCategory`) is the **whole** of VST2's
parameter/MIDI metadata surface. In particular VST2 has no CC→parameter mapping
query at all; the module docs carry the opcode evidence.

## Testing

`tutti-vst2-test-plugin` is a dev-dependency, so `cargo test` builds a real
reference plugin's cdylib in the same invocation and the conformance harness
loads it. RT-safety regressions run under a disabled global allocator, matching
the wiring the CLAP and VST3 hosts use.

## License

MIT OR Apache-2.0
