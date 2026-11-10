# tutti-vst2-ffi

The host half of Tutti's fork of [`vst-rs`](https://github.com/RustAudio/vst-rs) 0.3.0, and
the one crate that holds every VST2 `unsafe` (`scripts/check-plugin-ffi.sh`).

It holds the `AEffect` and the ABI types, `PluginLoader` and `PluginInstance`, the
`audioMaster` callback, `HostBuffer`, `RenderHandle`, the MIDI event buffers and the render
scratch. `tutti-vst2-host` drives it and forbids `unsafe`.

The plugin-authoring half of the old `vst-tutti` crate (`plugin_main!` and the plugin-side
dispatcher) is `testing/tutti-vst2-author`, which only the VST2 test plugins use.

Derived from vst-rs (MIT); see `LICENSE` and `CHANGELOG.md` for the upstream history.
