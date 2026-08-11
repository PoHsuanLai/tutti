# tutti-au-host

Audio Unit (AUv2) plugin hosting for macOS, via Apple's AudioToolbox framework.

## What this is

Low-level bindings plus the host machinery around them. `AuInstance` is the
entry point — load → initialize → render — and it tracks initialization state so
`process()` is only reachable once the AU is ready. Around it:

- `component` — enumerating installed AUs by `AuType`.
- `bus` / `stream` / `channel_layout` / `topology` — bus counts, stream formats,
  Apple layout tags, and the tag ↔ `ChannelTopology` conversion.
- `parameters` / `listener` — parameter query and set, plus change listeners.
- `preset` / `aupreset` — factory presets, and `.aupreset` file I/O (the
  interchange format Logic, Live, Reaper and GarageBand all read and write).
- `midi_map` / `midi_out` — MIDI in and the output-callback registration.
- `transport` / `render_notify` — host callbacks the AU pulls during render.
- `offline` — bounce-time facilities a live-only host never needs and an
  exporting host cannot do without: offline render mode, in-place processing,
  and the push-model render path.
- `editor` — the Cocoa view factory and editor lifecycle.

## Why it is its own crate

One crate per plugin format, all four sharing
[`tutti-plugin-types`](../../tutti-plugin-types)'s vocabulary, so
[`tutti-plugin-server`](../../tutti-plugin-server) can gate each behind its own
feature and a non-macOS build never touches AudioToolbox.

The platform boundary is the sharper reason here. **This crate is macOS-only.**
It compiles on other platforms but exposes no functionality: nearly every module
is `#[cfg(target_os = "macos")]`, and `topology.rs` carries an inner
`#![cfg(target_os = "macos")]`, which is why its re-exports are gated to match —
an ungated re-export would name items that do not exist and fail to build on
Linux.

## Where it sits

Depends on `tutti-plugin-types`, `tutti-midi-types` and `tutti-types`, plus
`core-foundation`, `coreaudio-sys` and `objc2` on macOS. `tutti-plugin-server`
and `tutti-plugin` depend on it behind an `au` feature.

It re-exports `EditorSize`, `MidiEvent`, `TransportInfo` and `WindowHandle` from
`tutti-plugin-types`, and `Samples` / `Seconds` from `tutti-types` — the latter
because `get_latency` and `get_tail_time` hand those back, and a consumer that
cannot name a returned type cannot bind it.

## Features

None. Platform gating is `cfg`, not a feature.

## Channel order is keyed on the tag, never on the count

The discipline `topology.rs` exists to enforce, and worth knowing before touching
any layout code. Apple defines **four different orders of the identical six
speakers** (`MPEG_5_1_A` through `_D`), plus `Emagic_Default_7_1` and `WAVE_7_1`
as further orders of one eight-speaker set. A channel count cannot distinguish
any of them, so nothing may branch on width.

Not hypothetical: `AudioUnit_5_1` is `L R C LFE Ls Rs` while `AudioUnit_5_0` is
`L R Ls Rs C`, so the same index means centre in one and a surround in the other.
Separately, Apple's `Ls`/`Rs` are the engine's SMPTE `SL`/`SR`, and `Rls`/`Rrs`
are `BL`/`BR` — mapping by name rather than by channel swaps a 5.1 bus's
surrounds into its 7.1 rear slots.

## Testing

The Cocoa editor-lifecycle tests run on the main thread — `harness = false` on
the `au_gui_lifecycle_main` target so it owns `main()`, which is the only way to
reach the main thread under cargo and is what AppKit requires.

## License

MIT OR Apache-2.0
