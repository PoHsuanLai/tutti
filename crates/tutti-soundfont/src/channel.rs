// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

use tutti_ump::{Bank, CCNumber, U7, U14, Voice2};

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

    reverb_send: u8,
    chorus_send: u8,
}

impl Channel {
    pub(crate) fn new(is_percussion_channel: bool) -> Self {
        let mut channel = Self {
            is_percussion_channel,
            bank_number: 0,
            pending_bank: U14::of(0),
            patch_number: 0,
            reverb_send: 0,
            chorus_send: 0,
        };

        channel.reset();

        channel
    }

    pub(crate) fn reset(&mut self) {
        self.bank_number = if self.is_percussion_channel { 128 } else { 0 };
        self.pending_bank = U14::of(0);
        self.patch_number = 0;

        self.reverb_send = 40;
        self.chorus_send = 0;
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

    /// The most messages [`chase`](Self::chase) writes: a Program Change with
    /// its bank, the Bank Select held for the next one (two controllers) and
    /// the two sends.
    pub(crate) const CHASE_MESSAGES: usize = 5;

    /// Writes what this channel holds that a fresh one does not, as the
    /// messages that set it (the unit's chase, `Node::chase`): the program
    /// and the bank it plays, as one Program Change with the bank riding
    /// along; the Bank Select received and not yet applied, which waits for
    /// the next Program Change (M1 p.13); and the two effect sends.
    pub(crate) fn chase(&self, emit: &mut impl FnMut(Voice2)) {
        let fresh = Self::new(self.is_percussion_channel);
        let cc = |controller: CCNumber, value: u8| Voice2::ControlChange {
            controller,
            value: u32::from(value) << 25,
        };
        // What the receiver holds as Bank Select once the messages so far
        // are read: nothing, then the bank's MSB if a Program Change goes.
        let mut held = fresh.pending_bank;
        if (self.bank_number, self.patch_number) != (fresh.bank_number, fresh.patch_number) {
            // `program_change` made the bank number of the held bank's MSB,
            // plus 128 on the percussion channel.
            let bank = self.bank_number - if self.is_percussion_channel { 128 } else { 0 };
            if let (Some(msb), Some(program)) = (
                u8::try_from(bank).ok().and_then(U7::new),
                u8::try_from(self.patch_number).ok().and_then(U7::new),
            ) {
                emit(Voice2::ProgramChange {
                    program,
                    bank: Some(Bank {
                        msb,
                        lsb: U7::of(0),
                    }),
                });
                held = with_msb(msb);
            }
        }
        if self.pending_bank != held {
            emit(cc(
                CCNumber::BANK_SELECT,
                (self.pending_bank.get() >> 7) as u8,
            ));
            let lsb = (self.pending_bank.get() & 0x7F) as u8;
            if lsb != 0
                && let Some(controller) = CCNumber::BANK_SELECT.lsb()
            {
                emit(cc(controller, lsb));
            }
        }
        if self.reverb_send != fresh.reverb_send {
            emit(cc(CCNumber::REVERB_SEND, self.reverb_send));
        }
        if self.chorus_send != fresh.chorus_send {
            emit(cc(CCNumber::CHORUS_SEND, self.chorus_send));
        }
    }

    pub(crate) fn set_reverb_send(&mut self, value: i32) {
        self.reverb_send = value as u8;
    }

    pub(crate) fn set_chorus_send(&mut self, value: i32) {
        self.chorus_send = value as u8;
    }

    pub(crate) fn bank_number(&self) -> i32 {
        self.bank_number
    }

    pub(crate) fn patch_number(&self) -> i32 {
        self.patch_number
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
}
