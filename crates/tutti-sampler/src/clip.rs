//! [`Clip`]: one sample placed on the timeline, built in a line and added to a
//! graph as a node, resident in memory or streamed from disk.

use std::sync::Arc;

use tutti_codec::Wave;
use tutti_graph::{HasParam, IntoNode, NodeParts};
use tutti_transport::TempoReader;
use tutti_types::{Amplitude, Beat, BeatDuration, ChannelLayout, PlaybackRate};

use crate::streamer::{Stream, StreamReader};
use crate::{
    DiskVoice, Loop, MemorySource, PlaceError, Playback, Voice, VoiceNode, VoiceNodeHandle,
    VoiceSource, VoiceWindow, voice_width,
};

/// A sample placed on the timeline: where it starts, how loud, and whether it
/// loops. It is a graph node as it stands, so a user never assembles a
/// [`Voice`], a [`VoiceSource`] or a
/// [`VoiceWindow`]:
///
/// ```ignore
/// let (kick, _) = session.graph().add(
///     Clip::new(wave)?.at(Beat(4.0)).gain(Amplitude::new(0.8)),
/// );
/// ```
///
/// It plays on the transport, from `at` (beat 0 until set) to the end of the
/// sample or the [`lasting`](Self::lasting) length. A node over a clip reads
/// the session's tempo map when the session's graph adds it
/// (`tutti-session`'s `SessionNode`).
///
/// # Two tiers, one clip
///
/// A clip is **resident** ([`new`](Self::new): the decoded wave, in memory)
/// or **streamed** ([`streaming`](Self::streaming): a stream the disk
/// streamer reads ahead, for a file too long to hold). Every builder method
/// reads the same on both, and both play what the file holds at the same
/// frames. The sampler never picks the tier: the caller does, by the
/// constructor it calls, and the session does for a file it loads
/// (`Session::load_clip`, which decides by the file's size).
///
/// A resident clip's width is the wave's own (placed by the wave's
/// [`topology`](Wave::topology), as every [`MemorySource`] places it; at most
/// the sampler's widest), until [`channels`](Self::channels) places it at
/// another. A streamed clip's is its stream's, which its open chose.
///
/// A clip is a [`VoiceNode`]'s: its controls are the node's
/// [`VoiceNodeHandle`], which also owns a streamed clip's stream, so the clip
/// sounds for as long as the node is in its graph.
///
/// `Clip` is a thin builder over [`VoiceNode`], which stays the doorway for
/// what a clip does not say (a pool of voices: [`VoicePool`](crate::VoicePool)).
#[derive(Debug)]
#[must_use = "a clip is a node: add it to a graph"]
pub struct Clip {
    tier: Tier,
    window: VoiceWindow,
    gain: Amplitude,
    speed: PlaybackRate,
    loop_: Option<Loop>,
    tempo: Option<TempoReader>,
}

/// What a clip plays.
#[derive(Debug)]
enum Tier {
    /// The decoded wave, placed.
    Resident {
        wave: Arc<Wave>,
        source: Box<MemorySource>,
    },
    /// A stream and the reader the streamer feeds.
    Streamed {
        reader: Box<StreamReader>,
        stream: Stream,
    },
}

impl Clip {
    fn over(tier: Tier) -> Self {
        Self {
            tier,
            window: VoiceWindow::default(),
            gain: Amplitude::new(1.0),
            speed: PlaybackRate::UNITY,
            loop_: None,
            tempo: None,
        }
    }

    /// A resident clip of `wave`, at beat 0, at unit gain, playing once.
    ///
    /// # Errors
    ///
    /// [`PlaceError`] for a wave with a channel no speaker names (a second
    /// LFE, a wide or high speaker): refused here, never played with a
    /// channel in the wrong place.
    pub fn new(wave: impl Into<Arc<Wave>>) -> Result<Self, PlaceError> {
        let wave = wave.into();
        let width = voice_width(ChannelLayout::clamped(wave.channels()));
        Ok(Self::over(Tier::Resident {
            source: Box::new(MemorySource::with_channels(Arc::clone(&wave), width)?),
            wave,
        }))
    }

    /// A streamed clip over `stream` and the `reader` it opened with (a
    /// [`DiskStreamer::open`](crate::DiskStreamer::open) or
    /// `Session::open_stream`), at beat 0, at unit gain, playing once. The
    /// clip owns the stream: it ends when the clip's node leaves the graph.
    pub fn streaming(reader: StreamReader, stream: Stream) -> Self {
        Self::over(Tier::Streamed {
            reader: Box::new(reader),
            stream,
        })
    }

    /// Whether the clip streams from disk (`false`: it is resident).
    #[must_use]
    pub fn is_streamed(&self) -> bool {
        matches!(self.tier, Tier::Streamed { .. })
    }

    /// Places a resident clip at `channels` wide, through the wave's topology
    /// (a mono wave into stereo plays on the centre of both, a stereo wave
    /// into mono folds). Gain, speed and loop set so far are kept.
    ///
    /// # Errors
    ///
    /// [`PlaceError`] when the wave has no place in that width (a channel no
    /// speaker names), or [`PlaceError::Streamed`] for a streamed clip, whose
    /// width is its stream's (`DiskStreamer::open_into` chooses it).
    pub fn channels(mut self, channels: impl Into<ChannelLayout>) -> Result<Self, PlaceError> {
        let Tier::Resident { wave, source } = &mut self.tier else {
            return Err(PlaceError::Streamed);
        };
        **source = MemorySource::with_channels(Arc::clone(wave), channels)?;
        Ok(self)
    }

    /// Starts the clip at `start` on the timeline.
    pub fn at(mut self, start: Beat) -> Self {
        self.window.start = start;
        self
    }

    /// Plays at most `length` beats of the sample from its start; the whole
    /// sample without it.
    pub fn lasting(mut self, length: BeatDuration) -> Self {
        self.window.duration = Some(length);
        self
    }

    /// The clip's level, linear (unity is `Amplitude::new(1.0)`).
    pub fn gain(mut self, gain: Amplitude) -> Self {
        self.gain = gain;
        self
    }

    /// The clip's varispeed (pitch follows the rate).
    pub fn speed(mut self, speed: PlaybackRate) -> Self {
        self.speed = speed;
        self
    }

    /// Loops the clip over `looped`; a clip plays once without it.
    pub fn looped(mut self, looped: Loop) -> Self {
        self.loop_ = Some(looped);
        self
    }

    /// This clip reading the project's tempo map through `tempo`, as
    /// [`VoiceNode::with_tempo`].
    pub fn with_tempo(mut self, tempo: TempoReader) -> Self {
        self.tempo = Some(tempo);
        self
    }

    /// The node this clip is, placed and set.
    fn into_node(self) -> VoiceNode {
        let Self {
            tier,
            window,
            gain,
            speed,
            loop_,
            tempo,
        } = self;
        let play = Playback {
            gain,
            speed,
            loop_,
            ..Playback::default()
        };
        let node = match tier {
            Tier::Resident { mut source, .. } => {
                source.set_window(window);
                source.set_gain(gain);
                source.set_speed(speed);
                source.set_loop(loop_);
                let width = source.channels();
                VoiceNode::with_channels(
                    Voice {
                        source: VoiceSource::Memory(*source),
                        play,
                    },
                    width,
                )
            }
            Tier::Streamed { reader, stream } => {
                VoiceNode::streaming(DiskVoice::from_reader(*reader, window), stream, play)
            }
        };
        match tempo {
            Some(tempo) => node.with_tempo(tempo),
            None => node,
        }
    }
}

/// A clip has the params of the node it builds.
impl<const P: u16> HasParam<P> for Clip
where
    VoiceNode: HasParam<P>,
{
    type Unit = <VoiceNode as HasParam<P>>::Unit;
}

impl IntoNode for Clip {
    type Controls = VoiceNodeHandle;

    fn into_parts(self) -> NodeParts<VoiceNodeHandle> {
        self.into_node().into_parts()
    }
}
