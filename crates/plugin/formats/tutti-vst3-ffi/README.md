# tutti-vst3-ffi

The unsafe half of the VST3 host. Every `unsafe` the host needs to talk to a
VST3 plugin lives here, behind safe handles over the [`vst3`] crate's COM
bindings; the host crate above it holds the logic (the lifecycle, the shell, the
MIDI mapping, the edit ring).

This crate currently holds the interfaces the host implements for the plugin
(streams, event lists, parameter changes, the component handler, the host
application, messages, the plug frame, the run loop), the panic guard and the
SDK value vocabulary. It names nothing of tutti's except the value crates
`tutti-types`, `tutti-plugin-types` and `tutti-midi-types`.

[`vst3`]: https://docs.rs/vst3
