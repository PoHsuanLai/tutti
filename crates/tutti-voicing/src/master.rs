//! What the Universal Device Control messages set: the device's own volume,
//! balance and tuning, apart from every channel's.
//!
//! [`Master`] is the value [`Voicing`](crate::Voicing) keeps. The instrument
//! reads two things from it when it renders: the [`MasterGain`] its output is
//! multiplied by (`Voicing::master_gain`), and, through each channel's pitch,
//! the tuning (the core adds it to what `Command::Retune` says).
//!
//! # Volume and balance
//!
//! M1 p.57: a device internally tracks three volume and two balance
//! scalars: the volume received on its own ID, the one received on the All
//! Call ID, and the channels' (CC 7 and 11, [`ChannelState`](crate::ChannelState)),
//! and for balance the first two. So each is kept separately and the gain is
//! their product, so all devices can be brought down together without
//! forgetting their individual levels. The own-ID scalars start at full volume
//! and centre (the power-up default is full volume), and
//! so do the all-call ones (GM2 section 4.1, default 7F/7F).
//!
//! **The volume's curve** is GM2 section 4.1's: the volume is proportional to the
//! square of the value, as for CC 7 and CC 11, so the amplitude is
//! `(value / 16383)^2` and `0` is off. **The balance's curve** is the one the
//! spec names (the effect of a balance control on a stereo amplifier) and
//! does not give: the knob attenuates the side it turns away from, linearly in
//! amplitude, and leaves the other at unity: `0x2000`, the centre, is unity on
//! both sides, `0` is the right side off and `0x3FFF` the left side off.
//!
//! # Tuning
//!
//! CA-025: Master Fine Tuning is 100/8192 cents a step with `0x2000` the
//! centre, and Master Coarse Tuning a semitone a step with `0x40` the centre;
//! the total displacement in cents from A440 for each MIDI channel is the sum
//! of the Master Tuning displacement and that of Fine Tuning using RPN.

use tutti_types::{Amplitude, Semitones};
use tutti_ump::{U7, U14};

use tutti_midi_protocol::Addressee;

/// The centre of Master Fine Tuning and of Master Balance (`40 00`).
const CENTRE_14: u16 = 0x2000;
/// The centre of Master Coarse Tuning's MSB (`0x40`).
const CENTRE_7: u8 = 0x40;

/// The gain a device's output is multiplied by: the Master Volume on both
/// sides, and the Master Balance's attenuation of one of them. Unity until a
/// Universal message says otherwise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MasterGain {
    /// The left side's gain.
    pub left: Amplitude,
    /// The right side's gain.
    pub right: Amplitude,
}

impl MasterGain {
    /// No change: the defaults.
    pub const UNITY: Self = Self {
        left: Amplitude::UNITY,
        right: Amplitude::UNITY,
    };
}

/// The device's Master Volume, Balance and Tuning. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Master {
    /// Volume, `[all-call, own]`.
    volume: [U14; 2],
    /// Balance, `[all-call, own]`.
    balance: [U14; 2],
    fine: U14,
    coarse: U7,
}

const fn slot(to: Addressee) -> usize {
    match to {
        Addressee::All => 0,
        Addressee::Own => 1,
    }
}

impl Master {
    /// The power-up state: full volume, centred balance, A440.
    pub(crate) const fn new() -> Self {
        Self {
            volume: [U14::MAX; 2],
            balance: [U14::of(CENTRE_14); 2],
            fine: U14::of(CENTRE_14),
            coarse: U7::of(CENTRE_7),
        }
    }

    /// General MIDI's initial status: what the all-call messages set returns to
    /// its default. The own-ID scalars are the device's knob and stay.
    pub(crate) fn reset_to_general_midi(&mut self) {
        let fresh = Self::new();
        self.volume[0] = fresh.volume[0];
        self.balance[0] = fresh.balance[0];
        self.fine = fresh.fine;
        self.coarse = fresh.coarse;
    }

    pub(crate) fn set_volume(&mut self, to: Addressee, value: U14) {
        self.volume[slot(to)] = value;
    }

    pub(crate) fn set_balance(&mut self, to: Addressee, value: U14) {
        self.balance[slot(to)] = value;
    }

    pub(crate) fn set_fine(&mut self, value: U14) {
        self.fine = value;
    }

    pub(crate) fn set_coarse(&mut self, value: U7) {
        self.coarse = value;
    }

    /// The gain of both sides.
    pub(crate) fn gain(&self) -> MasterGain {
        let volume: f32 = self.volume.iter().map(|&v| volume_gain(v)).product();
        let side = |balanced: fn(U14) -> f32| -> Amplitude {
            Amplitude(volume * self.balance.iter().map(|&b| balanced(b)).product::<f32>())
        };
        MasterGain {
            left: side(left_gain),
            right: side(right_gain),
        }
    }

    /// The displacement from A440: the coarse and the fine tuning.
    pub(crate) fn tuning(&self) -> Semitones {
        let fine = (f32::from(self.fine.get()) - f32::from(CENTRE_14)) / f32::from(CENTRE_14);
        Semitones(f32::from(self.coarse.get()) - f32::from(CENTRE_7) + fine)
    }
}

/// `(value / 16383)^2`.
fn volume_gain(value: U14) -> f32 {
    let x = f32::from(value.get()) / f32::from(U14::MAX.get());
    x * x
}

/// The left side: unity up to the centre, then falling linearly to 0 at the top.
fn left_gain(balance: U14) -> f32 {
    let b = balance.get();
    if b <= CENTRE_14 {
        1.0
    } else {
        f32::from(U14::MAX.get() - b) / f32::from(U14::MAX.get() - CENTRE_14)
    }
}

/// The right side: 0 at the bottom, rising linearly to unity at the centre.
fn right_gain(balance: U14) -> f32 {
    let b = balance.get();
    if b >= CENTRE_14 {
        1.0
    } else {
        f32::from(b) / f32::from(CENTRE_14)
    }
}
