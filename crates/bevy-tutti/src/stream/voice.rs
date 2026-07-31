//! Spawning a sampler voice as an ECS-owned graph node.
//!
//! One [`VoiceNode`] per entity, wired like any other node — `spawn_audio_node`
//! adds it, [`AudioSources`](crate::graph::AudioSources) on a sink names it as a
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
//! The pool's most intricate machinery does not transfer either, and the reason
//! is worth stating because it looks like a hazard we are ignoring. `VoicePool`
//! carries a bounded *retirement channel* so a removed slot is freed on the
//! control thread — necessary there because `VoiceCommand::Remove` is handled
//! inside `drain_commands`, which runs from `tick`/`process`, i.e. the audio
//! callback, and dropping a slot frees its vocoder bank (~192 KB at six
//! channels).
//!
//! **That cannot happen on this path.** `Net::remove` *returns* the
//! `Box<dyn AudioUnit>` rather than dropping it, and
//! [`reconcile_node_despawn`](crate::graph::reconcile_node_despawn) discards it
//! inside an observer — main thread. The free lands exactly where the pool's
//! channel was trying to put it, without a channel.
//!
//! What we do inherit from the per-node path: one
//! [`BeatCursor`](tutti_core::transport::BeatCursor) per voice rather than one
//! shared. The pool's doc argues against N cursors ("N chances to disagree about
//! whether the playhead moved"), and that is a real risk when N slots share one
//! timeline position. It is much weaker here: each source carries its own
//! placement, so they were never reading one position to begin with.
//!
//! Merging adjacent voices back into a shared node is an optimization available
//! later. It is not a correctness debt.
//!
//! # The two tiers arrive differently and converge here
//!
//! [`Source::Memory`](tutti_sampler::Source) needs an `Arc<Wave>` — resident
//! before the voice exists. [`Source::Disk`](tutti_sampler::Source) needs a
//! butler channel and a `Command::Stream` first. Both end as a
//! [`Voice`](tutti_sampler::Voice), and [`SpawnVoice`] takes either.

use bevy_ecs::prelude::*;
use std::sync::Arc;

use tutti_core::{ChannelLayout, Timeline};
use tutti_sampler::{MemorySource, Playback, Voice, VoiceNode, VoiceSource};

use crate::graph::SpawnAudioNode;

/// Marks an entity whose audio node is a sampler voice.
///
/// The node id itself lives in `AudioNode`, like every other graph node — this
/// says only *what kind* it is, so a system can query voices without inspecting
/// the unit behind a `NodeId`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SamplerVoice;

/// Spawn a [`VoiceNode`] on an entity, wired to the transport.
///
/// An extension trait on `Commands` for the same reason
/// [`SpawnAudioNode`](crate::graph::SpawnAudioNode) is one: `Net::add` returns
/// its id inside a deferred command, so nothing outside the command queue can
/// observe the binding.
pub trait SpawnVoice {
    /// Add `voice` to the graph as a `width`-wide node on this entity.
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

impl SpawnVoice for Commands<'_, '_> {
    fn spawn_voice(
        &mut self,
        mut voice: Voice,
        width: ChannelLayout,
        timeline: Arc<dyn Timeline>,
    ) -> EntityCommands<'_> {
        voice.replace_transport(timeline);
        let mut e = self.spawn_audio_node(VoiceNode::with_channels(voice, width));
        e.insert(SamplerVoice);
        e
    }
}

/// Build an in-memory voice from a decoded wave.
///
/// The `Residency::Whole` half of the tier decision, expressed as a function so
/// a host writes one line rather than assembling a `Voice` by hand. The disk
/// half needs a butler round-trip and so is not a pure function — see
/// [`DiskStreamerRes`](super::DiskStreamerRes).
pub fn memory_voice(wave: Arc<tutti_core::Wave>, width: ChannelLayout, play: Playback) -> Voice {
    let source = MemorySource::with_channels(wave, width);
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
/// **Deliberately not a function of [`AudioConfig`].** The first draft took one
/// and never read it: the device's width is not this node's business — the graph
/// root folds whatever reaches it, and a voice narrowed to the device here would
/// be narrowed *twice* on a wider project. The only ceiling that applies is the
/// sampler's own.
pub fn voice_width(file: ChannelLayout) -> ChannelLayout {
    let n = file
        .count()
        .clamp(1, tutti_sampler::MAX_SAMPLER_CHANNELS as u16);
    ChannelLayout::from(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::Wave;

    #[test]
    fn voice_width_clamps_to_what_the_graph_can_carry() {
        // A stereo file stays stereo.
        assert_eq!(voice_width(ChannelLayout::Stereo).count(), 2);
        // Wider than the sampler's ceiling comes back at the ceiling, not
        // truncated silently downstream at the root's fold.
        let absurd = ChannelLayout::from(64usize);
        assert_eq!(
            voice_width(absurd).count() as usize,
            tutti_sampler::MAX_SAMPLER_CHANNELS
        );
    }

    #[test]
    fn a_zero_width_file_becomes_mono_not_a_node_nobody_can_wire() {
        assert_eq!(voice_width(ChannelLayout::from(0usize)).count(), 1);
    }

    #[test]
    fn a_memory_voice_carries_no_butler_channel() {
        let wave = Arc::new(Wave::with_capacity(2, 48_000.0, 128));
        let v = memory_voice(wave, ChannelLayout::Stereo, Playback::default());
        assert!(
            v.channel_index.is_none(),
            "the butler channel is a disk-tier concept; `Voice`'s doc calls it \
             meaningless for Memory"
        );
        assert!(matches!(v.source, VoiceSource::Memory(_)));
    }
}
