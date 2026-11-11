//! [`Clip`]: one sample placed on the timeline, built in a line and added to a
//! graph as a node.

use std::sync::Arc;

use tutti_codec::Wave;
use tutti_graph::{IntoNode, NodeParts, ParamSet};
use tutti_transport::TempoReader;
use tutti_types::{Amplitude, Beat, BeatDuration, ChannelLayout, PlaybackRate};

use crate::{LoopSetting, MemorySource, PlaceError, VoiceWindow, voice_width};

/// A sample placed on the timeline: the resident wave, where it starts, how
/// loud, and whether it loops. It is a graph node as it stands, so a user
/// never assembles a [`Voice`](crate::Voice), a [`VoiceSource`](crate::VoiceSource)
/// or a [`VoiceWindow`]:
///
/// ```ignore
/// let (kick, _) = session.graph().add(
///     Clip::new(wave)?.at(Beat(4.0)).gain(Amplitude::new(0.8)),
/// );
/// ```
///
/// It plays on the transport, from `at` (beat 0 until set) to the end of the
/// wave or the [`lasting`](Self::lasting) length. Its width is the wave's own
/// (placed by the wave's [`topology`](Wave::topology), as every
/// [`MemorySource`] places it; at most the sampler's widest), until
/// [`channels`](Self::channels) places it at another. A node
/// over a clip reads the session's tempo map when the session's graph adds it
/// (`tutti-session`'s `SessionNode`).
///
/// `Clip` is a thin builder over [`MemorySource`], which stays the doorway for
/// what a clip does not say (a stream, a channel width, a pool of voices:
/// [`memory_voice`](crate::memory_voice)).
#[derive(Debug, Clone)]
#[must_use = "a clip is a node: add it to a graph"]
pub struct Clip {
    wave: Arc<Wave>,
    source: MemorySource,
    window: VoiceWindow,
}

impl Clip {
    /// A clip of `wave`, at beat 0, at unit gain, playing once.
    ///
    /// # Errors
    ///
    /// [`PlaceError`] for a wave with a channel no speaker names (a second
    /// LFE, a wide or high speaker): refused here, never played with a
    /// channel in the wrong place.
    pub fn new(wave: impl Into<Arc<Wave>>) -> Result<Self, PlaceError> {
        let wave = wave.into();
        let width = voice_width(ChannelLayout::clamped(wave.channels()));
        Ok(Self {
            source: MemorySource::with_channels(Arc::clone(&wave), width)?,
            wave,
            window: VoiceWindow::default(),
        })
    }

    /// Places the clip at `channels` wide, through the wave's topology (a mono
    /// wave into stereo plays on the centre of both, a stereo wave into mono
    /// folds). Gain, speed and loop set so far are kept.
    ///
    /// # Errors
    ///
    /// [`PlaceError`] when the wave has no place in that width (a channel no
    /// speaker names).
    pub fn channels(mut self, channels: impl Into<ChannelLayout>) -> Result<Self, PlaceError> {
        let mut placed = MemorySource::with_channels(Arc::clone(&self.wave), channels)?;
        placed.set_gain(self.source.gain());
        placed.set_speed(self.source.speed());
        placed.set_loop_setting(self.source.loop_setting());
        self.source = placed;
        Ok(self)
    }

    /// Starts the clip at `start` on the timeline.
    pub fn at(mut self, start: Beat) -> Self {
        self.window.start = start;
        self
    }

    /// Plays at most `length` beats of the wave from its start; the whole wave
    /// without it.
    pub fn lasting(mut self, length: BeatDuration) -> Self {
        self.window.duration = Some(length);
        self
    }

    /// The clip's level, linear (unity is `Amplitude::new(1.0)`).
    pub fn gain(self, gain: Amplitude) -> Self {
        self.source.set_gain(gain);
        self
    }

    /// The clip's varispeed (pitch follows the rate).
    pub fn speed(mut self, speed: PlaybackRate) -> Self {
        self.source.set_speed(speed);
        self
    }

    /// How the clip loops: [`LoopSetting::Off`] plays once.
    pub fn looped(mut self, setting: LoopSetting) -> Self {
        self.source.set_loop_setting(setting);
        self
    }

    /// This clip reading the project's tempo map through `tempo`, as
    /// [`MemorySource::with_tempo`].
    pub fn with_tempo(mut self, tempo: TempoReader) -> Self {
        self.source = self.source.with_tempo(tempo);
        self
    }

    /// The clip as the [`MemorySource`] it builds, placed.
    pub fn into_source(self) -> MemorySource {
        self.source.placed_at(self.window)
    }
}

impl IntoNode for Clip {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        self.into_source().into_parts()
    }
}
