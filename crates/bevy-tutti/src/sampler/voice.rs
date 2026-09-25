//! Spawning a sampler voice as an ECS-owned graph node.
//!
//! One [`VoiceNode`] per entity, wired like any other node — `spawn_audio_node`
//! adds it, [`PortSources`](crate::graph::PortSources) on a sink names it as a
//! source, and the `On<Remove, AudioNode>` observer takes it back out.
//!
//! # Why not `VoicePool`
//!
//! The sampler ships a [`VoicePool`](tutti_sampler::VoicePool) that mixes many
//! voices behind one node, and its markers (`VoicePoolRef` / `VoicePoolNode`) say
//! "live on the track entity". That is the right shape for a host whose model is
//! *track owns clips*. It is the wrong shape for a host whose model is a graph of
//! nodes with edges: there, a source **is** a node, with its own placement, its
//! own gain, and its own outgoing connection.
//!
//! The pool's *retirement channel* looks like a hazard being ignored here. It
//! is not: that channel exists because `VoiceCommand::Remove` is handled inside
//! `drain_commands`, which runs from the audio callback, and dropping a slot
//! frees its vocoder bank. **That cannot happen here.** `Net::remove` *returns*
//! the `Box<dyn AudioUnit>` rather than dropping it, and
//! [`reconcile_node_despawn`](crate::graph::reconcile_node_despawn) discards it
//! inside an observer — main thread. No channel needed.
//!
//! One [`BeatCursor`](tutti_core::transport::BeatCursor) per voice is inherited
//! rather than one shared. The pool's doc argues against N cursors, but that
//! risk is about N slots sharing one timeline position; here each source
//! carries its own placement. Merging adjacent voices into a shared node is an
//! optimization available later, not a correctness debt.
//!
//! # The two tiers arrive differently and converge here
//!
//! [`Source::Memory`](tutti_sampler::Source) needs an `Arc<Wave>` — resident
//! before the voice exists. [`Source::Disk`](tutti_sampler::Source) needs a
//! butler channel and a `Command::Stream` first. Both end as a
//! [`Voice`], and [`SpawnVoice`] takes either.

use bevy_ecs::prelude::*;
use std::sync::Arc;

use tutti_core::{ChannelLayout, Timeline};
use tutti_sampler::{MemorySource, Playback, Voice, VoiceNode, VoiceNodeHandle, VoiceSource};

use crate::graph::InsertAudioNode;

/// Marks an entity whose audio node is a sampler voice.
///
/// The node id itself lives in `AudioNode`, like every other graph node — this
/// says only *what kind* it is, so a system can query voices without inspecting
/// the unit behind a `NodeId`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SamplerVoice;

/// The control-thread handle to this entity's voice node.
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
/// It cannot be. The sender half is minted inside
/// [`VoiceNode::with_commands`] beside the receiver that lives in the unit, and
/// once the unit is in the graph there is no way back to it — the graph's copy
/// of the unit shares the receiver, but the sender was never stored anywhere.
/// The constructor is the only moment both ends exist, so the handle has to be
/// kept from there.
///
/// Present **iff** the voice was built through [`InsertVoice`]/[`SpawnVoice`],
/// which is every voice this crate builds. A `VoiceNode` a host constructs
/// directly (resynth does this) has no command channel and so no handle; it
/// renders normally, it just cannot be moved.
#[derive(Component, Debug, Clone)]
pub struct VoiceCommands(
    /// The sender minted alongside the node's receiver at construction. See
    /// above for why it cannot be recovered later.
    pub VoiceNodeHandle,
);

/// Spawn a [`VoiceNode`] on an entity, wired to the transport.
///
/// An extension trait on `Commands` for the same reason
/// [`SpawnAudioNode`](crate::graph::SpawnAudioNode) is one: `Net::add` returns
/// its id inside a deferred command, so nothing outside the command queue can
/// observe the binding.
pub trait SpawnVoice {
    /// Add `voice` to the graph as a `width`-wide node on a **new** entity.
    ///
    /// The transport handle is bound **here, once**, per
    /// [`TransportRes::timeline`](crate::graph::TransportRes::timeline)'s
    /// contract: a placed voice reads the beat itself every block, and a system
    /// pushing per-frame positions instead would quantise scheduling to the
    /// framerate.
    fn spawn_voice(
        &mut self,
        voice: Voice,
        width: ChannelLayout,
        timeline: Arc<dyn Timeline>,
    ) -> EntityCommands<'_>;
}

/// Add a voice node to an entity that already exists.
///
/// The common case for a host whose entities come from somewhere else — a
/// projection compiles the source entity first, and the voice arrives frames
/// later once its audio is ready. [`SpawnVoice`] is for the standalone case.
pub trait InsertVoice {
    /// Make this entity a `width`-wide voice node. See [`SpawnVoice::spawn_voice`]
    /// for why the timeline is bound here.
    fn insert_voice(&mut self, voice: Voice, width: ChannelLayout, timeline: Arc<dyn Timeline>);
}

impl SpawnVoice for Commands<'_, '_> {
    fn spawn_voice(
        &mut self,
        voice: Voice,
        width: ChannelLayout,
        timeline: Arc<dyn Timeline>,
    ) -> EntityCommands<'_> {
        let mut e = self.spawn_empty();
        e.insert_voice(voice, width, timeline);
        e
    }
}

impl InsertVoice for EntityCommands<'_> {
    fn insert_voice(
        &mut self,
        mut voice: Voice,
        width: ChannelLayout,
        timeline: Arc<dyn Timeline>,
    ) {
        voice.replace_transport(timeline);
        // `with_commands`, not `with_channels`: a voice this crate builds is one
        // a host will want to *move*, and the handle can only be taken at
        // construction — see [`VoiceCommands`]. A node built without one cannot
        // be given a channel later.
        let (node, handle) = VoiceNode::with_commands(voice, width);
        self.insert_audio_node(node);
        self.insert((SamplerVoice, VoiceCommands(handle)));
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
    let mut source = MemorySource::with_channels(wave, width);
    source.set_window(window);
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
