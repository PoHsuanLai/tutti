# tutti-plugin-types

The shared value vocabulary underneath Tutti's four plugin-host crates.

## What this is

Everything a VST2 / VST3 / CLAP / AU host needs to speak that is *not* specific
to one format: `PluginDescriptor` and `LoadedPlugin` (identity and the
load-time wiring snapshot), `ParameterInfo` with its `ParamAddress` / `ParamRange`
/ `ParamFlags` parts, `ProcessContext` and `ProcessOutput`, `TransportInfo`,
`ParameterChanges` automation queues, note expression and harmony changes,
`AudioBuffer` in its f32/f64 forms, editor vocabulary (`WindowHandle`,
`EditorSize`, `EditorCapabilities`), presets, and the `Features` capability
bitset.

It also carries `format_host`: the fine-grained capability traits
(`PluginMeta`, `PluginAudio`, `PluginParams`, `PluginState`, `PluginPresets`,
`PluginEditorHost`) that each format loader implements, reassembled by the
blanket-impl `PluginInstance`.

## Why it is its own crate

**It sits under all four format hosts, and each re-exports it.**
`tutti-vst2-host`, `tutti-vst3-host`, `tutti-clap-host` and `tutti-au-host` all
publish these types from their own public API, so a caller can stay
format-agnostic whenever only the shared surface is in play. Without a common
home, the same `TransportInfo` would exist four times and a host would translate
between four dialects of it.

The shape here is designed to keep formats' disagreements **representable rather
than averaged away**: `ParamRange` is a sum type because there is no safe number
to substitute when a format declares no range; `ParamSteps` separates
"continuous" from "nobody said", which a bare `step_count: u32` fuses at zero;
`ParamFlags` is paired with a `known` mask so a flag nobody reported reads as
`None` rather than `false`.

Representable is not the same as unavoidable, though. A consumer should read a
parameter through `ParameterInfo`'s own accessors (`bounds`, `default_value`,
`step_count`, `flag`, `to_plain`) — the variants exist for the host that must
produce them, not as the reading path.

## Where it sits

Depends on `tutti-types` (`Samples`, `ChannelLayout`, `ChannelTopology`, the
meter vocabulary) and `tutti-midi-types` (`MidiEvent`), re-exporting the parts
format crates need so none of them takes its own dependency for the type names.
All four format hosts and `tutti-plugin` depend on it.

## Features

`default = []`.

- `serde` — `Serialize`/`Deserialize` on the wire-eligible types
  (`TransportInfo`, `ParameterChanges`, `EditorSize`, `EditorCapabilities`,
  `WindowHandle`, and the metadata that rides the plugin-load reply). Required
  by `tutti-plugin`'s bincode IPC protocol; a host crate that does no IPC does
  not pay for the dependency.

## License

MIT OR Apache-2.0
