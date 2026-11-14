//! [`MachineControlSender`]: the MIDI Machine Control commands that make a
//! device follow this transport (RP-013 pp.20 and 28), from what the transport
//! did between two looks at it.
//!
//! The sender holds no clock and sends nothing: each [`observe`](MachineControlSender::observe)
//! takes the playhead as the control side reads it and returns the commands
//! the change since the last look calls for.
//!
//! - A transport that started sends **PLAY**; one that stopped, **STOP**.
//! - A position that is not where rolling or standing still would have left it
//!   is a locate: **LOCATE (target)** to its time code, and while the transport
//!   rolls (or starts rolling in the same look) **DEFERRED PLAY** after it, so
//!   the device plays once it has got there.
//! - The first look sends nothing: there is no change to report.
//!
//! A stop that also moved the position sends STOP alone. Commands are placed at
//! a look, not on a frame: MMC carries no time of its own.

use tutti_midi_protocol::{MmcCommand, MmcCommands, MmcLocate, MmcPosition, MtcRate, Recipient};
use tutti_types::{Frame, SampleRate, Seconds};

/// What the transport was at a look.
#[derive(Clone, Copy, Debug)]
struct Seen {
    playing: bool,
    seconds: Seconds,
    frame: Frame,
}

/// Turns the transport's changes into Machine Control commands.
///
/// ```
/// use tutti_midi_protocol::{MtcRate, Recipient};
/// use tutti_midi_runtime::MachineControlSender;
/// use tutti_types::{Frame, SampleRate, Seconds};
///
/// let mut sender = MachineControlSender::new(Recipient::AllCall, MtcRate::Fps30);
/// let rate = SampleRate::SR_48K;
/// assert!(sender.observe(false, Seconds::new(0.0), Frame::new(0), rate).is_none());
/// let started = sender.observe(true, Seconds::new(0.0), Frame::new(512), rate).unwrap();
/// assert_eq!(started, [0x7F, 0x7F, 0x06, 0x02]);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct MachineControlSender {
    to: Recipient,
    rate: MtcRate,
    last: Option<Seen>,
}

impl MachineControlSender {
    /// How far a position may be from where it should be before it is a locate:
    /// a millisecond (tutti's choice: it is far over the rounding of a
    /// position, and far under a frame).
    pub const LOCATE_TOLERANCE: Seconds = Seconds::new(0.001);

    /// A sender to `to` that states time codes at `rate`.
    #[must_use]
    pub fn new(to: Recipient, rate: MtcRate) -> Self {
        Self {
            to,
            rate,
            last: None,
        }
    }

    /// Where the commands go.
    #[must_use]
    pub fn to(&self) -> Recipient {
        self.to
    }

    /// Looks at the transport: whether it `playing`, its position in `seconds`
    /// and the graph `frame` count the look was made at, on a graph at `rate`.
    /// The payload of the message the change since the last look calls for, if
    /// any: pack it with `Sysex7Packets::from_payload`.
    pub fn observe(
        &mut self,
        playing: bool,
        seconds: Seconds,
        frame: Frame,
        rate: SampleRate,
    ) -> Option<Vec<u8>> {
        let now = Seen {
            playing,
            seconds,
            frame,
        };
        let before = self.last.replace(now)?;
        let mut commands = MmcCommands::new();
        if before.playing && !playing {
            commands.push(&MmcCommand::Stop).ok()?;
            return Some(commands.payload(self.to));
        }
        let expected = if before.playing {
            before.seconds
                + Frame::new(frame.get().saturating_sub(before.frame.get())).to_seconds(rate)
        } else {
            before.seconds
        };
        let moved = (now.seconds - expected).get().abs() > Self::LOCATE_TOLERANCE.get();
        if moved && let Some(target) = MmcPosition::from_seconds(now.seconds, self.rate) {
            commands
                .push(&MmcCommand::Locate(MmcLocate::Target(target)))
                .ok()?;
            if playing {
                commands.push(&MmcCommand::DeferredPlay).ok()?;
            }
        } else if playing && !before.playing {
            commands.push(&MmcCommand::Play).ok()?;
        }
        (!commands.is_empty()).then(|| commands.payload(self.to))
    }
}
