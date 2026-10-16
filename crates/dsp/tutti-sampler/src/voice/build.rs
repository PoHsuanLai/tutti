//! Building a [`Voice`] from what a host has: a decoded wave, and the width a
//! file declares.

use std::sync::Arc;

use tutti_core::ChannelLayout;

use super::memory_source::{MemorySource, VoiceWindow};
use super::types::{Playback, Voice, VoiceSource};
use crate::{Wave, MAX_SAMPLER_CHANNELS};

/// Builds an in-memory voice from a decoded wave.
///
/// The `Residency::Whole` half of the tier decision, expressed as a function so
/// a host writes one line rather than assembling a `Voice` by hand. The disk
/// half needs a butler round-trip and so is not a pure function: it goes
/// through a [`DiskStreamer`](crate::DiskStreamer).
///
/// # `window` is required, not defaulted
///
/// It is the clip's placement on the timeline. A voice built without one
/// would play at the default position regardless of where the clip is placed,
/// so it is taken here rather than defaulted.
pub fn memory_voice(
    wave: Arc<Wave>,
    width: ChannelLayout,
    play: Playback,
    window: VoiceWindow,
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
/// Clamped to [`MAX_SAMPLER_CHANNELS`] because that is the sampler's own
/// per-read stack ceiling — deliberately equal to the graph root's, since a
/// voice wider than the root can render is a voice nobody can hear.
///
/// A zero-width file is coerced to mono rather than rejected: `ChannelLayout` can
/// represent an empty bus (a plugin port genuinely can be zero wide), but a graph
/// node that reports zero outputs is one nothing can be wired to.
///
/// **Deliberately not a function of the device width.** The graph root folds
/// whatever reaches it, so a voice narrowed to the device here would be narrowed
/// *twice* on a wider project. The only ceiling that applies is the sampler's.
pub fn voice_width(file: ChannelLayout) -> ChannelLayout {
    let n = file.count().clamp(1, MAX_SAMPLER_CHANNELS as u16);
    ChannelLayout::from(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `voice_width` is the file's own width clamped to `1..=MAX_SAMPLER_CHANNELS`
    /// — the device is deliberately not consulted, or a voice would be narrowed
    /// twice on a wider project.
    #[test]
    fn voice_width_clamps_to_what_the_graph_can_carry() {
        let ceiling = MAX_SAMPLER_CHANNELS;
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
