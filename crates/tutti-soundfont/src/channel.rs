// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate.

/// The amount of the default modulators CC 91 to reverb send and CC 93 to
/// chorus send (SF2 2.04 sections 8.4.8 and 8.4.9): 200 tenths of a percent,
/// which is 20% of full level at the controller's top.
const SEND_AMOUNT: f32 = 0.2;

#[derive(Debug, Clone, PartialEq, Eq)]
enum DataType {
    None,
    Rpn,
    Nrpn,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) struct Channel {
    pub(crate) is_percussion_channel: bool,

    bank_number: i32,
    patch_number: i32,

    modulation: i16,
    volume: i16,
    pan: i16,
    expression: i16,

    reverb_send: u8,
    chorus_send: u8,

    rpn: i16,
    pitch_bend_range: i16,
    coarse_tune: i16,
    fine_tune: i16,

    pitch_bend: f32,

    last_data_type: DataType,
}

impl Channel {
    pub(crate) fn new(is_percussion_channel: bool) -> Self {
        let mut channel = Self {
            is_percussion_channel,
            bank_number: 0,
            patch_number: 0,
            modulation: 0,
            volume: 0,
            pan: 0,
            expression: 0,
            reverb_send: 0,
            chorus_send: 0,
            rpn: 0,
            pitch_bend_range: 0,
            coarse_tune: 0,
            fine_tune: 0,
            pitch_bend: 0_f32,
            last_data_type: DataType::None,
        };

        channel.reset();

        channel
    }

    pub(crate) fn reset(&mut self) {
        self.bank_number = if self.is_percussion_channel { 128 } else { 0 };
        self.patch_number = 0;

        self.modulation = 0;
        self.volume = 100 << 7;
        self.pan = 64 << 7;
        self.expression = 127 << 7;

        self.reverb_send = 40;
        self.chorus_send = 0;

        self.rpn = -1;
        self.pitch_bend_range = 2 << 7;
        self.coarse_tune = 0;
        self.fine_tune = 8192;

        self.pitch_bend = 0_f32;
    }

    pub(crate) fn reset_all_controllers(&mut self) {
        self.modulation = 0;
        self.expression = 127 << 7;

        self.rpn = -1;

        self.pitch_bend = 0_f32;
    }

    pub(crate) fn set_bank(&mut self, value: i32) {
        self.bank_number = value;

        if self.is_percussion_channel {
            self.bank_number += 128;
        }
    }

    pub(crate) fn set_patch(&mut self, value: i32) {
        self.patch_number = value;
    }

    pub(crate) fn set_modulation_coarse(&mut self, value: i32) {
        self.modulation = (self.modulation & 0x7F) | (value << 7) as i16;
    }

    pub(crate) fn set_modulation_fine(&mut self, value: i32) {
        self.modulation = (((self.modulation as i32) & 0xFF80) | value) as i16;
    }

    pub(crate) fn set_volume_coarse(&mut self, value: i32) {
        self.volume = (self.volume & 0x7F) | (value << 7) as i16;
    }

    pub(crate) fn set_volume_fine(&mut self, value: i32) {
        self.volume = (((self.volume as i32) & 0xFF80) | value) as i16;
    }

    pub(crate) fn set_pan_coarse(&mut self, value: i32) {
        self.pan = (self.pan & 0x7F) | (value << 7) as i16;
    }

    pub(crate) fn set_pan_fine(&mut self, value: i32) {
        self.pan = (((self.pan as i32) & 0xFF80) | value) as i16;
    }

    pub(crate) fn set_expression_coarse(&mut self, value: i32) {
        self.expression = (self.expression & 0x7F) | (value << 7) as i16;
    }

    pub(crate) fn set_expression_fine(&mut self, value: i32) {
        self.expression = (((self.expression as i32) & 0xFF80) | value) as i16;
    }

    pub(crate) fn set_reverb_send(&mut self, value: i32) {
        self.reverb_send = value as u8;
    }

    pub(crate) fn set_chorus_send(&mut self, value: i32) {
        self.chorus_send = value as u8;
    }

    pub(crate) fn set_rpn_coarse(&mut self, value: i32) {
        self.rpn = (self.rpn & 0x7F) | (value << 7) as i16;
        self.last_data_type = DataType::Rpn;
    }

    pub(crate) fn set_rpn_fine(&mut self, value: i32) {
        self.rpn = (((self.rpn as i32) & 0xFF80) | value) as i16;
        self.last_data_type = DataType::Rpn;
    }

    pub(crate) fn set_nrpn_coarse(&mut self, _value: i32) {
        self.last_data_type = DataType::Nrpn;
    }

    pub(crate) fn set_nrpn_fine(&mut self, _value: i32) {
        self.last_data_type = DataType::Nrpn;
    }

    pub(crate) fn data_entry_coarse(&mut self, value: i32) {
        if self.last_data_type != DataType::Rpn {
            return;
        }

        if self.rpn == 0 {
            self.pitch_bend_range = (self.pitch_bend_range & 0x7F) | (value << 7) as i16;
        } else if self.rpn == 1 {
            self.fine_tune = (self.fine_tune & 0x7F) | (value << 7) as i16;
        } else if self.rpn == 2 {
            self.coarse_tune = (value - 64) as i16;
        }
    }

    pub(crate) fn data_entry_fine(&mut self, value: i32) {
        if self.last_data_type != DataType::Rpn {
            return;
        }

        if self.rpn == 0 {
            self.pitch_bend_range = (((self.pitch_bend_range as i32) & 0xFF80) | value) as i16;
        } else if self.rpn == 1 {
            self.fine_tune = (((self.fine_tune as i32) & 0xFF80) | value) as i16;
        }
    }

    pub(crate) fn set_pitch_bend(&mut self, value1: i32, value2: i32) {
        self.pitch_bend = (1_f32 / 8192_f32) * ((value1 | (value2 << 7)) - 8192) as f32;
    }

    pub(crate) fn bank_number(&self) -> i32 {
        self.bank_number
    }

    pub(crate) fn patch_number(&self) -> i32 {
        self.patch_number
    }

    pub(crate) fn modulation(&self) -> f32 {
        (50_f32 / 16383_f32) * self.modulation as f32
    }

    pub(crate) fn volume(&self) -> f32 {
        (1_f32 / 16383_f32) * self.volume as f32
    }

    pub(crate) fn pan(&self) -> f32 {
        (100_f32 / 16383_f32) * self.pan as f32 - 50_f32
    }

    pub(crate) fn expression(&self) -> f32 {
        (1_f32 / 16383_f32) * self.expression as f32
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

    pub(crate) fn pitch_bend_range(&self) -> f32 {
        (self.pitch_bend_range >> 7) as f32 + 0.01_f32 * (self.pitch_bend_range & 0x7F) as f32
    }

    pub(crate) fn tune(&self) -> f32 {
        self.coarse_tune as f32 + (1_f32 / 8192_f32) * (self.fine_tune - 8192) as f32
    }

    pub(crate) fn pitch_bend(&self) -> f32 {
        self.pitch_bend_range() * self.pitch_bend
    }
}
