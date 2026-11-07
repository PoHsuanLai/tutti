//! Channel pitch bend: a position and a sensitivity for each of the sixteen
//! channels. A fixed array, so setting one allocates nothing.

use std::ops::Range;
use tutti_types::Semitones;
use tutti_ump::MidiChannel;

const CHANNELS: usize = MidiChannel::COUNT as usize;

#[derive(Debug, Clone, Copy)]
struct ChannelBend {
    /// The wheel, `-1..=1`.
    position: f32,
    /// What a full wheel travel spans.
    range: Semitones,
}

/// The bend of every channel.
#[derive(Debug, Clone)]
pub(crate) struct ChannelBends([ChannelBend; CHANNELS]);

impl ChannelBends {
    /// Every channel centred, with `range` as its sensitivity.
    pub(crate) fn new(range: Semitones) -> Self {
        Self(
            [ChannelBend {
                position: 0.0,
                range,
            }; CHANNELS],
        )
    }

    /// The channels a message on `channel` reaches: its own, or all of them
    /// under MPE, where the master channel's message bends the whole zone and
    /// the synth does not know the zone's layout.
    fn reach(channel: MidiChannel, all: bool) -> Range<usize> {
        if all {
            0..CHANNELS
        } else {
            let i = usize::from(channel.get());
            i..i + 1
        }
    }

    pub(crate) fn set_position(&mut self, channel: MidiChannel, all: bool, position: f32) {
        for bend in &mut self.0[Self::reach(channel, all)] {
            bend.position = position;
        }
    }

    pub(crate) fn set_range(&mut self, channel: MidiChannel, all: bool, range: Semitones) {
        for bend in &mut self.0[Self::reach(channel, all)] {
            bend.range = range;
        }
    }

    /// Back to centre; the sensitivity stays (RP-015 leaves registered
    /// parameters alone).
    pub(crate) fn centre(&mut self, channel: MidiChannel, all: bool) {
        self.set_position(channel, all, 0.0);
    }

    /// The frequency ratio the channel's bend applies.
    pub(crate) fn ratio(&self, channel: MidiChannel) -> f32 {
        let bend = self.0[usize::from(channel.get())];
        (bend.range * bend.position).to_pitch_ratio()
    }

    #[cfg(test)]
    pub(crate) fn position(&self, channel: MidiChannel) -> f32 {
        self.0[usize::from(channel.get())].position
    }
}
