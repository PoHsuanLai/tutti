# Tutti Core

Audio graph runtime and transport management.

## What this is

Vocabulary crate for DAW-oriented audio processing. Built on FunDSP's `Net` for the audio graph, with transport control (tempo, time signatures, BBT positioning), real-time level metering, MIDI routing, and plugin delay compensation. Uses atomic types (`AtomicFloat`, `AtomicDouble`) for UI-thread parameter updates.

**Most users should use the [`tutti`] umbrella crate instead** — it exposes a single `TuttiEngine` struct that owns the graph, CPAL audio I/O, and all optional subsystems in one place. `tutti-core` is the building block, not the entry point.

## What's here

- [`TuttiNet`] — thin facade over `fundsp::net::Net` with typed downcasts and auto-PDC on `commit()`
- [`TransportManager`] / [`TransportHandle`] — playback control (play/stop/seek/loop)
- [`MeteringManager`] — level, LUFS, correlation, CPU meters
- [`PdcManager`] — plugin delay compensation state
- [`MidiBus`] — MIDI event fan-out to audio-node inboxes (feature-gated)
- [`Engine`] — the RT callback graph render (MIDI-free; delivery is `MidiPreBlock` in tutti-midi-runtime)
- FunDSP re-exports (`AudioUnit`, `Net`, `Wave`, …)

## Features

- `default` — Core functionality
- `std` — Enable `std`-based debug assertions in `AudioThreadCell`
- `midi` — MIDI event types, registry, routing
- Audio formats: `wav`, `flac`, `mp3`, `ogg` (for loading `Wave` from disk)

## License

MIT OR Apache-2.0

[`tutti`]: https://crates.io/crates/tutti
