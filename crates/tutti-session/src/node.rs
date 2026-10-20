//! [`SessionNode`], the trait every insert path takes, and the engine's own
//! nodes as ones.

use tutti_graph::{ForkByClone, IntoNode, Node, ParamSet, Unforkable};

/// A node that may enter a session's graph: it declares its own ports (event
/// ports included), its controls and its fork ([`IntoNode`]), and says which
/// of its controls is a [`ParamSet`] ([`params`](Self::params)).
///
/// Every node the engine ships is one. A host's own node is one of:
///
/// - wrapped: [`ForkByClone`]`(node)` (forkable by a clone taken at insert,
///   for a node whose `Clone` shares nothing) or [`Unforkable`]`(node)`
///   (refuses a fork that needs it): no controls and no params;
/// - a `tutti_graph::ParamNode` (its params a `ParamSet`, forked by
///   `tutti_graph::param_parts`), whose impl returns its controls from
///   `params`: its params reached by address
///   ([`Graph::set_param`](crate::Graph::set_param), control-rate
///   modulation) and forked from what was set;
/// - its own impl with the default `params` (none), for a node with
///   controls of another shape.
pub trait SessionNode: IntoNode + Send + 'static {
    /// The node's params by address, read off its controls once it is in:
    /// what [`Graph::set_param`](crate::Graph::set_param) writes through, and
    /// what a fork of the node starts from. `None`, the default, for a node
    /// with none.
    fn params(controls: &Self::Controls) -> Option<ParamSet> {
        let _ = controls;
        None
    }
}

/// Each listed type is a [`SessionNode`] whose controls are its
/// [`ParamSet`].
macro_rules! param_session_node {
    ($($ty:ty),* $(,)?) => {$(
        impl SessionNode for $ty {
            fn params(controls: &ParamSet) -> Option<ParamSet> {
                Some(controls.clone())
            }
        }
    )*};
}

/// A node forkable by a clone taken at insert: no controls, no params.
impl<N: Node + Clone + Send + 'static> SessionNode for ForkByClone<N> {}

/// A node no fork may take: no controls, no params.
impl<N: Node + Send + 'static> SessionNode for Unforkable<N> {}

param_session_node!(
    tutti_nodes::SvfFilterNode<f32>,
    tutti_nodes::SvfFilterNode<f64>,
    tutti_nodes::EqBandNode<f32>,
    tutti_nodes::EqBandNode<f64>,
    tutti_nodes::LadderFilterNode<f32>,
    tutti_nodes::LadderFilterNode<f64>,
    tutti_nodes::CompressorNode,
    tutti_nodes::GateNode,
    tutti_nodes::LimiterNode,
    tutti_nodes::BrickwallLimiterNode,
    tutti_nodes::DistortionNode,
    tutti_nodes::BusStripNode,
);

/// The width adapters: no controls, forked by clone.
impl SessionNode for tutti_nodes::DownmixNode {}
impl SessionNode for tutti_nodes::ChannelSumNode {}

// The delay, modulation and LFO nodes.
param_session_node!(
    tutti_nodes::DelayLineNode,
    tutti_nodes::ModDelayNode,
    tutti_nodes::PhaserNode,
    tutti_nodes::LfoNode,
);

// The convolution reverb: its mix by address.
#[cfg(feature = "convolution")]
param_session_node!(tutti_nodes::ConvolverNode);

/// An automation lane as a graph node: no controls, the beat read from its
/// block's `Env`.
impl SessionNode for tutti_nodes::automation::AutomationLaneNode {}

/// A MIDI clip as a graph node: its `MidiClipControls`, no params.
#[cfg(feature = "midi")]
impl SessionNode for tutti_midi_runtime::MidiClipNode {}

/// The SoundFont player as a graph node.
#[cfg(feature = "soundfont")]
impl SessionNode for tutti_soundfont::SoundFontUnit {}

/// The VBAP panner as a graph node. Its position, spread and width are typed
/// cells no `UnitParam` addresses, so it has no [`ParamSet`]: its controls
/// are its `VbapPannerControls`.
#[cfg(feature = "spatial")]
impl SessionNode for tutti_spatial::VbapPannerNode {}

/// The binaural panner as a graph node, its controls its
/// `HrtfBinauralControls` (see the VBAP panner's impl for why not a
/// [`ParamSet`]).
#[cfg(feature = "hrtf")]
impl SessionNode for tutti_spatial::HrtfBinauralNode {}

// The synth as a graph node: one MIDI event input, and its live params
// (master volume, unison detune and spread) as a `ParamSet`, so a param
// write reaches it and a fork of it starts from what was set.
#[cfg(feature = "synth")]
param_session_node!(tutti_polysynth::PolySynth);

/// A voice as a graph node: its params (the gain) addressed by its set, so a
/// param write reaches the cell the node reads and a fork starts from the
/// gain as set.
#[cfg(feature = "sampler")]
impl SessionNode for tutti_sampler::VoiceNode {
    fn params(controls: &tutti_sampler::VoiceNodeHandle) -> Option<ParamSet> {
        Some(controls.params().clone())
    }
}

/// A pool as a graph node; its controls are its `VoicePoolHandle`.
#[cfg(feature = "sampler")]
impl SessionNode for tutti_sampler::VoicePool {}

/// A bare clip reader as a graph node, its gain addressed by its set.
#[cfg(feature = "sampler")]
impl SessionNode for tutti_sampler::MemorySource {
    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }
}

/// A streamed voice as a graph node; its controls are its
/// `DiskVoiceControls`.
#[cfg(feature = "sampler")]
impl SessionNode for tutti_sampler::DiskVoice {}
