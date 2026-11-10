# tutti-vst2-author

The plugin-authoring half of the old `vst-tutti` crate: `plugin_main!`, `main` (which builds
the `AEffect`), the plugin-side dispatcher and the `util` helpers. Test-only
(`publish = false`): the VST2 test plugins build on it. It re-exports `tutti-vst2-ffi`'s ABI
modules, so a plugin names `vst::api` and `vst::plugin::Plugin` from one place.

Derived from vst-rs 0.3.0 (MIT); see `LICENSE`.
