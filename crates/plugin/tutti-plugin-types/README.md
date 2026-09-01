# tutti-plugin-types

The shared value vocabulary underneath Tutti's four plugin-host crates.

## What this is

Everything a VST2 / VST3 / CLAP / AU host needs to speak that is *not* specific
to one format: [`PluginDescriptor`] and [`LoadedPlugin`] (identity and the
load-time wiring snapshot), [`ParameterInfo`] with its [`ParamAddress`] /
[`ParamRange`] / [`ParamFlags`] parts, [`ProcessContext`] and [`ProcessOutput`],
[`TransportInfo`], [`ParameterChanges`] automation queues, note expression and
harmony changes, [`AudioBuffer`] in its f32/f64 forms, editor vocabulary
([`WindowHandle`], [`EditorSize`], [`EditorCapabilities`]), presets, and the
[`Features`] capability bitset.

It also carries `format_host`: the fine-grained capability traits
([`PluginMeta`], [`PluginAudio`], [`PluginParams`], [`PluginState`],
[`PluginPresets`], [`PluginEditorHost`]) that each format loader implements,
reassembled by the blanket-impl [`PluginInstance`].

## What it does not own

**Any lifecycle.** These traits describe a plugin as a set of *capabilities* and
deliberately say nothing about what state it is in. That is what lets each format
crate model its own lifecycle as tightly as its own contract allows; the
comparative account is in `tutti-plugin`'s crate docs under *The plugin state
machine*.

**Any FFI.** Nothing here links a plugin SDK. The format crates under
`../formats/` do that, and each re-exports these types from its own public API so
a caller can stay format-agnostic whenever only the shared surface is in play.

## Why it is its own crate

**It sits under all four format hosts, and each re-exports it.** Without a common
home, the same `TransportInfo` would exist four times and a host would translate
between four dialects of it.

The shape here is designed to keep formats' disagreements **representable rather
than averaged away**: [`ParamRange`] is a sum type because there is no safe number
to substitute when a format declares no range; [`ParamSteps`] separates
"continuous" from "nobody said", which a bare `step_count: u32` fuses at zero;
[`ParamFlags`] is paired with a `known` mask so a flag nobody reported reads as
`None` rather than `false`.

Representable is not the same as unavoidable, though. A consumer should read a
parameter through [`ParameterInfo`]'s own accessors ([`bounds`], [`default_value`],
[`step_count`], [`flag`], [`to_plain`]) — the variants exist for the host that
must produce them, not as the reading path.

## Example

Two format hosts describe one parameter each. The shapes differ where the ABIs
differ, and the reading path is the same for both:

```rust
use tutti_plugin_types::{
    Normalized, ParamAddress, ParamFlags, ParamId, ParamSteps, ParameterInfo,
};

// VST3/CLAP/AU: a plugin-chosen handle, often a hash of the name. Not an
// index, not dense, not ordered.
let cutoff = ParameterInfo::new(ParamId::new(0x8000_0001), "Cutoff")
    .with_plain_range(20.0, 20_000.0, 440.0)
    .with_steps(ParamSteps::Continuous)
    .with_flags(ParamFlags::AUTOMATABLE, ParamFlags::AUTOMATABLE);

// VST2 alone: a dense position in `[0, numParams)`, and a format that
// declined `effGetParameterProperties` — so it declared no range at all.
let mix = ParameterInfo::new(ParamAddress::Index(3), "Mix")
    .with_normalized_default(0.5);

// An absent declaration is `None`, never a substituted number.
assert_eq!(cutoff.bounds(), Some((20.0, 20_000.0)));
assert_eq!(mix.bounds(), None);
assert_eq!(mix.step_count(), None); // `Unknown`, not "continuous"
assert_eq!(mix.flag(ParamFlags::AUTOMATABLE), None); // unreported, not `false`

// Arithmetic on the number means something for one model and nothing for
// the other, so the address is asked rather than cast.
assert_eq!(cutoff.id.index(), None);
assert_eq!(mix.id.index(), Some(3));

// The seam speaks normalized `0..=1`; `to_plain` maps onto the declared range.
assert_eq!(cutoff.to_plain(Normalized::new(1.0).get()), 20_000.0);
```

## The clamp in `Normalized::new` is silent

It saturates out-of-range input at the nearest bound and maps NaN to `0.0`,
returning no error and logging nothing. Encode ordering data — or any plain value
— as a `Normalized` and every entry above `1.0` becomes `1.0`, which reads
downstream as a legitimate sweep pinned at maximum rather than as bad input.
Normalize before constructing; this type will not tell you that you did not:

```rust
use tutti_plugin_types::Normalized;

// A plain 20 kHz cutoff, handed to the normalized seam.
assert_eq!(Normalized::new(20_000.0).get(), 1.0);

// Three distinct positions collapse onto one value, in silence.
let ranks: Vec<f64> = [7.0, 42.0, 79.0]
    .into_iter()
    .map(|r| Normalized::new(r).get())
    .collect();
assert_eq!(ranks, [1.0, 1.0, 1.0]);

// Divide by the span first, and the positions survive.
let span = 79.0;
let ranks: Vec<f64> = [7.0, 42.0, 79.0]
    .into_iter()
    .map(|r| Normalized::new(r / span).get())
    .collect();
assert!(ranks[0] < ranks[1] && ranks[1] < ranks[2]);
```

## Where it sits

Depends on `tutti-types` ([`Samples`], [`ChannelLayout`], [`ChannelTopology`], the
meter vocabulary) and `tutti-midi-types` ([`MidiEvent`]), re-exporting the parts
format crates need so none of them takes its own dependency for the type names.
All four format hosts and `tutti-plugin` depend on it.

## Features

`default = []`.

- `serde` — `Serialize`/`Deserialize` on the wire-eligible types
  ([`TransportInfo`], [`ParameterChanges`], [`EditorSize`],
  [`EditorCapabilities`], [`WindowHandle`], and the metadata that rides the
  plugin-load reply). Required by `tutti-plugin`'s bincode IPC protocol; a host
  crate that does no IPC does not pay for the dependency.

## License

MIT OR Apache-2.0

[`PluginDescriptor`]: crate::PluginDescriptor
[`LoadedPlugin`]: crate::LoadedPlugin
[`ParameterInfo`]: crate::ParameterInfo
[`ParamAddress`]: crate::ParamAddress
[`ParamRange`]: crate::ParamRange
[`ParamFlags`]: crate::ParamFlags
[`ParamSteps`]: crate::ParamSteps
[`ProcessContext`]: crate::ProcessContext
[`ProcessOutput`]: crate::ProcessOutput
[`TransportInfo`]: crate::TransportInfo
[`ParameterChanges`]: crate::ParameterChanges
[`AudioBuffer`]: crate::AudioBuffer
[`WindowHandle`]: crate::WindowHandle
[`EditorSize`]: crate::EditorSize
[`EditorCapabilities`]: crate::EditorCapabilities
[`Features`]: crate::Features
[`PluginMeta`]: crate::PluginMeta
[`PluginAudio`]: crate::PluginAudio
[`PluginParams`]: crate::PluginParams
[`PluginState`]: crate::PluginState
[`PluginPresets`]: crate::PluginPresets
[`PluginEditorHost`]: crate::PluginEditorHost
[`PluginInstance`]: crate::PluginInstance
[`Samples`]: crate::Samples
[`ChannelLayout`]: crate::ChannelLayout
[`ChannelTopology`]: crate::ChannelTopology
[`MidiEvent`]: crate::MidiEvent
[`bounds`]: crate::ParameterInfo::bounds
[`default_value`]: crate::ParameterInfo::default_value
[`step_count`]: crate::ParameterInfo::step_count
[`flag`]: crate::ParameterInfo::flag
[`to_plain`]: crate::ParameterInfo::to_plain
