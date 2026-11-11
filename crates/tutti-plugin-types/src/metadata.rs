//! Format-agnostic plugin load data.
//!
//! [`LoadedPlugin`] is the wiring data produced when a plugin is actually
//! instantiated (its buses, latency, tail, capability flags). It rides the
//! plugin-load IPC reply, so it is serializable under the `serde` feature.

use tutti_graph::BusRole;
use tutti_types::Samples;

use crate::{ChannelLayout, ChannelTopology, FeatureReport, Features};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Per-bus channel widths for one process direction, in bus-index order: the
/// widths alone, for sizing buffers (the shared-memory slab, a format's port
/// table). What a plugin *declares* about a bus is a [`PluginBus`].
pub type BusChannels = Vec<ChannelLayout>;

/// One audio bus a plugin declares: its width, where its channels sit, and what
/// it is for.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PluginBus {
    /// The channel count, which sizes the buffers.
    pub channels: ChannelLayout,
    /// Which speaker each channel feeds, when the loader could name every one.
    ///
    /// `None` is "unknown": the plugin declined, the format has no way to say
    /// (VST2), or the layout uses speakers this vocabulary does not yet name.
    /// It is never "no speakers": a bus with no channels is `Some` of an empty
    /// topology. A topology whose width is not [`channels`](Self::channels)'
    /// cannot be acted on; [`placement`](Self::placement) filters it out.
    pub topology: Option<ChannelTopology>,
    /// What the bus is for, as the format says it: [`BusRole::Main`] for the
    /// bus the format marks main (CLAP's `CLAP_AUDIO_PORT_IS_MAIN`, VST3's
    /// `kMain`, the one bus VST2 has, AU's element 0) and [`BusRole::Aux`] for
    /// the rest. No format marks a bus a sidechain, so no loader reports
    /// [`BusRole::Sidechain`].
    #[cfg_attr(feature = "serde", serde(with = "role_wire"))]
    pub role: BusRole,
}

impl PluginBus {
    /// A bus of `channels` with `role` and no placement.
    #[must_use]
    pub const fn new(channels: ChannelLayout, role: BusRole) -> Self {
        Self {
            channels,
            topology: None,
            role,
        }
    }

    /// A main bus of `channels` with no placement.
    #[must_use]
    pub const fn main(channels: ChannelLayout) -> Self {
        Self::new(channels, BusRole::Main)
    }

    /// The buses of a direction a format gives one bus (VST2, AU's element 0):
    /// a main bus of `channels`, or none for no channels (an instrument's
    /// inputs), since a list holds only buses the plugin has.
    #[must_use]
    pub fn single(channels: ChannelLayout) -> PluginBuses {
        if channels.count() == 0 {
            PluginBuses::new()
        } else {
            vec![Self::main(channels)]
        }
    }

    /// This bus with the placement `topology`.
    #[must_use]
    pub fn placed(self, topology: Option<ChannelTopology>) -> Self {
        Self { topology, ..self }
    }

    /// The bus's placement when it has one that agrees with its width: the
    /// one a caller routing by speaker may act on.
    #[must_use]
    pub fn placement(&self) -> Option<&ChannelTopology> {
        self.topology
            .as_ref()
            .filter(|t| t.layout() == self.channels)
    }
}

/// The buses of one direction, in bus-index order. Empty is a direction with
/// no audio (an instrument's inputs): a loader lists every bus it has.
pub type PluginBuses = Vec<PluginBus>;

/// The total width of `buses`.
fn width(buses: &[PluginBus]) -> usize {
    buses.iter().map(|b| b.channels.count() as usize).sum()
}

/// The widths of `buses`, in order.
fn widths(buses: &[PluginBus]) -> BusChannels {
    buses.iter().map(|b| b.channels).collect()
}

/// [`BusRole`] on the wire, as one byte: `tutti-graph` has no serde, and the
/// byte is the role's order in its declaration.
#[cfg(feature = "serde")]
mod role_wire {
    use super::BusRole;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub(super) fn serialize<S: Serializer>(role: &BusRole, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(match role {
            BusRole::Main => 0,
            BusRole::Sidechain => 1,
            BusRole::Aux => 2,
        })
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BusRole, D::Error> {
        match u8::deserialize(d)? {
            0 => Ok(BusRole::Main),
            1 => Ok(BusRole::Sidechain),
            2 => Ok(BusRole::Aux),
            other => Err(D::Error::custom(format_args!("no bus role {other}"))),
        }
    }
}

/// What a plugin declares on its audio ports: the buses, per direction.
///
/// Two layouts are equal when every bus agrees, which is how a re-read after a
/// restart decides the host must be told.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    /// The input buses.
    pub inputs: PluginBuses,
    /// The output buses.
    pub outputs: PluginBuses,
}

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
/// which is). Carries what the audio graph needs to wire the node: the buses
/// and placement, latency, tail, and the [`Features`] capability flags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct LoadedPlugin {
    /// The input buses, in bus-index order; see [`PluginBuses`].
    pub inputs: PluginBuses,
    /// The output buses.
    pub outputs: PluginBuses,
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
    // postcard payload is `PROTOCOL_VERSION`'s job, not serde's.
    #[cfg_attr(feature = "serde", serde(default))]
    pub probed: Features,
}

impl LoadedPlugin {
    /// Returns the total input channel width across all buses.
    pub fn total_inputs(&self) -> usize {
        width(&self.inputs)
    }

    /// Returns the total output channel width across all buses.
    pub fn total_outputs(&self) -> usize {
        width(&self.outputs)
    }

    /// The input buses' widths, in order: what sizes the input buffers.
    #[must_use]
    pub fn input_widths(&self) -> BusChannels {
        widths(&self.inputs)
    }

    /// The output buses' widths, in order.
    #[must_use]
    pub fn output_widths(&self) -> BusChannels {
        widths(&self.outputs)
    }

    /// Returns `true` if the plugin exposes more than one bus in either
    /// direction (sidechain / aux).
    pub fn multi_bus(&self) -> bool {
        self.inputs.len() > 1 || self.outputs.len() > 1
    }

    /// Returns `true` if every bus in both directions has a
    /// [`placement`](PluginBus::placement): the consistency a caller routing
    /// by speaker depends on. False when any bus's placement is unknown or
    /// disagrees with its width, which is not an error, just not enough
    /// information.
    pub fn topology_is_complete(&self) -> bool {
        self.inputs
            .iter()
            .chain(&self.outputs)
            .all(|b| b.placement().is_some())
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

    fn stereo() -> ChannelTopology {
        ChannelTopology::smpte(ChannelLayout::STEREO).expect("stereo has an order")
    }

    /// A stereo main input with its placement, a mono aux with none.
    fn main_and_aux() -> PluginBuses {
        vec![
            PluginBus::new(ChannelLayout::STEREO, BusRole::Main).placed(Some(stereo())),
            PluginBus::new(ChannelLayout::MONO, BusRole::Aux),
        ]
    }

    /// A multi-bus plugin round-trips through the postcard wire, every bus's
    /// width, placement and role with it, and the totals sum across buses.
    ///
    /// The `None` placement is load-bearing: "unknown" is a different answer
    /// from `Some(empty)` ("no channels"), and a round trip that collapsed the
    /// two would let a caller read an aux bus's speakers it never reported.
    #[test]
    fn a_multi_bus_plugin_round_trips() {
        let loaded = LoadedPlugin {
            inputs: main_and_aux(),
            outputs: vec![PluginBus::new(ChannelLayout::STEREO, BusRole::Main)],
            latency_samples: Samples(128),
            tail: PluginTail::Finite(Samples(48_000)),
            features: Features::F64_AUDIO | Features::MIDI_IN,
            probed: Features::F64_AUDIO | Features::MIDI_IN | Features::EDITOR,
        };
        let bytes = postcard::to_allocvec(&loaded).unwrap();
        let back: LoadedPlugin = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, loaded);
        assert_eq!(back.inputs[1].topology, None);
        assert_eq!(back.total_inputs(), 3);
        assert_eq!(back.total_outputs(), 2);
        assert_eq!(
            back.input_widths(),
            vec![ChannelLayout::STEREO, ChannelLayout::MONO]
        );
        assert!(back.multi_bus());
        assert!(back.latency());
    }

    /// **Every role crosses the wire as itself**, and a byte that names no
    /// role is refused rather than read as one.
    #[test]
    fn every_role_crosses_the_wire_and_an_unknown_one_is_refused() {
        for role in [BusRole::Main, BusRole::Sidechain, BusRole::Aux] {
            let bus = PluginBus::new(ChannelLayout::MONO, role);
            let bytes = postcard::to_allocvec(&bus).unwrap();
            assert_eq!(postcard::from_bytes::<PluginBus>(&bytes).unwrap(), bus);
        }
        let mut bytes =
            postcard::to_allocvec(&PluginBus::new(ChannelLayout::MONO, BusRole::Aux)).unwrap();
        *bytes.last_mut().unwrap() = 3;
        assert!(postcard::from_bytes::<PluginBus>(&bytes).is_err());
    }

    /// An unknown bus makes the topology incomplete, and a full one completes it.
    ///
    /// `topology_is_complete` is what a caller checks before routing by
    /// speaker, so both answers are pinned — a predicate hardwired to `false`
    /// would satisfy the negative half alone.
    #[test]
    fn topology_is_complete_only_when_every_bus_agrees_with_its_count() {
        let mono = ChannelTopology::smpte(ChannelLayout::MONO).expect("mono has an order");
        let bus = |topology| PluginBus::new(ChannelLayout::STEREO, BusRole::Main).placed(topology);
        let of = |input, output| LoadedPlugin {
            inputs: vec![bus(input)],
            outputs: vec![bus(output)],
            ..Default::default()
        };
        assert!(of(Some(stereo()), Some(stereo())).topology_is_complete());
        assert!(
            !of(None, Some(stereo())).topology_is_complete(),
            "an unknown bus is not complete"
        );
        // The count sizes the buffer, so a topology of another width describes
        // another bus.
        assert!(
            !of(Some(mono), Some(stereo())).topology_is_complete(),
            "a mono topology on a stereo bus describes a different bus"
        );
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
        let bytes = postcard::to_allocvec(&loaded).unwrap();
        let back: LoadedPlugin = postcard::from_bytes(&bytes).unwrap();

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

    /// A plugin with no buses round-trips and has no width.
    #[test]
    fn empty_buses_round_trip() {
        let loaded = LoadedPlugin::default();
        let bytes = postcard::to_allocvec(&loaded).unwrap();
        let back: LoadedPlugin = postcard::from_bytes(&bytes).unwrap();
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
    /// the *encoding*, because this is the crate whose postcard IPC carries it.
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
            let bytes = postcard::to_allocvec(&tail).unwrap();
            assert_eq!(postcard::from_bytes::<PluginTail>(&bytes).unwrap(), tail);
        }
    }
}
