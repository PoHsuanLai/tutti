use std::cmp;
use std::collections::HashMap;
use std::sync::Arc;

use crate::array_math::ArrayMath;
use crate::channel::Channel;
use crate::chorus::Chorus;
use tutti_sf2::SoundFont;
use tutti_types::{Amplitude, SampleRate, Samples};
use tutti_ump::{CCNumber, MidiChannel, NoteNumber, U7, U14};

use crate::error::SynthesizerError;
use crate::region_pair::RegionPair;
use crate::reverb::Reverb;
use crate::soundfont_math::SoundFontMath;
use crate::synthesizer_settings::SynthesizerSettings;
use crate::voice_collection::VoiceCollection;

/// How a note-off-all ends the voices it reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// Release into the envelope: the voice rings out its release stage.
    Release,
    /// Silence at once.
    Cut,
}

/// SoundFont synthesizer with per-channel MIDI state, voice allocation, and optional reverb/chorus.
#[derive(Debug, Clone)]
pub struct Synthesizer {
    sound_font: Arc<SoundFont>,
    sample_rate: i32,
    block_size: usize,
    maximum_polyphony: usize,

    preset_lookup: HashMap<i32, usize>,
    default_preset: usize,

    channels: Vec<Channel>,

    voices: VoiceCollection,

    block_left: Vec<f32>,
    block_right: Vec<f32>,

    inverse_block_size: f32,

    block_read: usize,

    master_volume: f32,

    effects: Option<Effects>,
}

impl Synthesizer {
    /// The channel General MIDI plays percussion on.
    pub const PERCUSSION_CHANNEL: MidiChannel = MidiChannel::of(9);

    /// Builds a synthesizer over `sound_font`. Allocates the voices and the
    /// effect lines.
    ///
    /// # Errors
    ///
    /// Returns [`SynthesizerError`] if `settings` is out of range.
    pub fn new(
        sound_font: &Arc<SoundFont>,
        settings: &SynthesizerSettings,
    ) -> Result<Self, SynthesizerError> {
        settings.validate()?;

        let mut preset_lookup: HashMap<i32, usize> = HashMap::new();

        let mut min_preset_id = i32::MAX;
        let mut default_preset: usize = 0;
        for (i, preset) in sound_font.presets().iter().enumerate() {
            // The preset ID is Int32, where the upper 16 bits represent the bank number
            // and the lower 16 bits represent the patch number.
            // This ID is used to search for presets by the combination of bank number
            // and patch number.
            let preset_id = (preset.bank_number() << 16) | preset.patch_number();
            preset_lookup.insert(preset_id, i);

            // The preset with the minimum ID number will be default.
            // If the SoundFont is GM compatible, the piano will be chosen.
            if preset_id < min_preset_id {
                default_preset = i;
                min_preset_id = preset_id;
            }
        }

        let channels = (0..MidiChannel::COUNT)
            .map(|i| Channel::new(i == Synthesizer::PERCUSSION_CHANNEL.get()))
            .collect();

        let voices = VoiceCollection::new(settings);

        let block_size = settings.block_len();
        let block_left: Vec<f32> = vec![0_f32; block_size];
        let block_right: Vec<f32> = vec![0_f32; block_size];

        let inverse_block_size = 1_f32 / block_size as f32;

        let block_read = block_size;

        let master_volume = 0.5_f32;

        let effects = if settings.enable_reverb_and_chorus {
            Some(Effects::new(settings))
        } else {
            None
        };

        Ok(Self {
            sound_font: Arc::clone(sound_font),
            sample_rate: settings.hz(),
            block_size,
            maximum_polyphony: settings.maximum_polyphony,
            preset_lookup,
            default_preset,
            channels,
            voices,
            block_left,
            block_right,
            inverse_block_size,
            block_read,
            master_volume,
            effects,
        })
    }

    /// Handles a Control Change.
    ///
    /// Followed: bank select, modulation, data entry, volume, pan,
    /// expression, hold pedal, reverb and chorus send, the RPN and NRPN
    /// selectors, all sound off, reset all controllers and all notes off.
    /// Any other controller is ignored.
    pub fn control_change(&mut self, channel: MidiChannel, controller: CCNumber, value: U7) {
        let data = i32::from(value.get());
        let channel_info = &mut self.channels[usize::from(channel.get())];

        match controller.get() {
            0x00 => channel_info.set_bank(data),
            0x01 => channel_info.set_modulation_coarse(data),
            0x21 => channel_info.set_modulation_fine(data),
            0x06 => channel_info.data_entry_coarse(data),
            0x26 => channel_info.data_entry_fine(data),
            0x07 => channel_info.set_volume_coarse(data),
            0x27 => channel_info.set_volume_fine(data),
            0x0A => channel_info.set_pan_coarse(data),
            0x2A => channel_info.set_pan_fine(data),
            0x0B => channel_info.set_expression_coarse(data),
            0x2B => channel_info.set_expression_fine(data),
            0x40 => channel_info.set_hold_pedal(data),
            0x5B => channel_info.set_reverb_send(data),
            0x5D => channel_info.set_chorus_send(data),
            0x63 => channel_info.set_nrpn_coarse(data),
            0x62 => channel_info.set_nrpn_fine(data),
            0x65 => channel_info.set_rpn_coarse(data),
            0x64 => channel_info.set_rpn_fine(data),
            0x78 => self.note_off_all_channel(channel, Stop::Cut),
            0x79 => self.reset_all_controllers_channel(channel),
            0x7B => self.note_off_all_channel(channel, Stop::Release),
            _ => (),
        }
    }

    /// Selects the preset `channel` plays: `program` within its current bank.
    pub fn program_change(&mut self, channel: MidiChannel, program: U7) {
        self.channels[usize::from(channel.get())].set_patch(i32::from(program.get()));
    }

    /// Bends `channel`'s pitch. `bend` is the 14-bit value, 0x2000 for none.
    pub fn pitch_bend(&mut self, channel: MidiChannel, bend: U14) {
        self.channels[usize::from(channel.get())]
            .set_pitch_bend(i32::from(bend.lsb().get()), i32::from(bend.msb().get()));
    }

    /// Releases every voice sounding `key` on `channel` into its envelope.
    pub fn note_off(&mut self, channel: MidiChannel, key: NoteNumber) {
        let (channel, key) = (i32::from(channel.get()), i32::from(key.get()));
        for voice in self.voices.get_active_voices().iter_mut() {
            if voice.channel() == channel && voice.key() == key {
                voice.end();
            }
        }
    }

    /// Starts `key` on `channel` with the preset the channel has selected. A
    /// `velocity` of 0 is a Note Off.
    pub fn note_on(&mut self, channel: MidiChannel, key: NoteNumber, velocity: U7) {
        if velocity.get() == 0 {
            self.note_off(channel, key);
            return;
        }

        let channel_info = &self.channels[usize::from(channel.get())];

        let preset_id = (channel_info.get_bank_number() << 16) | channel_info.get_patch_number();

        let mut preset = self.default_preset;
        match self.preset_lookup.get(&preset_id) {
            Some(value) => preset = *value,
            None => {
                // Try fallback to the GM sound set.
                // Normally, the given patch number + the bank number 0 will work.
                // For drums (bank number >= 128), it seems to be better to select the standard set (128:0).
                let gm_preset_id = if channel_info.get_bank_number() < 128 {
                    channel_info.get_patch_number()
                } else {
                    128 << 16
                };

                // If no corresponding preset was found. Use the default one...
                if let Some(value) = self.preset_lookup.get(&gm_preset_id) {
                    preset = *value
                }
            }
        }

        let (channel, key_number, velocity_number) = (
            i32::from(channel.get()),
            i32::from(key.get()),
            i32::from(velocity.get()),
        );
        let preset = &self.sound_font.presets()[preset];
        for preset_region in preset.regions() {
            if preset_region.contains(key, velocity) {
                let instrument = &self.sound_font.instruments()[preset_region.instrument_id()];
                for instrument_region in instrument.regions() {
                    if instrument_region.contains(key, velocity) {
                        let region_pair = RegionPair::new(preset_region, instrument_region);

                        if let Some(value) = self.voices.request_new(instrument_region, channel) {
                            value.start(&region_pair, channel, key_number, velocity_number)
                        }
                    }
                }
            }
        }
    }

    /// Stops every voice, by `stop`.
    pub fn note_off_all(&mut self, stop: Stop) {
        match stop {
            Stop::Cut => self.voices.clear(),
            Stop::Release => {
                for voice in self.voices.get_active_voices().iter_mut() {
                    voice.end();
                }
            }
        }
    }

    /// Stops every voice on `channel`, by `stop`.
    pub fn note_off_all_channel(&mut self, channel: MidiChannel, stop: Stop) {
        let channel = i32::from(channel.get());
        for voice in self.voices.get_active_voices().iter_mut() {
            if voice.channel() == channel {
                match stop {
                    Stop::Cut => voice.kill(),
                    Stop::Release => voice.end(),
                }
            }
        }
    }

    /// Resets every channel's controllers.
    pub fn reset_all_controllers(&mut self) {
        for channel in &mut self.channels {
            channel.reset_all_controllers();
        }
    }

    /// Resets `channel`'s controllers.
    pub fn reset_all_controllers_channel(&mut self, channel: MidiChannel) {
        self.channels[usize::from(channel.get())].reset_all_controllers();
    }

    /// Cuts every voice, resets every channel and mutes the effects.
    pub fn reset(&mut self) {
        self.voices.clear();

        for channel in &mut self.channels {
            channel.reset();
        }

        if let Some(effects) = self.effects.as_mut() {
            effects.reverb.mute();
            effects.chorus.mute();
        }

        self.block_read = self.block_size;
    }

    /// Renders stereo audio into the provided buffers.
    ///
    /// # Panics
    ///
    /// If the buffers differ in length.
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        if left.len() != right.len() {
            panic!("The output buffers for the left and right must be the same length.");
        }

        let left_length = left.len();

        let mut wrote = 0;
        while wrote < left_length {
            if self.block_read == self.block_size {
                self.render_block();
                self.block_read = 0;
            }

            let src_rem = self.block_size - self.block_read;
            let dst_rem = left_length - wrote;
            let rem = cmp::min(src_rem, dst_rem);

            for t in 0..rem {
                left[wrote + t] = self.block_left[self.block_read + t];
                right[wrote + t] = self.block_right[self.block_read + t];
            }

            self.block_read += rem;
            wrote += rem;
        }
    }

    fn render_block(&mut self) {
        self.voices
            .process(self.sound_font.wave_data(), &self.channels);

        self.block_left.fill(0_f32);
        self.block_right.fill(0_f32);
        for voice in self.voices.get_active_voices().iter_mut() {
            let previous_gain_left = self.master_volume * voice.previous_mix_gain_left;
            let current_gain_left = self.master_volume * voice.current_mix_gain_left;
            Synthesizer::write_block(
                previous_gain_left,
                current_gain_left,
                voice.block(),
                &mut self.block_left[..],
                self.inverse_block_size,
            );
            let previous_gain_right = self.master_volume * voice.previous_mix_gain_right;
            let current_gain_right = self.master_volume * voice.current_mix_gain_right;
            Synthesizer::write_block(
                previous_gain_right,
                current_gain_right,
                voice.block(),
                &mut self.block_right[..],
                self.inverse_block_size,
            );
        }

        if let Some(effects) = self.effects.as_mut() {
            let chorus = &mut effects.chorus;
            let chorus_input_left = &mut effects.chorus_input_left[..];
            let chorus_input_right = &mut effects.chorus_input_right[..];
            let chorus_output_left = &mut effects.chorus_output_left[..];
            let chorus_output_right = &mut effects.chorus_output_right[..];
            chorus_input_left.fill(0_f32);
            chorus_input_right.fill(0_f32);
            for voice in self.voices.get_active_voices().iter_mut() {
                let previous_gain_left = voice.previous_chorus_send * voice.previous_mix_gain_left;
                let current_gain_left = voice.current_chorus_send * voice.current_mix_gain_left;
                Synthesizer::write_block(
                    previous_gain_left,
                    current_gain_left,
                    voice.block(),
                    chorus_input_left,
                    self.inverse_block_size,
                );
                let previous_gain_right =
                    voice.previous_chorus_send * voice.previous_mix_gain_right;
                let current_gain_right = voice.current_chorus_send * voice.current_mix_gain_right;
                Synthesizer::write_block(
                    previous_gain_right,
                    current_gain_right,
                    voice.block(),
                    chorus_input_right,
                    self.inverse_block_size,
                );
            }
            chorus.process(
                chorus_input_left,
                chorus_input_right,
                chorus_output_left,
                chorus_output_right,
            );
            ArrayMath::multiply_add(
                self.master_volume,
                chorus_output_left,
                &mut self.block_left[..],
            );
            ArrayMath::multiply_add(
                self.master_volume,
                chorus_output_right,
                &mut self.block_right[..],
            );

            let reverb = &mut effects.reverb;
            let reverb_input = &mut effects.reverb_input[..];
            let reverb_output_left = &mut effects.reverb_output_left[..];
            let reverb_output_right = &mut effects.reverb_output_right[..];
            reverb_input.fill(0_f32);
            for voice in self.voices.get_active_voices().iter_mut() {
                let previous_gain = reverb.get_input_gain()
                    * voice.previous_reverb_send
                    * (voice.previous_mix_gain_left + voice.previous_mix_gain_right);
                let current_gain = reverb.get_input_gain()
                    * voice.current_reverb_send
                    * (voice.current_mix_gain_left + voice.current_mix_gain_right);
                Synthesizer::write_block(
                    previous_gain,
                    current_gain,
                    voice.block(),
                    &mut reverb_input[..],
                    self.inverse_block_size,
                );
            }

            reverb.process(reverb_input, reverb_output_left, reverb_output_right);
            ArrayMath::multiply_add(
                self.master_volume,
                reverb_output_left,
                &mut self.block_left[..],
            );
            ArrayMath::multiply_add(
                self.master_volume,
                reverb_output_right,
                &mut self.block_right[..],
            );
        }
    }

    fn write_block(
        previous_gain: f32,
        current_gain: f32,
        source: &[f32],
        destination: &mut [f32],
        inverse_block_size: f32,
    ) {
        if SoundFontMath::max(previous_gain, current_gain) < SoundFontMath::NON_AUDIBLE {
            return;
        }

        if (current_gain - previous_gain).abs() < 1.0E-3_f32 {
            ArrayMath::multiply_add(current_gain, source, destination);
        } else {
            let step = inverse_block_size * (current_gain - previous_gain);
            ArrayMath::multiply_add_slope(previous_gain, step, source, destination);
        }
    }

    /// The SoundFont the synthesizer plays.
    #[must_use]
    pub fn sound_font(&self) -> &SoundFont {
        &self.sound_font
    }

    /// The rate the synthesizer renders at, in whole hertz.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        SampleRate(f64::from(self.sample_rate))
    }

    /// The frames the synthesizer renders at a time.
    #[must_use]
    pub fn block_size(&self) -> Samples {
        Samples(self.block_size)
    }

    /// The most voices sounding at once.
    #[must_use]
    pub fn maximum_polyphony(&self) -> usize {
        self.maximum_polyphony
    }

    /// Whether the reverb and chorus run.
    #[must_use]
    pub fn enable_reverb_and_chorus(&self) -> bool {
        self.effects.is_some()
    }

    /// The gain applied to the mix of every voice.
    #[must_use]
    pub fn master_volume(&self) -> Amplitude {
        Amplitude(self.master_volume)
    }

    /// Sets the gain applied to the mix of every voice.
    pub fn set_master_volume(&mut self, value: Amplitude) {
        self.master_volume = value.get();
    }

    /// A copy of this synthesizer that renders at `sample_rate`: the same
    /// SoundFont (shared, not reloaded), block size, polyphony, effects
    /// switch, master volume and per-channel state (bank, patch,
    /// controllers, pitch bend), with no voice sounding and fresh effect
    /// lines. Allocates.
    ///
    /// The voices and the effects are built against a rate and cannot be
    /// re-rated in place, but a channel's state holds no rate, so a re-rated
    /// copy keeps the preset a host selected. `tutti-soundfont` uses it to
    /// run a fork of a unit at its render's rate.
    ///
    /// # Errors
    ///
    /// Returns [`SynthesizerError::SampleRateOutOfRange`] if `sample_rate` is
    /// outside 16 to 192 kHz.
    pub fn with_sample_rate(&self, sample_rate: SampleRate) -> Result<Self, SynthesizerError> {
        let mut settings = SynthesizerSettings::new(sample_rate);
        settings.block_size = Samples(self.block_size);
        settings.maximum_polyphony = self.maximum_polyphony;
        settings.enable_reverb_and_chorus = self.effects.is_some();
        let mut synthesizer = Synthesizer::new(&self.sound_font, &settings)?;
        synthesizer.channels = self.channels.clone();
        synthesizer.master_volume = self.master_volume;
        Ok(synthesizer)
    }
}

#[derive(Debug, Clone)]
struct Effects {
    reverb: Reverb,
    reverb_input: Vec<f32>,
    reverb_output_left: Vec<f32>,
    reverb_output_right: Vec<f32>,

    chorus: Chorus,
    chorus_input_left: Vec<f32>,
    chorus_input_right: Vec<f32>,
    chorus_output_left: Vec<f32>,
    chorus_output_right: Vec<f32>,
}

impl Effects {
    fn new(settings: &SynthesizerSettings) -> Effects {
        Self {
            reverb: Reverb::new(settings.hz()),
            reverb_input: vec![0_f32; settings.block_len()],
            reverb_output_left: vec![0_f32; settings.block_len()],
            reverb_output_right: vec![0_f32; settings.block_len()],
            chorus: Chorus::new(settings.hz(), 0.002, 0.0019, 0.4),
            chorus_input_left: vec![0_f32; settings.block_len()],
            chorus_input_right: vec![0_f32; settings.block_len()],
            chorus_output_left: vec![0_f32; settings.block_len()],
            chorus_output_right: vec![0_f32; settings.block_len()],
        }
    }
}
