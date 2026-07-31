//! Format-agnostic plugin load data.
//!
//! The catalog-identity half (name, vendor, version, and each format's native
//! classification) lives in `tutti-plugin` next to `PluginRecord`, because it
//! carries per-format vocabulary this crate deliberately knows nothing about.
//!
//! What stays here is [`LoadedPlugin`] — the engine-wiring data produced when a
//! plugin is actually instantiated (bus widths, latency, f64 support). It rides
//! the IPC `BridgeMessage::PluginLoaded` reply, so its `Serialize`/`Deserialize`
//! derives are gated behind the `serde` feature that `tutti-plugin` enables for
//! its bincode wire path.

use smallvec::SmallVec;

use tutti_types::Samples;

use crate::{ChannelLayout, FeatureReport, Features};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Per-bus channel layouts for one process direction, in bus-index order.
///
/// An empty list means "single main bus" — the legacy single-bus convention,
/// where the one main bus's layout is recovered from elsewhere. A populated list
/// is `[main, aux/sidechain, …]`, so summing the counts gives the total channel
/// width the host must supply flat (see `tutti-vst3-host`'s bus buffers).
pub type BusChannels = SmallVec<[ChannelLayout; 4]>;

/// How long a plugin keeps producing audio after its input goes silent.
///
/// The engine-wide [`Tail`](tutti_types::Tail) under the name the loaders speak.
/// It is one type, not a plugin-shaped copy: a convolution reverb's tail and a
/// hosted reverb's tail have the same four answers and the same algebra, and two
/// names for the same behaviour is not a type. The format-specific decoding
/// (AU's seconds, the `u32::MAX` sentinel CLAP and VST3 share) is documented
/// there, on the constructor that performs it.
pub use tutti_types::Tail as PluginTail;

/// Engine-wiring data for a freshly instantiated plugin.
///
/// Produced by the plugin server at load time and returned over IPC; never
/// persisted (unlike the catalog `PluginDescriptor`, which is). Carries only
/// what the audio graph needs to wire the node — no format vocabulary, so it
/// stays in this format-agnostic crate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct LoadedPlugin {
    /// Per-bus input channel counts, main bus first. Empty == single main bus.
    pub inputs: BusChannels,
    /// Per-bus output channel counts, main bus first. Empty == single main bus.
    pub outputs: BusChannels,
    /// Reported initial processing latency, in samples (for PDC). The numeric
    /// half — the `Features::latency` presence bit is derived from this
    /// (`latency()`), not stored separately.
    ///
    /// [`Samples`] rather than a bare `usize`: AU already reports latency as
    /// one (its ABI gives seconds, so `tutti-au-host` converts and returns the
    /// unit type), and this field used to flatten it back with `.get()` at the
    /// loader. `Samples` is `#[serde(transparent)]`, so this is `512` on the
    /// wire either way and host and subprocess upgrade independently.
    pub latency_samples: Samples,
    /// How long the plugin keeps sounding after its input stops — see
    /// [`PluginTail`].
    ///
    /// Sits beside `latency_samples` because both are engine-wiring numbers a
    /// render needs, but it is a sum type rather than a count: "unbounded" and
    /// "never asked" are real answers here, and neither is a number.
    ///
    /// `serde(default)` so a peer built before this field decodes to
    /// [`PluginTail::Unknown`] — which is the honest reading of a payload that
    /// never carried a tail — rather than failing. bincode is not
    /// self-describing, so this does NOT rescue a short payload; that is
    /// `PROTOCOL_VERSION`'s job.
    #[cfg_attr(feature = "serde", serde(default))]
    pub tail: PluginTail,
    /// Capability flag set the plugin reported at load. The on/off half;
    /// numeric wiring stays in `inputs`/`outputs`/`latency_samples` above.
    /// (`f64` support was formerly the standalone `supports_f64` bool — it is
    /// now `Features::F64_AUDIO`.)
    ///
    /// Read through [`capability`](Self::capability) when the answer feeds a
    /// person — a clear bit here is also what an unprobed capability looks like,
    /// and [`probed`](Self::probed) is the half that tells them apart.
    /// The per-block send-gate reads this field directly and should: it has no
    /// way to act on "unknown" and must stay one mask-and-compare.
    pub features: Features,
    /// Which capabilities the loader actually probed.
    ///
    /// The AU loader probes one (`EDITOR`); VST3 probes nine. Without this,
    /// both report the same `Features` for a plugin that takes no MIDI — one
    /// because it was asked, one because nobody asked.
    ///
    /// `serde(default)` keeps a peer built before this field from failing to
    /// decode into an empty mask rather than a wrong one. bincode is not
    /// self-describing, so this does NOT rescue a short payload — that is
    /// `PROTOCOL_VERSION`'s job. It covers the JSON path and struct-update
    /// construction, where the honest default is "nothing was asked".
    #[cfg_attr(feature = "serde", serde(default))]
    pub probed: Features,
}

impl LoadedPlugin {
    /// Total input channel width across all buses (main + aux/sidechain). Falls
    /// back to `0` for an empty bus list — callers that need the single-bus main
    /// width supply it themselves (the count isn't carried here).
    pub fn total_inputs(&self) -> usize {
        self.inputs.iter().map(|l| l.count() as usize).sum()
    }

    /// Total output channel width across all buses.
    pub fn total_outputs(&self) -> usize {
        self.outputs.iter().map(|l| l.count() as usize).sum()
    }

    /// `true` if the plugin exposes more than one bus in either direction
    /// (sidechain / aux). Derived from the bus lists — not a stored flag, so it
    /// can't drift from the actual channel layout.
    pub fn multi_bus(&self) -> bool {
        self.inputs.len() > 1 || self.outputs.len() > 1
    }

    /// `true` if the plugin reports non-zero processing latency (participates in
    /// PDC). Derived from `latency_samples`.
    pub fn latency(&self) -> bool {
        self.latency_samples > Samples::ZERO
    }

    /// `Some(true)`/`Some(false)` when the loader probed `f`, [`None`] when
    /// it did not. Pass exactly one bit.
    ///
    /// The read for anything user-facing. The send-gate uses `features`
    /// directly — see the field docs for why the two differ.
    pub fn capability(&self, f: Features) -> Option<bool> {
        self.report().get(f)
    }

    /// The capability halves as one value.
    pub fn report(&self) -> FeatureReport {
        FeatureReport::new(self.probed, self.features)
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    /// A populated multi-bus layout round-trips through the real bincode wire
    /// serializer and the totals sum across buses.
    #[test]
    fn multi_bus_round_trips() {
        let loaded = LoadedPlugin {
            // stereo main + mono sidechain
            inputs: SmallVec::from_slice(&[ChannelLayout::Stereo, ChannelLayout::Mono]),
            outputs: SmallVec::from_slice(&[ChannelLayout::Stereo]),
            latency_samples: Samples(128),
            tail: PluginTail::Finite(Samples(48_000)),
            features: Features::F64_AUDIO | Features::MIDI_IN,
            probed: Features::F64_AUDIO | Features::MIDI_IN | Features::EDITOR,
        };
        let bytes = bincode::serialize(&loaded).unwrap();
        let back: LoadedPlugin = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, loaded);

        // `Samples` is `#[serde(transparent)]`, so typing this field cost no
        // wire bytes — a peer built against the bare `usize` decodes the same
        // payload. Pinned here because that is the whole reason the migration
        // needed no protocol bump.
        let as_usize = bincode::serialize(&128usize).unwrap();
        let as_samples = bincode::serialize(&Samples(128)).unwrap();
        assert_eq!(as_samples, as_usize);
        assert_eq!(back.total_inputs(), 3);
        assert_eq!(back.total_outputs(), 2);
        assert!(back.features.contains(Features::F64_AUDIO));
        assert!(back.multi_bus()); // two input buses
        assert!(back.latency()); // 128 samples
    }

    /// The probed mask crosses the wire, so an unprobed capability stays
    /// distinguishable from a declined one on the receiving side.
    #[test]
    fn the_probed_mask_crosses_the_wire() {
        // An AU-shaped report: one capability probed out of ten.
        let loaded = LoadedPlugin {
            features: Features::EDITOR,
            probed: Features::EDITOR,
            ..Default::default()
        };
        let bytes = bincode::serialize(&loaded).unwrap();
        let back: LoadedPlugin = bincode::deserialize(&bytes).unwrap();

        assert_eq!(back.capability(Features::EDITOR), Some(true));
        assert_eq!(
            back.capability(Features::MIDI_IN),
            None,
            "a capability the loader never probed must not arrive as a false"
        );
    }

    /// A default `LoadedPlugin` claims nothing, rather than claiming that every
    /// capability was probed and declined.
    #[test]
    fn a_defaulted_plugin_claims_no_capability() {
        let loaded = LoadedPlugin::default();
        assert_eq!(loaded.capability(Features::EDITOR), None);
        assert_eq!(loaded.capability(Features::TRANSPORT), None);
    }

    /// An empty bus list (single-bus legacy) round-trips and sums to zero.
    #[test]
    fn empty_buses_round_trip() {
        let loaded = LoadedPlugin::default();
        let bytes = bincode::serialize(&loaded).unwrap();
        let back: LoadedPlugin = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, loaded);
        assert!(back.inputs.is_empty());
        assert_eq!(back.total_inputs(), 0);
        assert_eq!(back.features, Features::empty());
        assert!(!back.multi_bus());
        assert!(!back.latency());
    }

    /// Every tail arm survives the wire distinctly.
    ///
    /// The value semantics are pinned where the type lives; what belongs here is
    /// the *encoding*, because this is the crate whose bincode IPC carries it.
    /// `PluginTail` is an alias rather than a local copy, and an alias changes
    /// no discriminant, no arm order and no payload — this test is the evidence
    /// for that, so it must keep passing unedited.
    #[test]
    fn every_tail_arm_survives_the_wire() {
        for tail in [
            PluginTail::Unknown,
            PluginTail::None,
            PluginTail::Finite(Samples(48_000)),
            PluginTail::Unbounded,
        ] {
            let bytes = bincode::serialize(&tail).unwrap();
            assert_eq!(bincode::deserialize::<PluginTail>(&bytes).unwrap(), tail);
        }
    }
}
