//! Spawning a sampler voice as an ECS-owned graph node.
//!
//! One [`VoiceNode`] per entity, wired like any other node — it goes in as a
//! native [`GraphNode`] (`spawn_graph_node` / `insert_and_bind`, the path every
//! ported node takes), [`PortSources`](crate::graph::PortSources) on a sink
//! names it as a source, and the `On<Remove, AudioNode>` observer takes it
//! back out. Its controls are typed: a [`VoiceNodeHandle`] (gain as a param
//! cell, also addressable as `UnitParam::Volume`; placement over the node's
//! command queue), kept on the entity as [`VoiceCommands`].
//!
//! # Why not `VoicePool`
//!
//! The sampler ships a [`VoicePool`] that mixes many
//! voices behind one node, and its markers (`VoicePoolRef` / `VoicePoolNode`) say
//! "live on the track entity". That is the right shape for a host whose model is
//! *track owns clips*. It is the wrong shape for a host whose model is a graph of
//! nodes with edges: there, a source **is** a node, with its own placement, its
//! own gain, and its own outgoing connection.
//!
//! The pool's *retirement channel* looks like a hazard being ignored here. It
//! is not: that channel exists because `VoiceCommand::Remove` is handled inside
//! `drain_commands`, which runs from the audio callback, and dropping a slot
//! frees its vocoder bank. **That cannot happen here.** A node removed from
//! the graph ([`reconcile_node_despawn`](crate::graph::reconcile_node_despawn))
//! is retired by the executor *back* to the editor rather than dropped, and
//! freed in [`commit_graph`](crate::graph::commit_graph)'s collect — main
//! thread. No channel needed.
//!
//! A voice reads the transport from its block's `Env`, frame by frame (doc
//! 013 items 8 and 9): no voice holds a clock, so N voices cost N placements,
//! not N cursors on one timeline. Merging adjacent voices into a shared node
//! is an optimization available later, not a correctness debt.
//!
//! # The two tiers arrive differently and converge here
//!
//! [`Source::Memory`](tutti_sampler::Source) needs an `Arc<Wave>` — resident
//! before the voice exists. [`Source::Disk`](tutti_sampler::Source) needs a
//! butler channel and a `Command::Stream` first. Both end as a
//! [`Voice`], and [`SpawnVoice`] takes either.

use bevy_ecs::prelude::*;
use std::sync::Arc;

use tutti_core::ChannelLayout;
use tutti_graph::ParamSet;
use tutti_sampler::{
    DiskVoice, MemorySource, Playback, Voice, VoiceNode, VoiceNodeHandle, VoicePool, VoiceSource,
};

use crate::graph::events::insert_and_bind;
use crate::graph::{CapturedControls, GraphNode, NodeControls};

/// Marks an entity whose audio node is a sampler voice.
///
/// The node id itself lives in `AudioNode`, like every other graph node — this
/// says only *what kind* it is, so a system can query voices without inspecting
/// the unit behind a `NodeId`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SamplerVoice;

/// The control-thread handle to this entity's voice node: the
/// [`NodeControls`] a voice's insert keeps, by the name this crate has always
/// given it.
///
/// # Why a component rather than a resource map
///
/// It shares the voice's lifetime **exactly**. A `Resource` holding
/// `HashMap<Entity, VoiceNodeHandle>` would be a second owner of that lifetime,
/// and keeping it honest needs an invalidation path that always has a case it
/// cannot see — a despawned source, a voice rebuilt on a path change. A
/// component is removed when the entity is, for free, which is the whole
/// argument this codebase makes for components over caches.
///
/// # Why the handle is not simply rebuilt on demand
///
/// It cannot be. The handle is minted by the node's `IntoNode` beside the
/// receiver the node keeps, and once the node is in the graph there is no way
/// back to it. The insert is the only moment both ends exist, so the handle
/// has to be kept from there.
///
/// Present **iff** the voice was built through [`InsertVoice`]/[`SpawnVoice`]
/// (or `spawn_graph_node`), which is every voice this crate builds.
pub type VoiceCommands = NodeControls<VoiceNodeHandle>;

/// A voice as a native graph node: its params (the gain) addressed by its
/// set, so an [`AudioParam`](crate::graph::AudioParam) on the entity writes
/// the cell the node reads and a fork starts from the authored gain.
impl GraphNode for VoiceNode {
    fn captured(&self) -> CapturedControls {
        CapturedControls::for_params(&self.param_set())
    }

    fn params(controls: &VoiceNodeHandle) -> Option<ParamSet> {
        Some(controls.params().clone())
    }
}

/// A pool as a native graph node; its controls are its
/// [`VoicePoolHandle`](tutti_sampler::VoicePoolHandle).
impl GraphNode for VoicePool {}

/// A bare clip reader as a native graph node, its gain addressed by its set.
impl GraphNode for MemorySource {
    fn captured(&self) -> CapturedControls {
        CapturedControls::for_params(&tutti_graph::ParamNode::param_set(self))
    }

    fn params(controls: &ParamSet) -> Option<ParamSet> {
        Some(controls.clone())
    }
}

/// A streamed voice as a native graph node; its controls are its
/// [`DiskVoiceControls`](tutti_sampler::DiskVoiceControls).
impl GraphNode for DiskVoice {}

/// Spawn a [`VoiceNode`] on an entity.
///
/// An extension trait on `Commands` for the same reason
/// [`SpawnGraphNode`](crate::graph::SpawnGraphNode) is one: the insert returns
/// its id inside a deferred command, so nothing outside the command queue can
/// observe the binding.
pub trait SpawnVoice {
    /// Add `voice` to the graph as a `width`-wide node on a **new** entity.
    ///
    /// No transport is bound: a placed voice reads the playhead from its
    /// block's `Env`, per frame, and a system pushing per-frame positions
    /// instead would quantise scheduling to the framerate.
    fn spawn_voice(&mut self, voice: Voice, width: ChannelLayout) -> EntityCommands<'_>;
}

/// Add a voice node to an entity that already exists.
///
/// The common case for a host whose entities come from somewhere else — a
/// projection compiles the source entity first, and the voice arrives frames
/// later once its audio is ready. [`SpawnVoice`] is for the standalone case.
pub trait InsertVoice {
    /// Make this entity a `width`-wide voice node. See
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

/// Build an in-memory voice from a decoded wave.
///
/// The `Residency::Whole` half of the tier decision, expressed as a function so
/// a host writes one line rather than assembling a `Voice` by hand. The disk
/// half needs a butler round-trip and so is not a pure function — see
/// [`DiskStreamerRes`](super::DiskStreamerRes).
///
/// # `window` is required, not defaulted
///
/// It is the clip's authored placement on the timeline.
/// `MemorySource::with_channels` builds at `VoiceWindow::default()`, so a
/// caller allowed to omit this gets a resident clip playing at the default
/// position regardless of what the document authored — silently, from the first
/// frame, and only for short files, since the streaming tier takes the
/// placement in `take_disk_voice`.
///
/// Taking it as a parameter rather than letting a host patch it afterwards is
/// what makes that omission impossible: `apply_placement` is `pub(crate)` in
/// the sampler, so there is no after-the-fact fix available outside that crate.
pub fn memory_voice(
    wave: Arc<tutti_io::Wave>,
    width: ChannelLayout,
    play: Playback,
    window: tutti_sampler::VoiceWindow,
) -> Voice {
    let source = MemorySource::with_channels(wave, width).placed_at(window);
    Voice {
        source: VoiceSource::Memory(source),
        play,
        // `None`: the butler channel is a disk-tier concept. `Voice`'s own doc
        // says it is "meaningless for `Memory` (loop is primed directly on the
        // `MemorySource`)".
        channel_index: None,
    }
}

/// The width a voice should run at, given what the file has.
///
/// Clamped to [`MAX_SAMPLER_CHANNELS`](tutti_sampler::MAX_SAMPLER_CHANNELS)
/// because that is the sampler's own per-read stack ceiling — deliberately equal
/// to the graph root's, since a voice wider than the root can render is a voice
/// nobody can hear.
///
/// A zero-width file is coerced to mono rather than rejected: `ChannelLayout` can
/// represent an empty bus (a plugin port genuinely can be zero wide), but a graph
/// node that reports zero outputs is one nothing can be wired to.
///
/// **Deliberately not a function of the device width.** The graph root folds
/// whatever reaches it, so a voice narrowed to the device here would be narrowed
/// *twice* on a wider project. The only ceiling that applies is the sampler's.
pub fn voice_width(file: ChannelLayout) -> ChannelLayout {
    let n = file
        .count()
        .clamp(1, tutti_sampler::MAX_SAMPLER_CHANNELS as u16);
    ChannelLayout::from(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_io::Wave;

    /// `voice_width` is the file's own width clamped to `1..=MAX_SAMPLER_CHANNELS`
    /// — the device is deliberately not consulted, or a voice would be narrowed
    /// twice on a wider project.
    #[test]
    fn voice_width_clamps_to_what_the_graph_can_carry() {
        let ceiling = tutti_sampler::MAX_SAMPLER_CHANNELS;
        let cases: &[(ChannelLayout, usize, &str)] = &[
            (ChannelLayout::STEREO, 2, "a stereo file stays stereo"),
            (
                ChannelLayout::from(64usize),
                ceiling,
                "wider than the sampler's ceiling comes back at the ceiling, not \
                 truncated silently downstream at the root's fold",
            ),
            (
                ChannelLayout::from(0usize),
                1,
                "a zero-width file becomes mono, not a node nobody can wire",
            ),
        ];

        for &(file, expected, why) in cases {
            assert_eq!(
                voice_width(file).count() as usize,
                expected,
                "voice_width({} channels): {why}",
                file.count()
            );
        }
    }

    #[test]
    fn a_memory_voice_carries_no_butler_channel() {
        let wave = Arc::new(Wave::with_capacity(2, 48_000.0, 128));
        let v = memory_voice(
            wave,
            ChannelLayout::STEREO,
            Playback::default(),
            Default::default(),
        );
        assert!(
            v.channel_index.is_none(),
            "the butler channel is a disk-tier concept; `Voice`'s doc calls it \
             meaningless for Memory"
        );
        assert!(matches!(v.source, VoiceSource::Memory(_)));
    }
}
