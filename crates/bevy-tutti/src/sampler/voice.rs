//! Spawning a sampler voice as an ECS-owned graph node.
//!
//! One [`VoiceNode`] per entity, wired like any other node — it goes in as a
//! [`GraphNode`] (`spawn_audio_node` / `insert_and_bind`, the path every
//! node takes), [`PortSources`](crate::graph::PortSources) on a sink
//! names it as a source, and the `On<Remove, AudioNode>` observer takes it
//! back out. Its controls are typed: a [`VoiceNodeHandle`] (gain as a param
//! cell, also addressable as `UnitParam::Volume`; placement as a published
//! value the node reads each block), kept on the entity as [`VoiceCommands`].
//!
//! # One node per voice, or a pool
//!
//! The sampler also ships a [`VoicePool`] that mixes many voices behind one
//! node, for a host whose model is *a track owns its clips*. A host whose model
//! is a graph of nodes with edges spawns one voice per entity instead: a source
//! **is** a node, with its own placement, its own gain, and its own outgoing
//! connection. Both are [`GraphNode`]s.
//!
//! A voice node removed from the graph
//! ([`reconcile_node_despawn`](crate::graph::reconcile_node_despawn)) is
//! handed back by the audio thread and freed on the main thread, in
//! [`commit_graph`](crate::graph::commit_graph), never on the audio thread.
//!
//! A voice reads the transport from its block's `Env`, frame by frame: no
//! voice holds a clock, so N voices cost N placements, not N cursors on one
//! timeline.
//!
//! # The two tiers arrive differently and converge here
//!
//! [`Source::Memory`](tutti_sampler::Source) needs an `Arc<Wave>` — resident
//! before the voice exists. [`Source::Disk`](tutti_sampler::Source) needs a
//! butler channel and a `Command::Stream` first. Both end as a
//! [`Voice`], and [`SpawnVoice`] takes either. The memory tier's is one call,
//! [`tutti_sampler::memory_voice`], at the width
//! [`tutti_sampler::voice_width`] gives the file.

use bevy_ecs::prelude::*;

use tutti_core::ChannelLayout;
use tutti_graph::ParamSet;
use tutti_sampler::{DiskVoice, MemorySource, Voice, VoiceNode, VoiceNodeHandle, VoicePool};

use crate::graph::insert_and_bind;
use crate::graph::{GraphNode, NodeControls};

/// Marks an entity whose audio node is a sampler voice.
///
/// The node id itself lives in `AudioNode`, like every other graph node — this
/// says only *what kind* it is, so a system can query voices without inspecting
/// the unit behind a `NodeKey`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SamplerVoice;

/// The control-thread handle to this entity's voice node: the
/// [`NodeControls`] a voice's insert keeps.
///
/// A component, so it shares the voice's lifetime exactly and goes when the
/// entity does. The handle is minted when the node goes into the graph and
/// cannot be rebuilt afterwards, so keep this component to control the voice
/// (gain, placement).
///
/// Present **iff** the voice was built through [`InsertVoice`]/[`SpawnVoice`]
/// (or `spawn_audio_node`), which is every voice this crate builds.
pub type VoiceCommands = NodeControls<VoiceNodeHandle>;

/// A voice as a graph node: its params (the gain) addressed by its
/// set, so an [`AudioParam`](crate::graph::AudioParam) on the entity writes
/// the cell the node reads and a fork starts from the gain as set.
impl GraphNode for VoiceNode {
    fn params(controls: &VoiceNodeHandle) -> Option<ParamSet> {
        Some(controls.params().clone())
    }
}

/// A pool as a graph node; its controls are its
/// [`VoicePoolHandle`](tutti_sampler::VoicePoolHandle).
impl GraphNode for VoicePool {}

/// A bare clip reader as a graph node, its gain addressed by its set.
impl GraphNode for MemorySource {
    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }
}

/// A streamed voice as a graph node; its controls are its
/// [`DiskVoiceControls`](tutti_sampler::DiskVoiceControls).
impl GraphNode for DiskVoice {}

/// `Commands` extension that spawns a [`VoiceNode`] on a new entity.
///
/// An extension trait on `Commands` for the same reason
/// [`SpawnAudioNode`](crate::graph::SpawnAudioNode) is one: the insert returns
/// its id inside a deferred command. The entity gets [`SamplerVoice`],
/// [`AudioNode`](tutti_core::AudioNode) and [`VoiceCommands`]; it arrives
/// unwired, like every node.
pub trait SpawnVoice {
    /// Adds `voice` to the graph as a `width`-wide node on a **new** entity.
    ///
    /// No transport is bound: a placed voice reads the playhead from its
    /// block's `Env`, per frame, and a system pushing per-frame positions
    /// instead would quantise scheduling to the framerate.
    fn spawn_voice(&mut self, voice: Voice, width: ChannelLayout) -> EntityCommands<'_>;
}

/// `EntityCommands` extension that makes an existing entity a voice node.
///
/// For an entity spawned before its audio is ready: the voice arrives frames
/// later, once the clip is loaded. [`SpawnVoice`] is for the standalone case.
pub trait InsertVoice {
    /// Makes this entity a `width`-wide voice node. See
    /// [`SpawnVoice::spawn_voice`].
    fn insert_voice(&mut self, voice: Voice, width: ChannelLayout);
}

impl SpawnVoice for Commands<'_, '_> {
    fn spawn_voice(&mut self, voice: Voice, width: ChannelLayout) -> EntityCommands<'_> {
        let mut e = self.spawn_empty();
        e.insert_voice(voice, width);
        e
    }
}

impl InsertVoice for EntityCommands<'_> {
    fn insert_voice(&mut self, voice: Voice, width: ChannelLayout) {
        // Inserted through its `IntoNode`: the handle ([`VoiceCommands`]) can
        // only be taken there — see its doc.
        let node = VoiceNode::with_channels(voice, width);
        let entity = self.id();
        self.insert(SamplerVoice);
        self.commands()
            .queue(move |world: &mut World| insert_and_bind(world, entity, node));
    }
}
