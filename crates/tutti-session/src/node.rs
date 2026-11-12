//! [`SessionNode`], the trait every insert path takes, and the engine's own
//! nodes as ones.

use tutti_graph::{ForkByClone, IntoNode, Node, ParamNode, ParamSet, Params, Unforkable};
#[allow(unused_imports)]
use tutti_types::{
    Amplitude, Azimuth, Cents, CompressionRatio, Db, Depth, Drive, Elevation, Feedback, Hz, Mix,
    Pan, Q, Resonance, Seconds, Spread, StereoWidth, UnitParam,
};

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

    /// The node reading the session's tempo map through `tempo` (a reader
    /// the graph hands each node it takes in [`Graph::add`](crate::Graph::add)
    /// and [`Graph::crossfade`](crate::Graph::crossfade); the node clones it).
    /// The identity, the default, for a node that reads no map: a sampler
    /// node overrides it, so a placed voice stays where the map puts it
    /// through a step or a ramp (doc 020, stage 5).
    #[must_use]
    fn bind_tempo(self, tempo: &tutti_transport::TempoReader) -> Self
    where
        Self: Sized,
    {
        let _ = tempo;
        self
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
/// [`ParamSet`], with the params it declares: a kind and the unit its value is
/// in, from which its [`Key`](crate::Key)'s param methods are generated
/// ([`declare_params!`](crate::declare_params)). The `ParamSet` the node
/// builds is held against the declaration by `tests::declarations_match`.
macro_rules! param_session_node {
    ($($ty:ty { $($decl:tt)* }),* $(,)?) => {$(
        impl SessionNode for $ty {
            fn params(controls: &ParamSet) -> Option<ParamSet> {
                Some(controls.clone())
            }
        }
        crate::declare_params!($ty { $($decl)* });
    )*};
}

/// A node forkable by a clone taken at insert: no controls, no params.
impl<N: Node + Clone + Send + 'static> SessionNode for ForkByClone<N> {}

/// A node no fork may take: no controls, no params.
impl<N: Node + Send + 'static> SessionNode for Unforkable<N> {}

/// A host's [`ParamNode`], inserted as one: its [`ParamSet`] as its controls
/// and its params reached by address, forked from the values set. Its typed
/// param methods come from
/// [`declare_params!`](crate::declare_params).
impl<N: ParamNode + Clone + Send + 'static> SessionNode for Params<N> {
    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }
}

param_session_node!(
    tutti_nodes::SvfFilterNode<f32> {
        UnitParam::Cutoff => Hz, UnitParam::Q => Q, UnitParam::GainDb => Db
    },
    tutti_nodes::SvfFilterNode<f64> {
        UnitParam::Cutoff => Hz, UnitParam::Q => Q, UnitParam::GainDb => Db
    },
    tutti_nodes::EqBandNode<f32> {
        UnitParam::Cutoff => Hz, UnitParam::Q => Q, UnitParam::GainDb => Db
    },
    tutti_nodes::EqBandNode<f64> {
        UnitParam::Cutoff => Hz, UnitParam::Q => Q, UnitParam::GainDb => Db
    },
    // The ladder's resonance is addressed as `Q`, in `Resonance`.
    tutti_nodes::LadderFilterNode<f32> {
        UnitParam::Cutoff => Hz, UnitParam::Q => Resonance, UnitParam::Drive => Drive
    },
    tutti_nodes::LadderFilterNode<f64> {
        UnitParam::Cutoff => Hz, UnitParam::Q => Resonance, UnitParam::Drive => Drive
    },
    // The compressor's make-up gain is addressed as `GainDb`.
    tutti_nodes::CompressorNode {
        UnitParam::Threshold => Db,
        UnitParam::Ratio => CompressionRatio,
        UnitParam::Attack => Seconds,
        UnitParam::Release => Seconds,
        UnitParam::GainDb => Db
    },
    tutti_nodes::GateNode {
        UnitParam::Threshold => Db, UnitParam::Attack => Seconds, UnitParam::Release => Seconds
    },
    tutti_nodes::LimiterNode {
        UnitParam::Threshold => Db, UnitParam::Ceiling => Db, UnitParam::Release => Seconds
    },
    tutti_nodes::BrickwallLimiterNode { UnitParam::Ceiling => Db },
    tutti_nodes::DistortionNode { UnitParam::Drive => Drive },
    tutti_nodes::BusStripNode { UnitParam::Volume => Amplitude, UnitParam::Pan => Pan },
    // One cutoff per split.
    tutti_nodes::CrossoverNode { UnitParam::Cutoff => [Hz] },
);

/// The multiband compressor: its params by address, its band meters beside
/// them in its controls. Per band: a threshold, knee, ratio, attack, release,
/// gain, bypass and solo; a cutoff per split.
impl SessionNode for tutti_nodes::MultibandCompressorNode {
    fn params(controls: &tutti_nodes::MultibandControls) -> Option<ParamSet> {
        Some(controls.params().clone())
    }
}
crate::declare_params!(tutti_nodes::MultibandCompressorNode {
    UnitParam::Cutoff => [Hz],
    UnitParam::Threshold => [Db],
    UnitParam::Knee => [Db],
    UnitParam::Ratio => [CompressionRatio],
    UnitParam::Attack => [Seconds],
    UnitParam::Release => [Seconds],
    UnitParam::GainDb => [Db],
    UnitParam::Bypass => [bool],
    UnitParam::Solo => [bool]
});

// The delay, modulation and LFO nodes. The chorus and flanger's depth is a
// swing in seconds, the phaser's and the LFO's a fraction.
param_session_node!(
    tutti_nodes::DelayLineNode { UnitParam::Feedback => Feedback, UnitParam::DelayTime => Seconds },
    tutti_nodes::ModDelayNode {
        UnitParam::Rate => Hz, UnitParam::Depth => Seconds, UnitParam::Feedback => Feedback
    },
    tutti_nodes::PhaserNode {
        UnitParam::Rate => Hz, UnitParam::Depth => Depth, UnitParam::Feedback => Feedback
    },
    // A beat-synced LFO has no rate.
    tutti_nodes::LfoNode { UnitParam::Depth => Depth, UnitParam::Rate => Hz },
);

// The two reverbs (wet only: their blend is the graph's mix): the room and damping (Freeverb) or the
// decays, size, diffusion, pre-delay, modulation, width, wet gain and (if the
// design can freeze) freeze (the FDN), unless it is the fixed General MIDI send.
param_session_node!(
    tutti_nodes::FreeverbNode { UnitParam::RoomSize => Mix, UnitParam::Damping => Mix },
    tutti_nodes::FdnNode {
        UnitParam::Decay => [Seconds],
        UnitParam::RoomSize => Mix,
        UnitParam::Diffusion => Mix,
        UnitParam::DelayTime => Seconds,
        UnitParam::Depth => Depth,
        UnitParam::Rate => Hz,
        UnitParam::StereoSpread => StereoWidth,
        UnitParam::GainDb => Db,
        UnitParam::Freeze => bool
    },
);

// The convolution reverb: wet only; its blend is the graph's mix. Its wet
// gain has no address.
#[cfg(feature = "convolution")]
param_session_node!(tutti_nodes::ConvolverNode {});

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

/// The MIDI ingest: its `MidiIngestControls`, no params; the MPE modes
/// it replaced freed by its reclaimer.
#[cfg(feature = "midi")]
impl SessionNode for tutti_midi_runtime::MidiIngestNode {
    fn reclaimer(controls: &tutti_midi_runtime::MidiIngestControls) -> Option<Reclaimer> {
        let controls = controls.clone();
        Some(Reclaimer::new(move || controls.reclaim()))
    }
}

/// The SoundFont player as a graph node.
#[cfg(feature = "soundfont")]
impl SessionNode for tutti_soundfont::SoundFontUnit {}

// The two panners: bearing and height, and the VBAP spread and width or the
// HRTF blend, as a `ParamSet`.
param_session_node!(
    tutti_nodes::VbapPannerNode {
        UnitParam::Azimuth => Azimuth,
        UnitParam::Elevation => Elevation,
        UnitParam::Spread => Spread,
        UnitParam::StereoWidth => StereoWidth
    },
    tutti_nodes::HrtfBinauralNode {
        UnitParam::Azimuth => Azimuth, UnitParam::Elevation => Elevation, UnitParam::Wet => Mix
    },
);

// The synth as a graph node: one MIDI event input, and its live params
// (master volume, unison detune and spread) as a `ParamSet`, so a param
// write reaches it and a fork of it starts from what was set.
#[cfg(feature = "synth")]
param_session_node!(tutti_polysynth::PolySynth {
    UnitParam::Volume => Amplitude, UnitParam::Detune => Cents, UnitParam::StereoSpread => Spread
});

/// A voice as a graph node: its params (the gain) addressed by its set, so a
/// param write reaches the cell the node reads and a fork starts from the
/// gain as set; the placements it replaced freed by its reclaimer.
#[cfg(feature = "sampler")]
impl SessionNode for tutti_sampler::VoiceNode {
    fn bind_tempo(self, tempo: &tutti_transport::TempoReader) -> Self {
        self.with_tempo(tempo.clone())
    }

    fn params(controls: &tutti_sampler::VoiceNodeHandle) -> Option<ParamSet> {
        Some(controls.params().clone())
    }

    fn reclaimer(controls: &tutti_sampler::VoiceNodeHandle) -> Option<Reclaimer> {
        let controls = controls.clone();
        Some(Reclaimer::new(move || controls.reclaim()))
    }
}
#[cfg(feature = "sampler")]
crate::declare_params!(tutti_sampler::VoiceNode { UnitParam::Volume => Amplitude });

/// A pool as a graph node; its controls are its `VoicePoolHandle`.
#[cfg(feature = "sampler")]
impl SessionNode for tutti_sampler::VoicePool {
    fn bind_tempo(self, tempo: &tutti_transport::TempoReader) -> Self {
        self.with_tempo(tempo.clone())
    }
}

/// A bare clip reader as a graph node, its gain addressed by its set.
#[cfg(feature = "sampler")]
impl SessionNode for tutti_sampler::MemorySource {
    fn bind_tempo(self, tempo: &tutti_transport::TempoReader) -> Self {
        self.with_tempo(tempo.clone())
    }

    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }
}
#[cfg(feature = "sampler")]
crate::declare_params!(tutti_sampler::MemorySource { UnitParam::Volume => Amplitude });

/// A [`Clip`](tutti_sampler::Clip) as a graph node: the placed clip reader it
/// builds, its gain addressed by its set.
#[cfg(feature = "sampler")]
impl SessionNode for tutti_sampler::Clip {
    fn bind_tempo(self, tempo: &tutti_transport::TempoReader) -> Self {
        self.with_tempo(tempo.clone())
    }

    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }
}
#[cfg(feature = "sampler")]
crate::declare_params!(tutti_sampler::Clip { UnitParam::Volume => Amplitude });

/// A streamed voice as a graph node; its controls are its
/// `DiskVoiceControls`.
#[cfg(feature = "sampler")]
impl SessionNode for tutti_sampler::DiskVoice {
    fn bind_tempo(self, tempo: &tutti_transport::TempoReader) -> Self {
        self.with_tempo(tempo.clone())
    }
}

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
    /// crate; the graph's sweep of them in `tests/wiring/graph.rs`.
    #[test]
    fn every_publishing_node_keeps_a_reclaimer() {
        #[cfg(feature = "midi")]
        {
            assert!(keeps_one(tutti_midi_runtime::MidiClipNode::new([])));
            assert!(keeps_one(tutti_midi_runtime::HarmonyNode::new([])));
            assert!(keeps_one(tutti_midi_runtime::MidiIngestNode::new(1)));
        }
        #[cfg(feature = "sampler")]
        {
            use std::sync::Arc;
            let source = tutti_sampler::MemorySource::placed(
                Arc::new(tutti_codec::Wave::new(
                    1,
                    tutti_types::SampleRate::new(48_000.0),
                )),
                tutti_types::Beat(0.0),
                None,
            )
            .expect("a placeable wave");
            assert!(keeps_one(tutti_sampler::VoiceNode::new(
                tutti_sampler::Voice {
                    source: tutti_sampler::VoiceSource::Memory(source),
                    play: tutti_sampler::Playback::default(),
                }
            )));
        }
        // A node that publishes nothing keeps none.
        assert!(!keeps_one(
            tutti_nodes::automation::AutomationLaneNode::new(
                tutti_nodes::automation::Trajectory::new()
            )
        ));
    }

    /// The declared kinds held against what the node builds: every address of
    /// its `ParamSet` is a declared kind (at index 0, unless several are
    /// declared), and every declared kind is built (the maximal configuration
    /// of the node: a freezable FDN, a free-running LFO).
    fn declared_matches<N: SessionNode + crate::params::Declared>(node: N, what: &str) {
        let (_, controls) = node.into_node();
        let set = N::params(&controls).expect("a node with params");
        let built: Vec<_> = set.params().collect();
        for addr in &built {
            let declared = N::KINDS.iter().find(|(kind, _)| *kind == addr.param);
            let Some((_, many)) = declared else {
                panic!("{what}: builds {addr:?}, which it does not declare");
            };
            assert!(
                *many || addr.index == 0,
                "{what}: {addr:?} is one of several"
            );
        }
        for (kind, _) in N::KINDS {
            assert!(
                built.iter().any(|addr| addr.param == *kind),
                "{what}: declares {kind:?}, which it does not build"
            );
        }
    }

    /// **Each engine node's declaration of its params (the kinds a `Key`'s
    /// methods are generated from) is the `ParamSet` the node builds**: a kind
    /// declared and not built, or built and not declared, fails here.
    #[test]
    fn declarations_match_what_the_nodes_build() {
        use tutti_nodes::*;
        use tutti_types::{
            ChannelLayout, CompressionRatio, Db, Drive, Feedback, Hz, Q, Resonance, Seconds,
        };
        declared_matches(
            SvfFilterNode::<f32>::new(SvfType::LowPass, Hz(1e3), Q(0.7)),
            "svf f32",
        );
        declared_matches(
            SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(1e3), Q(0.7)),
            "svf f64",
        );
        declared_matches(
            EqBandNode::<f32>::new(SvfType::LowPass, Hz(1e3), Q(0.7), Db(3.0)),
            "eq band",
        );
        declared_matches(
            LadderFilterNode::<f32>::new(LadderType::LP24, Hz(1e3), Resonance::new(0.3)),
            "ladder",
        );
        declared_matches(
            CompressorNode::mono(
                Db(-20.0),
                CompressionRatio(4.0),
                Seconds(0.01),
                Seconds(0.1),
            ),
            "compressor",
        );
        declared_matches(
            GateNode::mono(Db(-40.0), Seconds(0.001), Seconds(0.01), Seconds(0.1)),
            "gate",
        );
        declared_matches(LimiterNode::new(Db(-6.0), Db(-1.0)), "limiter");
        declared_matches(BrickwallLimiterNode::new(Db(-1.0)), "brickwall limiter");
        declared_matches(
            DistortionNode::new(ShapeKind::Tanh, Drive::new(2.0)),
            "distortion",
        );
        declared_matches(BusStripNode::new(), "strip");
        declared_matches(
            CrossoverNode::new(ChannelLayout::STEREO, &[Hz(500.0), Hz(2e3)]).unwrap(),
            "crossover",
        );
        declared_matches(
            MultibandCompressorNode::new(ChannelLayout::STEREO, 3).unwrap(),
            "multiband",
        );
        declared_matches(
            DelayLineNode::new(Seconds(1.0), Seconds(0.2), Feedback::new(0.3)),
            "delay",
        );
        declared_matches(ModDelayNode::chorus(ChannelLayout::STEREO), "chorus");
        declared_matches(PhaserNode::new(4), "phaser");
        declared_matches(LfoNode::new(LfoShape::Sine), "free-running lfo");
        declared_matches(
            FreeverbNode::live(FreeverbDesign, FreeverbControls::GENERAL_MIDI),
            "live freeverb",
        );
        declared_matches(
            FdnNode::live(
                FdnDesign::new(8, Seconds(8.0), true).unwrap(),
                FdnControls::DEFAULT,
            ),
            "freezable fdn",
        );
    }
}
