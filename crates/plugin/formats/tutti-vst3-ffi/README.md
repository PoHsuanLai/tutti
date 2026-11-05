# tutti-vst3-ffi

The unsafe half of the VST3 host. Every `unsafe` the host needs to talk to a
VST3 plugin lives here, behind safe handles over the [`vst3`] crate's COM
bindings; the host crate above it, `tutti-vst3-host`, holds the logic (the
lifecycle, the shell, the MIDI mapping, the edit ring).

(Work in progress: this crate is being filled in by a stack of changes.)

[`vst3`]: https://docs.rs/vst3
