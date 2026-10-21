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
///
/// A node whose controls publish to it through an `ArcLend` cell also says
/// how to free what that cell retired ([`reclaimer`](Self::reclaimer)), so
/// the session's reclaim phase reaches it whichever host inserted it.
pub trait SessionNode: IntoNode + Send + 'static {
    /// The node's params by address, read off its controls once it is in:
    /// what [`Graph::set_param`](crate::Graph::set_param) writes through, and
    /// what a fork of the node starts from. `None`, the default, for a node
    /// with none.
    fn params(controls: &Self::Controls) -> Option<ParamSet> {
        let _ = controls;
        None
    }

    /// What frees the values the node's published cells retired, read off
    /// its controls once it is in. The graph keeps it by node until the node
    /// is removed or replaced, and runs it on every update
    /// ([`Phase::Reclaim`](crate::Phase::Reclaim)). `None`, the default, for
    /// a node that publishes nothing.
    fn reclaimer(controls: &Self::Controls) -> Option<Reclaimer> {
        let _ = controls;
        None
    }
}

/// A node's control-side reclaim, kept by the graph
/// ([`SessionNode::reclaimer`]): a closure over a clone of the node's
/// controls that calls their `reclaim` (each `ArcLend::reclaim`) and returns
/// how many retired values are still pinned afterwards. Control thread only,
/// like the `reclaim` it calls.
pub struct Reclaimer(Box<dyn Fn() -> usize + Send>);

impl Reclaimer {
    /// A reclaimer that runs `reclaim`.
    pub fn new(reclaim: impl Fn() -> usize + Send + 'static) -> Self {
        Self(Box::new(reclaim))
    }

    /// Runs it: how many retired values are still pinned.
    pub fn reclaim(&self) -> usize {
        (self.0)()
    }
}

impl std::fmt::Debug for Reclaimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Reclaimer(..)")
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

/// A MIDI clip as a graph node: its `MidiClipControls`, no params; the
/// clips it replaced freed by its reclaimer.
#[cfg(feature = "midi")]
impl SessionNode for tutti_midi_runtime::MidiClipNode {
    fn reclaimer(controls: &tutti_midi_runtime::MidiClipControls) -> Option<Reclaimer> {
        let controls = controls.clone();
        Some(Reclaimer::new(move || controls.reclaim()))
    }
}

/// The harmony node: its `HarmonyControls`, no params; the timelines it
/// replaced freed by its reclaimer.
#[cfg(feature = "midi")]
impl SessionNode for tutti_midi_runtime::HarmonyNode {
    fn reclaimer(controls: &tutti_midi_runtime::HarmonyControls) -> Option<Reclaimer> {
        let controls = controls.clone();
        Some(Reclaimer::new(move || controls.reclaim()))
    }
}

/// The live MIDI input: its `MidiInputControls`, no params; the MPE modes
/// it replaced freed by its reclaimer.
#[cfg(feature = "midi")]
impl SessionNode for tutti_midi_runtime::MidiInputNode {
    fn reclaimer(controls: &tutti_midi_runtime::MidiInputControls) -> Option<Reclaimer> {
        let controls = controls.clone();
        Some(Reclaimer::new(move || controls.reclaim()))
    }
}

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
/// gain as set; the placements it replaced freed by its reclaimer.
#[cfg(feature = "sampler")]
impl SessionNode for tutti_sampler::VoiceNode {
    fn params(controls: &tutti_sampler::VoiceNodeHandle) -> Option<ParamSet> {
        Some(controls.params().clone())
    }

    fn reclaimer(controls: &tutti_sampler::VoiceNodeHandle) -> Option<Reclaimer> {
        let controls = controls.clone();
        Some(Reclaimer::new(move || controls.reclaim()))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `N`'s impl hands the graph a reclaimer once `node` is split.
    fn keeps_one<N: SessionNode>(node: N) -> bool {
        let (_, controls) = node.into_node();
        N::reclaimer(&controls).is_some()
    }

    /// **Every engine node whose controls publish to it keeps a
    /// reclaimer**: the clip, the harmony track, the live input and the
    /// voice. What each one frees is pinned with a held loan in its own
    /// crate; the graph's sweep of them in `tests/graph.rs`.
    ///
    /// Mutation (run): each impl's `reclaimer` returning `None` (the
    /// default) → fails, one at a time.
    #[test]
    fn every_publishing_node_keeps_a_reclaimer() {
        #[cfg(feature = "midi")]
        {
            assert!(keeps_one(tutti_midi_runtime::MidiClipNode::new([])));
            assert!(keeps_one(tutti_midi_runtime::HarmonyNode::new([])));
            assert!(keeps_one(tutti_midi_runtime::MidiInputNode::new(None)));
        }
        #[cfg(feature = "sampler")]
        {
            use std::sync::Arc;
            let source = tutti_sampler::MemorySource::placed(
                Arc::new(tutti_io::Wave::new(1, 48_000.0)),
                tutti_core::Beat(0.0),
                None,
            );
            assert!(keeps_one(tutti_sampler::VoiceNode::new(
                tutti_sampler::Voice {
                    source: tutti_sampler::VoiceSource::Memory(source),
                    play: tutti_sampler::Playback::default(),
                    channel_index: None,
                }
            )));
        }
        // A node that publishes nothing keeps none.
        assert!(!keeps_one(tutti_nodes::DownmixNode::new(
            tutti_core::ChannelLayout::STEREO,
            tutti_core::ChannelLayout::MONO,
        )));
    }
}
