//! [`ActiveSensing`]: the receiver's watchdog of MIDI 1.0 Detailed
//! Specification 4.2.1 (p. 32), as a pure value stepped with frames.
//!
//! "Use of Active Sensing is optional for either receivers or transmitters.
//! This byte (FE) is sent every 300 ms (maximum) whenever there is no other
//! MIDI data being transmitted. If a device never receives Active Sensing it
//! should operate normally. However, once the receiver recognizes Active
//! Sensing (FE), it then will expect to get a message of some kind every 300
//! milliseconds. If no messages are received within this time period the
//! receiver will assume the MIDI cable has been disconnected for some reason
//! and should turn off all voices and return to normal operation. It is
//! recommended that transmitters transmit Active Sensing within 270 ms and
//! receivers judge at over 330 ms leaving a margin of roughly 10%."
//!
//! The spec's flowchart is two interrupts: reception (any data clears the
//! timer, an FE sets the flag) and the timer (a flag set and over the limit:
//! turn all notes off and reset the flag). This is that, with the timer's
//! clock a value: the caller says at which [`Frame`] each message was
//! [`heard`](ActiveSensing::heard) and asks, at a later frame, which cable has
//! [`lapsed`](ActiveSensing::next_lapse). It reads no clock and holds no
//! thread, so the audio thread can step it with the graph's own frames, and a
//! test steps it with any it likes.
//!
//! **One watch per Group.** A Group is one cable of a device (M2-104 section
//! 2.1.2, Appendix B.1), and the spec's cable is what goes quiet: Group 2's
//! silence is not Group 1's. A Group that never sent an FE is never watched.

use tutti_types::{Frame, SampleRate, Samples, Seconds};
use tutti_ump::MidiGroup;

/// How long a receiver waits for any message once it has seen an Active
/// Sensing before it judges the cable gone: "receivers judge at over 330 ms"
/// (p. 32), the 300 ms of the spec and the roughly 10% margin it recommends.
pub const ACTIVE_SENSING_JUDGE: Seconds = Seconds(0.330);

/// The Active Sensing watch of every Group of one device.
#[derive(Clone, Debug)]
pub struct ActiveSensing {
    /// How long over which a watched Group is judged gone.
    limit: Samples,
    /// For each Group: the frame of the last message it sent, once it has sent
    /// an Active Sensing, `None` while it is not watched.
    watched: [Option<Frame>; MidiGroup::COUNT as usize],
}

impl ActiveSensing {
    /// A watch that judges a Group gone `limit` after its last message.
    #[must_use]
    pub const fn new(limit: Samples) -> Self {
        Self {
            limit,
            watched: [None; MidiGroup::COUNT as usize],
        }
    }

    /// A watch with [`ACTIVE_SENSING_JUDGE`] at `rate`.
    #[must_use]
    pub fn at_rate(rate: SampleRate) -> Self {
        Self::new(ACTIVE_SENSING_JUDGE.to_samples(rate))
    }

    /// How long a watched Group may be silent.
    #[must_use]
    pub const fn limit(&self) -> Samples {
        self.limit
    }

    /// Whether `group` is watched: it has sent an Active Sensing and not
    /// lapsed since.
    #[must_use]
    pub fn is_watched(&self, group: MidiGroup) -> bool {
        self.watched[group.index()].is_some()
    }

    /// A message of `group` arrived at `at`: it clears the timer of a watched
    /// Group, and an Active Sensing (`sensing`) starts watching one. Calls
    /// are in time order; one at an earlier frame than the last is taken as
    /// at the last.
    pub fn heard(&mut self, group: MidiGroup, at: Frame, sensing: bool) {
        let slot = &mut self.watched[group.index()];
        match slot {
            Some(last) => *last = (*last).max(at),
            None if sensing => *slot = Some(at),
            None => {}
        }
    }

    /// The watched Group that ran out of time soonest by `now`, with the
    /// frame it ran out at (its last message plus the limit), if any is
    /// **over** the limit at `now`; that Group is not watched after (the
    /// spec's "reset flag": the receiver returns to normal operation until
    /// the next Active Sensing). Call again for the next, until `None`.
    pub fn next_lapse(&mut self, now: Frame) -> Option<(MidiGroup, Frame)> {
        let (index, deadline) = self
            .watched
            .iter()
            .enumerate()
            .filter_map(|(i, last)| Some((i, (*last)? + self.limit)))
            .filter(|&(_, deadline)| now > deadline)
            .min_by_key(|&(_, deadline)| deadline)?;
        self.watched[index] = None;
        Some((MidiGroup::masked(index as u8), deadline))
    }

    /// Stops watching `group` (its source reset, or went).
    pub fn forget(&mut self, group: MidiGroup) {
        self.watched[group.index()] = None;
    }

    /// Stops watching every Group.
    pub fn forget_all(&mut self) {
        self.watched = [None; MidiGroup::COUNT as usize];
    }
}

#[cfg(test)]
mod tests;
