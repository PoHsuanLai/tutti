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
/// A reverb with a 4-second decay rings for 4 seconds past its last input; a
/// gain stage stops immediately. A bounce that stops rendering when the last
/// clip ends truncates the first mid-decay, so the render has to keep pulling
/// for the tail.
///
/// **A sum type rather than a number, because three answers are not one.**
/// The formats disagree, and each disagreement is real:
///
/// - **AU** reports `kAudioUnitProperty_TailTime` in *seconds*, and rejects the
///   property outright on units that have no tail concept — every Apple
///   instrument, mixer and generator does (measured, macOS 15.6).
/// - **CLAP** reports `clap_plugin_tail.get` in *samples*.
/// - **VST3** exposes `getTailSamples`, also in samples, where `0` means no
///   tail.
///
/// Both sample-based formats saturate at `u32::MAX`, which their specs read as
/// an effectively unbounded tail; `clap-sys` does not bind a named constant for
/// it, so [`from_samples`](Self::from_samples) names the sentinel once here
/// rather than each loader spelling the literal.
/// - **VST2** has no tail concept the vendored bindings surface.
///
/// [`Unbounded`](Self::Unbounded) exists because a real plugin uses it and a
/// number cannot carry it. TAL Reverb 4 answers `f64::INFINITY` for its tail
/// (measured; no Apple unit exceeds ~21 s), and
/// [`Seconds::to_samples`](tutti_types::Seconds::to_samples) maps every
/// non-finite input to `Samples::ZERO` — deliberately, since that is the right
/// answer for NaN and negatives. The consequence is that an *infinite* tail
/// arrives bit-identical to a *no* tail, and a bounce sizing its render from
/// that number truncates the reverb completely. The two want opposite handling:
/// unbounded wants a user-chosen fade, none wants nothing.
///
/// [`Unknown`](Self::Unknown) is not [`None`](Self::None). A plugin that was
/// never asked, or whose format has no tail query, has said nothing about its
/// tail — reporting that as "no tail" is the same class of invention the
/// `probed` mask exists to prevent for capabilities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PluginTail {
    /// The format has no tail query, or this loader did not ask.
    #[default]
    Unknown,
    /// The plugin declared it produces nothing after its input stops.
    None,
    /// A bounded tail, in samples at the rate the plugin was loaded with.
    Finite(Samples),
    /// The plugin declared an unbounded tail — it never decays to silence on
    /// its own. A bounce must choose where to stop; it cannot ask the plugin.
    Unbounded,
}

impl PluginTail {
    /// The tail as a sample count a render can add, or `None` when there is no
    /// finite answer.
    ///
    /// [`Unknown`](Self::Unknown) and [`Unbounded`](Self::Unbounded) both yield
    /// `None`, for opposite reasons — one has no information, the other has
    /// information that is not a number. A caller that wants to treat either as
    /// zero says so with `unwrap_or(Samples::ZERO)` and is seen to have decided.
    pub const fn samples(self) -> Option<Samples> {
        match self {
            Self::None => Some(Samples::ZERO),
            Self::Finite(s) => Some(s),
            Self::Unknown | Self::Unbounded => None,
        }
    }

    /// Build from a format's raw sample count, mapping the `u32::MAX` sentinel
    /// CLAP and VST3 both use for "unbounded".
    ///
    /// The sentinel is the formats' own, so decoding it belongs here rather
    /// than being repeated at each loader.
    pub fn from_samples(raw: u32) -> Self {
        match raw {
            0 => Self::None,
            u32::MAX => Self::Unbounded,
            n => Self::Finite(Samples(n as usize)),
        }
    }
}

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

    /// An unbounded tail must stay distinguishable from no tail at all.
    ///
    /// This is the whole reason [`PluginTail`] is a sum type. TAL Reverb 4
    /// answers `f64::INFINITY` for `kAudioUnitProperty_TailTime`, and
    /// `Seconds::to_samples` maps every non-finite input to `Samples::ZERO` —
    /// correct for NaN and negatives, exactly backwards for `+∞`. Carried as a
    /// number, "infinite reverb" and "no tail" arrive bit-identical, and a
    /// bounce that sizes its render from that number truncates the reverb
    /// completely.
    #[test]
    fn unbounded_is_not_none() {
        assert_ne!(PluginTail::Unbounded, PluginTail::None);

        // `samples()` refuses to answer for both `Unbounded` and `Unknown`, so
        // a caller cannot accidentally read either as zero.
        assert_eq!(PluginTail::None.samples(), Some(Samples::ZERO));
        assert_eq!(PluginTail::Unbounded.samples(), None);
        assert_eq!(PluginTail::Unknown.samples(), None);
        assert_eq!(
            PluginTail::Finite(Samples(512)).samples(),
            Some(Samples(512))
        );

        // And the distinction survives the wire, which is where it would be
        // lost if the field were a count.
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

    /// The `u32::MAX` sentinel CLAP and VST3 share decodes to `Unbounded`, and
    /// zero to `None` — the two ends a raw count cannot tell apart.
    #[test]
    fn from_samples_decodes_the_format_sentinel() {
        assert_eq!(PluginTail::from_samples(0), PluginTail::None);
        assert_eq!(PluginTail::from_samples(u32::MAX), PluginTail::Unbounded);
        assert_eq!(
            PluginTail::from_samples(44_100),
            PluginTail::Finite(Samples(44_100))
        );
    }
}
