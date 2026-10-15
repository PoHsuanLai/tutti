//! Format-agnostic plugin load data.
//!
//! [`LoadedPlugin`] is the wiring data produced when a plugin is actually
//! instantiated (bus widths, latency, tail, capability flags). It rides the
//! plugin-load IPC reply, so it is serializable under the `serde` feature.

use tutti_types::Samples;

use crate::{ChannelLayout, ChannelTopology, FeatureReport, Features};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Per-bus channel layouts for one process direction, in bus-index order.
///
/// An empty list means "single main bus", whose layout the list does not
/// carry. A populated list
/// is `[main, aux/sidechain, …]`, so summing the counts gives the total channel
/// width the host must supply flat (see `tutti-vst3-host`'s bus buffers).
pub type BusChannels = Vec<ChannelLayout>;

/// Per-bus channel *topology* for one process direction, in bus-index order.
///
/// This is the speaker-placement half of [`BusChannels`].
///
/// `None` at an index means the layout for that bus is unknown: the plugin
/// declined, the format cannot express it (VST2), or it names speakers this
/// vocabulary does not yet have. That is deliberately distinct from
/// `Some(empty topology)`, which means a bus with no channels.
///
/// An empty *list* carries no claim about any bus, which is what a host that
/// never asked reports.
pub type BusTopologies = Vec<Option<ChannelTopology>>;

/// How long a plugin keeps producing audio after its input goes silent.
///
/// An alias of the engine-wide [`Tail`](tutti_types::Tail), so a hosted
/// plugin's tail composes with any other node's. The format-specific decoding
/// (AU's seconds, the `u32::MAX` sentinel CLAP and VST3 share) is documented on
/// the constructor that performs it.
pub use tutti_types::Tail as PluginTail;

/// The wiring data a plugin reports when it is instantiated.
///
/// Produced by the plugin server at load time and returned over IPC; never
/// persisted (unlike the catalog [`PluginDescriptor`](crate::PluginDescriptor),
/// which is). Carries what the audio graph needs to wire the node: bus widths
/// and placement, latency, tail, and the [`Features`] capability flags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct LoadedPlugin {
    /// Per-bus input channel counts, main bus first. Empty == single main bus.
    pub inputs: BusChannels,
    /// Per-bus output channel counts, main bus first. Empty == single main bus.
    pub outputs: BusChannels,
    /// Reported initial processing latency in sample frames, used for plugin
    /// delay compensation. [`latency`](Self::latency) reads whether it is
    /// non-zero.
    pub latency_samples: Samples,
    /// How long the plugin keeps sounding after its input stops — see
    /// [`PluginTail`].
    ///
    /// Sits beside `latency_samples` because both are engine-wiring numbers a
    /// render needs, but it is a sum type rather than a count: "unbounded" and
    /// "never asked" are real answers here, and neither is a number.
    ///
    /// Deserializes as [`PluginTail::Unknown`] when absent.
    #[cfg_attr(feature = "serde", serde(default))]
    pub tail: PluginTail,
    /// Capability flags the plugin reported at load, such as
    /// [`Features::F64_AUDIO`] and [`Features::MIDI_IN`].
    ///
    /// Read through [`capability`](Self::capability) when the answer feeds a
    /// person — a clear bit here is also what an unprobed capability looks like,
    /// and [`probed`](Self::probed) is the half that tells them apart.
    /// The per-block send-gate reads this field directly and should: it has no
    /// way to act on "unknown" and must stay one mask-and-compare.
    pub features: Features,
    /// Which capabilities the loader actually probed.
    ///
    /// Loaders probe different subsets (see
    /// [`features::probed`](crate::features::probed)). Without this mask, a
    /// plugin that was asked about MIDI and declined looks the same as one
    /// nobody asked. Empty ("nothing was asked") when absent from a serialized
    /// value.
    // `serde(default)` covers JSON and struct-update construction; a short
    // bincode payload is `PROTOCOL_VERSION`'s job, not serde's.
    #[cfg_attr(feature = "serde", serde(default))]
    pub probed: Features,
    /// Which speaker each channel feeds, per bus, in the same order as
    /// [`inputs`](Self::inputs) / [`outputs`](Self::outputs).
    ///
    /// The placement half of what those two carry as counts. A format host
    /// reports it when it can name every channel; an entry is `None` when the
    /// plugin declined, the format has no way to say (VST2), or the layout uses
    /// speakers this vocabulary does not yet name. `None` is therefore
    /// "unknown", never "no speakers" — an empty topology is a real, distinct
    /// answer meaning a bus with no channels.
    ///
    /// **Widths still come from `inputs`/`outputs`.** A caller sizing buffers
    /// reads the counts, and a caller routing *by speaker* reads this; see
    /// [`topology_is_complete`](Self::topology_is_complete) for whether the two
    /// agree.
    // Appended last and `serde(default)`: bincode is positional, so a field in
    // the middle would renumber everything after it.
    #[cfg_attr(feature = "serde", serde(default))]
    pub input_topology: BusTopologies,
    /// Per-bus output channel topology. See
    /// [`input_topology`](Self::input_topology).
    #[cfg_attr(feature = "serde", serde(default))]
    pub output_topology: BusTopologies,
}

impl LoadedPlugin {
    /// Returns the total input channel width across all buses (main plus
    /// aux/sidechain).
    ///
    /// Returns `0` for an empty bus list; a caller that needs the single-bus
    /// main width supplies it itself, since it is not carried here.
    pub fn total_inputs(&self) -> usize {
        self.inputs.iter().map(|l| l.count() as usize).sum()
    }

    /// Returns the total output channel width across all buses.
    pub fn total_outputs(&self) -> usize {
        self.outputs.iter().map(|l| l.count() as usize).sum()
    }

    /// Returns `true` if the plugin exposes more than one bus in either
    /// direction (sidechain / aux).
    pub fn multi_bus(&self) -> bool {
        self.inputs.len() > 1 || self.outputs.len() > 1
    }

    /// Returns the channel topology of one input bus, if the loader could name
    /// it.
    ///
    /// `None` covers every "not known" case in one answer — the bus index is
    /// past the reported list, the format cannot express placement, or the
    /// plugin declined. A caller that needs to distinguish those reads
    /// [`input_topology`](Self::input_topology) directly.
    ///
    /// Accessors rather than raw indexing because the topology list and the
    /// count list are parallel, and a caller indexing both by hand is one
    /// off-by-one away from reading another bus's speakers.
    pub fn input_bus_topology(&self, bus: usize) -> Option<&ChannelTopology> {
        self.input_topology.get(bus)?.as_ref()
    }

    /// Returns the channel topology of one output bus. See
    /// [`input_bus_topology`](Self::input_bus_topology).
    pub fn output_bus_topology(&self, bus: usize) -> Option<&ChannelTopology> {
        self.output_topology.get(bus)?.as_ref()
    }

    /// Returns `true` if every bus in both directions reported a topology whose
    /// width matches the count beside it.
    ///
    /// The consistency a caller routing by speaker depends on: the counts size
    /// the buffers and the topology says what each channel is, so a topology
    /// describing a different number of channels than its own bus cannot be
    /// acted on. False when any topology is absent, which is the honest answer
    /// for "can I route this by speaker" — not an error, just not enough
    /// information.
    pub fn topology_is_complete(&self) -> bool {
        let agrees = |counts: &BusChannels, topos: &BusTopologies| {
            counts.len() == topos.len()
                && counts
                    .iter()
                    .zip(topos.iter())
                    .all(|(c, t)| t.as_ref().is_some_and(|t| t.layout() == *c))
        };
        agrees(&self.inputs, &self.input_topology) && agrees(&self.outputs, &self.output_topology)
    }

    /// Returns `true` if the plugin reports non-zero processing latency, so it
    /// takes part in plugin delay compensation.
    pub fn latency(&self) -> bool {
        self.latency_samples > Samples::ZERO
    }

    /// Returns `Some(answer)` when the loader probed `f`, [`None`] when it did
    /// not.
    ///
    /// Pass exactly one bit.
    ///
    /// The read for anything user-facing. The send-gate uses `features`
    /// directly — see the field docs for why the two differ.
    pub fn capability(&self, f: Features) -> Option<bool> {
        self.report().get(f)
    }

    /// Returns [`features`](Self::features) and [`probed`](Self::probed) as one
    /// [`FeatureReport`].
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
            inputs: vec![ChannelLayout::STEREO, ChannelLayout::MONO],
            outputs: vec![ChannelLayout::STEREO],
            latency_samples: Samples(128),
            tail: PluginTail::Finite(Samples(48_000)),
            features: Features::F64_AUDIO | Features::MIDI_IN,
            probed: Features::F64_AUDIO | Features::MIDI_IN | Features::EDITOR,
            ..Default::default()
        };
        let bytes = bincode::serialize(&loaded).unwrap();
        let back: LoadedPlugin = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, loaded);

        // `Samples` is `#[serde(transparent)]`, so it encodes exactly like a
        // bare `usize` on the wire.
        let as_usize = bincode::serialize(&128usize).unwrap();
        let as_samples = bincode::serialize(&Samples(128)).unwrap();
        assert_eq!(as_samples, as_usize);
        assert_eq!(back.total_inputs(), 3);
        assert_eq!(back.total_outputs(), 2);
        assert!(back.features.contains(Features::F64_AUDIO));
        assert!(back.multi_bus()); // two input buses
        assert!(back.latency()); // 128 samples
    }

    /// **The bus lists' wire format is unchanged**: each encodes as a
    /// sequence (a length, then the buses), which is what the inline lists
    /// they replaced encoded as, so a server and host built on either side
    /// of the change still read each other's load reply.
    ///
    /// No change to the list type that keeps it a list changes these bytes
    /// (every serde sequence encodes so); the layout is written out instead.
    /// Mutation (run): the expected bytes without the length prefix → fails.
    #[test]
    fn the_bus_lists_encode_as_sequences() {
        fn seq<T: Serialize>(items: &[T]) -> Vec<u8> {
            let mut out = (items.len() as u64).to_le_bytes().to_vec();
            for item in items {
                out.extend(bincode::serialize(item).unwrap());
            }
            out
        }
        let stereo = ChannelTopology::smpte(ChannelLayout::STEREO).expect("stereo has an order");
        let inputs: BusChannels = vec![ChannelLayout::STEREO, ChannelLayout::MONO];
        let topology: BusTopologies = vec![Some(stereo), None];
        assert_eq!(bincode::serialize(&inputs).unwrap(), seq(&inputs));
        assert_eq!(bincode::serialize(&topology).unwrap(), seq(&topology));
    }

    /// The [`LoadedPlugin`] behind [`GOLDEN_LOADED_PLUGIN`]: two input buses
    /// (a six-speaker 5.1 bed and a mono bus with no topology) and a stereo
    /// output, so the bus lists and `ChannelTopology`'s positions are both
    /// on the wire.
    fn golden_loaded_plugin() -> LoadedPlugin {
        use tutti_types::Speaker;
        let five_one = ChannelTopology::new([
            Speaker::FrontLeft,
            Speaker::FrontRight,
            Speaker::FrontCenter,
            Speaker::LowFrequency,
            Speaker::BackLeft,
            Speaker::BackRight,
        ]);
        LoadedPlugin {
            inputs: [ChannelLayout::from_count(6), ChannelLayout::MONO]
                .into_iter()
                .collect(),
            outputs: [ChannelLayout::STEREO].into_iter().collect(),
            latency_samples: Samples::new(64),
            tail: PluginTail::Finite(Samples::new(4800)),
            features: Features::MIDI_IN | Features::TRANSPORT,
            probed: Features::MIDI_IN | Features::TRANSPORT | Features::EDITOR,
            input_topology: [Some(five_one), None].into_iter().collect(),
            output_topology: [ChannelTopology::smpte(ChannelLayout::STEREO)]
                .into_iter()
                .collect(),
        }
    }

    /// `golden_loaded_plugin()` encoded by origin/main at `f07984d6`, the code
    /// before `smallvec` was removed (bus lists and topology positions were
    /// `SmallVec`s then). Recorded by running the same fixture there.
    const GOLDEN_LOADED_PLUGIN: [u8; 113] = [
        0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x02, 0x00, 0x00, 0x00, 0xc0, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x22, 0x00, 0x2a,
        0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x06, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    ];

    /// **A load reply encodes to the bytes the code before this change
    /// produced, and decodes from them.** Unlike the sequence-shape test
    /// above, these bytes were recorded from the old code rather than
    /// restated from the layout, so a host and a server built on either side
    /// of the change are shown to exchange the same reply, topology positions
    /// included.
    ///
    /// Mutation (run): `input_topology` and `output_topology` swapped in
    /// `LoadedPlugin`'s declaration → the encoding differs → fails.
    #[test]
    fn a_load_reply_encodes_as_before_smallvec_was_removed() {
        let value = golden_loaded_plugin();
        assert_eq!(bincode::serialize(&value).unwrap(), GOLDEN_LOADED_PLUGIN);
        let back: LoadedPlugin = bincode::deserialize(&GOLDEN_LOADED_PLUGIN).unwrap();
        assert_eq!(back, value);
        assert_eq!(
            back.input_topology[0].as_ref().map(|t| t.positions().len()),
            Some(6)
        );
    }

    /// Per-bus topology survives the real bincode wire, `None` entries included.
    ///
    /// The `None` is the load-bearing part: it means "this bus's placement is
    /// unknown", which is a different answer from `Some(empty)` ("this bus has
    /// no channels"). A round trip that collapsed the two would let a caller
    /// read a sidechain's speakers off a bus that never reported any.
    #[cfg(feature = "serde")]
    #[test]
    fn per_bus_topology_round_trips_including_the_unknown_entries() {
        let stereo = ChannelTopology::smpte(ChannelLayout::STEREO).expect("stereo has an order");
        let loaded = LoadedPlugin {
            inputs: vec![ChannelLayout::STEREO, ChannelLayout::MONO],
            outputs: vec![ChannelLayout::STEREO],
            // The main input is named; the sidechain's placement is not known.
            input_topology: vec![Some(stereo.clone()), None],
            output_topology: vec![Some(stereo.clone())],
            ..Default::default()
        };

        let bytes = bincode::serialize(&loaded).unwrap();
        let back: LoadedPlugin = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, loaded);

        assert_eq!(back.input_bus_topology(0), Some(&stereo));
        assert_eq!(
            back.input_bus_topology(1),
            None,
            "an unknown bus must stay unknown, not become an empty topology"
        );
        assert_eq!(
            back.input_bus_topology(9),
            None,
            "a bus index past the list is also unknown"
        );

        // Widths still come from the counts, unchanged by any of this.
        assert_eq!(back.total_inputs(), 3);
        assert_eq!(back.total_outputs(), 2);
    }

    /// An unknown bus makes the topology incomplete, and a full one completes it.
    ///
    /// `topology_is_complete` is what a caller checks before routing by
    /// speaker, so both answers are pinned — a predicate hardwired to `false`
    /// would satisfy the negative half alone.
    #[test]
    fn topology_is_complete_only_when_every_bus_agrees_with_its_count() {
        let stereo = ChannelTopology::smpte(ChannelLayout::STEREO).expect("stereo has an order");
        let mono = ChannelTopology::smpte(ChannelLayout::MONO).expect("mono has an order");

        let full = LoadedPlugin {
            inputs: vec![ChannelLayout::STEREO],
            outputs: vec![ChannelLayout::STEREO],
            input_topology: vec![Some(stereo.clone())],
            output_topology: vec![Some(stereo.clone())],
            ..Default::default()
        };
        assert!(full.topology_is_complete());

        let missing = LoadedPlugin {
            input_topology: vec![None],
            ..full.clone()
        };
        assert!(
            !missing.topology_is_complete(),
            "an unknown bus is not complete"
        );

        // A topology whose width disagrees with its own bus count cannot be
        // acted on: the count sizes the buffer, so the two must describe the
        // same channels.
        let disagreeing = LoadedPlugin {
            input_topology: vec![Some(mono)],
            ..full.clone()
        };
        assert!(
            !disagreeing.topology_is_complete(),
            "a mono topology on a stereo bus describes a different bus"
        );

        // A host that never asked reports nothing, which is also not complete.
        let never_asked = LoadedPlugin {
            input_topology: Vec::new(),
            output_topology: Vec::new(),
            ..full
        };
        assert!(!never_asked.topology_is_complete());
    }

    /// The probed mask crosses the wire, so an unprobed capability stays
    /// distinguishable from a declined one on the receiving side.
    #[test]
    fn the_probed_mask_crosses_the_wire() {
        // A report with one capability probed.
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

    /// An empty bus list (single main bus) round-trips and sums to zero.
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
