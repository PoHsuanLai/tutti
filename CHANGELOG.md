# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **The MIDI delivery traits split on arity.** There are now four, in two pairs,
  one pair per direction — keeping the `In`/`Out` axis `AudioIn`/`AudioOut` set:

  |          | one stream | one of many, by id |
  |----------|------------|--------------------|
  | **push** | `MidiOut`  | `MidiRouter`       |
  | **pull** | `MidiIn`   | `MidiUnitIn` *(new)* |

  `MidiIn` previously spanned both columns, carrying a `unit_id` for the fan-out
  half. Three of its four implementors treated that parameter as dead weight —
  two checked it against an id they already owned, one ignored it — and the
  hardware edge had to be polled with a `MidiUnitId::new(0)` **sentinel**. Since
  `0` is a real id, a per-unit source installed at that seam would silently have
  received the entire hardware stream. That install is now a compile error.

  Migrating an implementor: if it ignored `unit_id`, implement `MidiIn` and drop
  the parameter (`poll_into` → `poll_block`). If it dispatched on it, implement
  `MidiUnitIn` (`poll_into` → `poll_unit`). `MidiInPort::install` and
  `MidiPreBlock::set_input` now take the respective trait.

  `MidiReceiver` no longer implements a read trait at all. Its inherent
  `poll_into(&self, out)` is unchanged and is what every caller already used —
  the trait impl's only added behaviour was a check its sole caller deliberately
  routed around.

- **`MidiOut::queue` returns the accepted count** (`()` → `usize`). A sink that
  cannot fail returns `events.len()`; a short count means the rest were dropped.

  This fixes a live bug: `MidiSession::send` returned `events.len()` whenever an
  output was open, under a doc promising an *accepted* count, so a device
  refusing every event reported full success. Both OS backends now count what
  they wrote and stop at the first failure, so the number names an unbroken
  prefix rather than a subset with holes.

  **The return value's meaning changed without its type changing** — a consumer
  treating `send(&e) == e.len()` as "connected" now correctly gets `false` when
  the device is refusing.

- **`InputConnection` carries no methods.** Its doc said exactly that and then
  declared `fn endpoint()`, which had no callers — the id is already the key of
  the map connections are stored under. The trait remains as the RAII marker its
  doc describes: dropping it closes the port.

- **`tutti-midi-io` is native-UMP; `midir` is gone.** MIDI reaches the wire as
  UMP words on every supported platform, so MIDI-2-only messages — per-note
  controllers, per-note pitch bend, JR Timestamps — survive. They were
  previously dropped in silence by `to_midi1_bytes` returning `None`.
  - macOS: CoreMIDI, no new dependencies.
  - Linux: ALSA UMP sequencer, needs alsa-lib ≥ 1.2.10. `build.rs` probes the
    version and degrades to a stub with a `cargo:warning` below that floor, so
    older distros still build.
  - Windows: a stub. No `windows` crate version binds Windows MIDI Services.
  - JR-stamped output used to be macOS-only, because `UmpOutRes` wrapped a
    CoreMIDI type. It now takes an erased sink, so every backend gets it.

### Added

- **`tutti-midi-file`** — the SMF and MIDI 2.0 Clip File codecs, split out of
  `tutti-midi-io`. Reading a `.mid` needs no OS MIDI port, and pairing the two
  behind one feature flag meant a consumer wanting only the codecs linked
  CoreMIDI unless it knew to pass `default-features = false`. `tutti-midi-io`
  re-exports the file API, so `tutti_midi_io::smf` still resolves.
- `MidiSession` replaces `MidiIo`: same job, no driver code, no background
  threads, and an output sink that is **absent** rather than silently
  discarding when nothing is connected. `send` returns the accepted count.
- `UmpCapability` records what an endpoint can carry — protocol and function
  blocks — read from the OS rather than assumed.

### Removed

- **`tutti-midi-io`'s `midi-hardware` feature.** With the file codecs split out,
  the crate contains nothing that is not OS MIDI, so the flag no longer named an
  axis: turning it off left the whole session/port surface compiled with no
  backend to drive it. Platform gating is `cfg(target_os)`, which is what
  actually decided this all along. `bevy-tutti` keeps its own `midi-hardware` —
  that axis is still real.
- The MIDI-1.0 `VirtualMidiSource` / `VirtualMidiDestination` pair, which had no
  consumers.

### Fixed

- SysEx reassembly, extracted to `tutti-midi-io`'s `Sysex7Assembler` and now
  unit-testable off the driver thread. Three bugs it had been hiding: bytes
  after the terminating `0xF7` were discarded (a device packing two dumps into
  one buffer lost the second); the buffer had no ceiling, so a lost `0xF7` grew
  it for the lifetime of the connection; and each completed message allocated
  twice on the driver callback thread. A mid-run `0xF0` now restarts the run
  rather than being kept as payload, where the 7-bit mask silently turned it
  into `0x70`.

## [0.0.1] - 2025-01-29

### Added
- Initial release of Tutti audio engine
- Core audio graph runtime with FunDSP integration
- MIDI subsystem with I/O, MPE, and MIDI 2.0 support
- Sample playback with Butler thread and time-stretch
- DSP building blocks: LFO, dynamics, envelope followers, spatial audio
- Plugin hosting for VST2, VST3, and CLAP (multi-process with crash isolation)
- Neural audio synthesis and effects (GPU-accelerated)
- Audio analysis tools: waveform, transient detection, pitch detection
- Offline audio export (WAV, FLAC)
- Real-time transport with tempo mapping
- EBU R128 LUFS metering
- Plugin Delay Compensation (PDC)
- Modular feature flags for flexible builds
- Ergonomic graph API with `pipe()`, `node_mut()`, `add_split()`, `add_join()`
- 9 comprehensive examples showcasing core features

### Architecture
- Workspace with 8 independent crates
- Lock-free audio thread design
- Framework-agnostic (works without Bevy/egui)
- MIT OR Apache-2.0 dual license

[Unreleased]: https://github.com/PoHsuanLai/Tutti/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/PoHsuanLai/Tutti/releases/tag/v0.0.1
