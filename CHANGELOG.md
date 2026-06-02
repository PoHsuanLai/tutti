# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
