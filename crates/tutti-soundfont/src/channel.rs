// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_ump::{U7, U14};

/// The amount of the default modulators CC 91 to reverb send and CC 93 to
/// chorus send (SF2 2.04 sections 8.4.8 and 8.4.9): 200 tenths of a percent,
/// which is 20% of full level at the controller's top.
const SEND_AMOUNT: f32 = 0.2;

/// A 14-bit controller with its most significant 7 bits replaced (CC 0 to 31).
/// The least significant 7 bits go to zero: M1 p.12, "When an MSB is received,
/// the receiver should set its concept of the LSB to zero."
fn with_msb(msb: U7) -> U14 {
    U14::masked(u16::from(msb.get()) << 7)
}

/// A 14-bit controller with its least significant 7 bits replaced (CC 32 to 63).
fn with_lsb(current: U14, lsb: U7) -> U14 {
    U14::masked((current.get() & 0x3F80) | u16::from(lsb.get()))
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct Channel {
    pub(crate) is_percussion_channel: bool,

    bank_number: i32,
    /// The Bank Select (CC 0 and 32) received and not yet applied: M1 p.13,
    /// "the receiving device must remember that bank number in readiness for
    /// the following Program Change". The next Program Change makes it the
    /// `bank_number`.
    pending_bank: U14,
    patch_number: i32,

    modulation: U14,
    volume: U14,
    pan: U14,
    expression: U14,

    reverb_send: u8,
    chorus_send: u8,

    coarse_tune: i16,
    fine_tune: i16,
}

impl Channel {
    pub(crate) fn new(is_percussion_channel: bool) -> Self {
        let mut channel = Self {
            is_percussion_channel,
            bank_number: 0,
            pending_bank: U14::of(0),
            patch_number: 0,
            modulation: U14::of(0),
            volume: U14::of(0),
            pan: U14::of(0),
            expression: U14::of(0),
            reverb_send: 0,
            chorus_send: 0,
            coarse_tune: 0,
            fine_tune: 0,
        };

        channel.reset();

        channel
    }

    pub(crate) fn reset(&mut self) {
        self.bank_number = if self.is_percussion_channel { 128 } else { 0 };
        self.pending_bank = U14::of(0);
        self.patch_number = 0;

        self.modulation = U14::of(0);
        self.volume = with_msb(U7::of(100));
        self.pan = with_msb(U7::of(64));
        self.expression = with_msb(U7::of(127));

        self.reverb_send = 40;
        self.chorus_send = 0;

        self.coarse_tune = 0;
        self.fine_tune = 8192;
    }

    pub(crate) fn reset_all_controllers(&mut self) {
        self.modulation = U14::of(0);
        self.expression = with_msb(U7::of(127));
    }

    /// Bank Select, coarse (CC 0): held for the next Program Change, and the
    /// pending LSB goes to zero (M1 p.12).
    pub(crate) fn select_bank_coarse(&mut self, value: U7) {
        self.pending_bank = with_msb(value);
    }

    /// Bank Select, fine (CC 32): held for the next Program Change.
    pub(crate) fn select_bank_fine(&mut self, value: U7) {
        self.pending_bank = with_lsb(self.pending_bank, value);
    }

    /// A Program Change: the held bank takes effect (M1 p.13), by its MSB (a
    /// SoundFont's bank is one number, which MIDI sends as the MSB), and the
    /// program is selected.
    pub(crate) fn program_change(&mut self, program: U7) {
        self.bank_number = i32::from(self.pending_bank.get() >> 7);

        if self.is_percussion_channel {
            self.bank_number += 128;
        }
        self.patch_number = i32::from(program.get());
    }

    pub(crate) fn set_modulation_coarse(&mut self, value: U7) {
        self.modulation = with_msb(value);
    }

    pub(crate) fn set_modulation_fine(&mut self, value: U7) {
        self.modulation = with_lsb(self.modulation, value);
    }

    pub(crate) fn set_volume_coarse(&mut self, value: U7) {
        self.volume = with_msb(value);
    }

    pub(crate) fn set_volume_fine(&mut self, value: U7) {
        self.volume = with_lsb(self.volume, value);
    }

    pub(crate) fn set_pan_coarse(&mut self, value: U7) {
        self.pan = with_msb(value);
    }

    pub(crate) fn set_pan_fine(&mut self, value: U7) {
        self.pan = with_lsb(self.pan, value);
    }

    pub(crate) fn set_expression_coarse(&mut self, value: U7) {
        self.expression = with_msb(value);
    }

    pub(crate) fn set_expression_fine(&mut self, value: U7) {
        self.expression = with_lsb(self.expression, value);
    }

    pub(crate) fn set_reverb_send(&mut self, value: i32) {
        self.reverb_send = value as u8;
    }

    pub(crate) fn set_chorus_send(&mut self, value: i32) {
        self.chorus_send = value as u8;
    }

    /// A Registered Controller of bank 0 (M2-104 7.4.13): fine tuning (1) and
    /// coarse tuning (2); any other is ignored (pitch bend sensitivity, 0, is
    /// the note logic's, `Voicing::step`). The data is the MIDI 1.0 pair zero-extended (M2-115 section
    /// 4.1, as the translator sends it): `MSB << 7 | LSB` is its top 14 bits.
    pub(crate) fn set_registered(&mut self, index: u8, value: u32) {
        let data = (value >> 18) as i16;
        match index {
            1 => self.fine_tune = data,
            2 => self.coarse_tune = (data >> 7) - 64,
            _ => {}
        }
    }

    pub(crate) fn bank_number(&self) -> i32 {
        self.bank_number
    }

    pub(crate) fn patch_number(&self) -> i32 {
        self.patch_number
    }

    pub(crate) fn modulation(&self) -> f32 {
        (50_f32 / 16383_f32) * f32::from(self.modulation.get())
    }

    pub(crate) fn volume(&self) -> f32 {
        (1_f32 / 16383_f32) * f32::from(self.volume.get())
    }

    pub(crate) fn pan(&self) -> f32 {
        (100_f32 / 16383_f32) * f32::from(self.pan.get()) - 50_f32
    }

    pub(crate) fn expression(&self) -> f32 {
        (1_f32 / 16383_f32) * f32::from(self.expression.get())
    }

    /// What CC 91 adds to a voice's reverb send, as a fraction of full level:
    /// the default modulator of SF2 2.04 section 8.4.8 (a linear positive
    /// unipolar source, so the controller maps to `value / 128`, times an
    /// amount of 200 tenths of a percent).
    pub(crate) fn reverb_send(&self) -> f32 {
        SEND_AMOUNT * (f32::from(self.reverb_send) / 128_f32)
    }

    /// What CC 93 adds to a voice's chorus send: the default modulator of
    /// section 8.4.9, as [`reverb_send`](Self::reverb_send).
    pub(crate) fn chorus_send(&self) -> f32 {
        SEND_AMOUNT * (f32::from(self.chorus_send) / 128_f32)
    }

    pub(crate) fn tune(&self) -> f32 {
        self.coarse_tune as f32 + (1_f32 / 8192_f32) * (self.fine_tune - 8192) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M1 p.12: "When an MSB is received, the receiver should set its concept
    /// of the LSB to zero." After MSB 100, LSB 64, MSB 100 the 14-bit value is
    /// 100 * 128, not 100 * 128 + 64.
    #[test]
    fn an_msb_sets_the_lsb_to_zero() {
        let mut channel = Channel::new(false);
        channel.set_volume_coarse(U7::of(100));
        channel.set_volume_fine(U7::of(64));
        channel.set_volume_coarse(U7::of(100));
        assert_eq!(channel.volume(), (1_f32 / 16383_f32) * (100 * 128) as f32);
    }
}
