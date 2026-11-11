// Derived from rustysynth 1.3.6 (MIT), Copyright (C) 2022 Nobuaki Tanaka, after
// MeltySynth (Copyright (C) 2021 Nobuaki Tanaka). The MIT notice is
// LICENSE-RUSTYSYNTH.txt, at the root of this crate. In this file: render_block, write_block and the preset id come from rustysynth's synthesizer.rs.

//! The SoundFont player: the channels' controller state, the voices that
//! sound, the core that decides when they start and end, and the mix.
//!
//! A message goes to [`Voicing::step`], which says which voices to start,
//! release or cut; [`Voices`] does it. What a note becomes is the preset's
//! ([`PresetPatch`]); how a voice sounds is the voice's own.
//!
//! The player renders five planes, not a mix: the dry pair, the chorus send
//! pair and the reverb send. It holds no effect (see [`Plane`]).

use std::collections::HashMap;
use std::sync::Arc;

use tutti_midi_protocol::narrow_value;
use tutti_sf2::SoundFont;
use tutti_types::{SampleRate, Semitones};
use tutti_ump::{CCNumber, MidiChannel, MidiGroup, U7, Voice2};
use tutti_voicing::{Mode, Order, Retrigger, Rules, Steal, Voicing};

use crate::array_math::ArrayMath;
use crate::channel::Channel;
use crate::error::{Error, Result};
use crate::patch::PresetPatch;
use crate::render::Render;
use crate::settings::Settings;
use crate::soundfont_math::SoundFontMath;
use crate::voices::Voices;

// The controllers `ump` has no name for: the least significant half of a 14-bit
// controller is its MSB's number plus 32, and the two sends are General MIDI's
// effects depths 1 and 3.
// The fine (LSB) controller of each coarse one the player reads; a match arm
// needs a constant, and `lsb` is `None` only above controller 31.
const BANK_SELECT_LSB: CCNumber = CCNumber::BANK_SELECT.lsb().unwrap();

/// The gain applied to the mix of every voice, on every plane: the sends carry
/// it too, so an effect node after the unit is unity-gain.
const MASTER_VOLUME: f32 = 0.5;

/// One of the unit's five outputs, in port order.
///
/// The player reverberates and chorus-processes nothing: a voice's mix gains go
/// to the dry pair, and those gains scaled by the voice's two effect sends go
/// to the chorus pair (per channel, as the dry pair is) and to the reverb
/// plane (mono, the sum of the two channels' gains). The effects are graph
/// nodes after the unit.
///
/// A send is a linear level, and does not touch the dry pair (SF2 2.04 section
/// 8.1.2, generators 15 and 16: `chorusEffectsSend` and `reverbEffectsSend`, in
/// 0.1% units, 250 is a quarter of full level). A voice's send is the preset's
/// and the instrument's generator summed over both zones (section 9.4), plus
/// the output of the default modulators of CC 93 (chorus) and CC 91 (reverb;
/// sections 8.4.9 and 8.4.8: the controller over 128, times 200 tenths of a
/// percent), and then limited to the generator's range, 0 to 1000 tenths
/// (section 8.1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plane {
    DryLeft = 0,
    DryRight = 1,
    ChorusLeft = 2,
    ChorusRight = 3,
    Reverb = 4,
}

impl Plane {
    /// The number of planes: the unit's output count.
    pub(crate) const COUNT: usize = 5;

    /// Every plane, in port order.
    pub(crate) const ALL: [Plane; Plane::COUNT] = [
        Plane::DryLeft,
        Plane::DryRight,
        Plane::ChorusLeft,
        Plane::ChorusRight,
        Plane::Reverb,
    ];
}

/// The planes a render has written something into (as a bit set over
/// [`Plane`]); a plane outside it is exact silence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Heard(u8);

impl Heard {
    fn insert(&mut self, plane: Plane) {
        self.0 |= 1 << plane as u8;
    }

    fn union(&mut self, other: Heard) {
        self.0 |= other.0;
    }

    /// Whether anything was written into any plane.
    pub(crate) fn has_any(self) -> bool {
        self.0 != 0
    }

    /// Whether anything was written into `plane`.
    pub(crate) fn has(self, plane: Plane) -> bool {
        self.0 & (1 << plane as u8) != 0
    }
}

/// Five equal-length planes: the unit's outputs for one stretch of frames.
pub(crate) struct Planes<'a>(pub(crate) [&'a mut [f32]; Plane::COUNT]);

/// The rates a player renders at, in whole hertz.
const RATES: std::ops::RangeInclusive<i32> = 16_000..=192_000;

/// The note rules of a SoundFont, as a value of [`Rules`]. Where the SoundFont
/// 2.04 specification says, the rule is the spec's; where it is silent, it is
/// tutti's, and says so.
///
/// - `Poly`: the specification describes polyphonic playback and nothing else.
/// - `Replace` (tutti): the spec does not say what a key struck again does.
///   The earlier strike is released, whatever pedal holds it, so a repeated
///   key never stacks voices.
/// - `By(Oldest)` (tutti): the spec does not say who gives way when the
///   voices are all taken. A released voice goes before any held one, the
///   quietest of the released ones first (`Voicing::victim`); among held
///   voices the one that started longest ago.
/// - `exclusive_groups` (spec, section 8.1.2, generator 57): a note with an
///   exclusive class ends the other sounding notes of that class in its
///   preset, rapidly; the scope is the preset, which the patch names.
/// - `sostenuto` (tutti, with MIDI 1.0): CC 66. The spec lists CC 66 as "soft"
///   and CC 67 as "sostenuto" in section 9.2, the reverse of MIDI 1.0, whose
///   numbers a keyboard sends. The owner decided on 2026-11-08 that tutti
///   follows MIDI 1.0: 66 is the sostenuto and 67 the soft pedal (which is not
///   followed).
fn rules(polyphony: usize) -> Rules {
    Rules {
        mode: Mode::Poly,
        retrigger: Retrigger::Replace,
        polyphony,
        steal: Steal::By(Order::Oldest),
        exclusive_groups: true,
        sostenuto: true,
        // What a full bend spans until RPN 0 says otherwise: General MIDI's
        // default of 2 semitones.
        bend_range: Semitones(2.0),
        per_note_bend_range: Semitones(48.0),
    }
}

/// The player. Its voices are built against a rate and cannot be re-rated in
/// place: [`with_sample_rate`](Self::with_sample_rate) builds another.
#[derive(Debug, Clone)]
pub(crate) struct Player {
    font: Arc<SoundFont>,
    render: Render,
    settings: Settings,

    presets: HashMap<i32, usize>,
    default_preset: usize,

    channels: Vec<Channel>,
    voicing: Voicing,
    voices: Voices,

    /// The chunk being read out, a plane each (see [`Plane`]).
    block: [Vec<f32>; Plane::COUNT],
    /// What the chunk holds written, and what has been read out since
    /// [`take_heard`](Self::take_heard).
    block_heard: Heard,
    heard: Heard,
    block_read: usize,
}

impl Player {
    /// The channel General MIDI plays percussion on.
    pub(crate) const PERCUSSION_CHANNEL: MidiChannel = MidiChannel::of(9);

    /// Builds a player over `font`. Allocates the voices.
    ///
    /// # Errors
    ///
    /// [`Error::NoPlayablePreset`] when no preset of the font has a MIDI bank
    /// and program (SF2 2.04 section 7.2),
    /// [`Error::SampleRateOutOfRange`] outside 16 to 192 kHz and
    /// [`Error::PolyphonyOutOfRange`] for more than [`Settings::MAX_POLYPHONY`]
    /// voices (none is not representable: the polyphony is a `NonZeroUsize`).
    pub(crate) fn new(font: &Arc<SoundFont>, settings: &Settings) -> Result<Self> {
        let render = Render::new(settings.sample_rate());
        if !RATES.contains(&render.hz()) {
            return Err(Error::SampleRateOutOfRange(settings.sample_rate()));
        }
        if settings.polyphony().get() > Settings::MAX_POLYPHONY {
            return Err(Error::PolyphonyOutOfRange(settings.polyphony().get()));
        }
        let polyphony = settings.polyphony().get();

        let mut presets: HashMap<i32, usize> = HashMap::new();
        let mut min_preset_id = i32::MAX;
        let mut default_preset: Option<usize> = None;
        for (i, preset) in font.presets().iter().enumerate() {
            // A preset whose bank or program is not MIDI's cannot be played
            // (SF2 2.04 section 7.2): it is kept in the font and not looked up.
            let (Some(bank), Some(patch)) = (preset.bank(), preset.patch()) else {
                continue;
            };
            // The preset ID is Int32, where the upper 16 bits represent the bank number
            // and the lower 16 bits represent the patch number.
            // This ID is used to search for presets by the combination of bank number
            // and patch number.
            let preset_id = (i32::from(bank.get()) << 16) | i32::from(patch.get());
            // Two presets with the same bank and program: the first in the file
            // is the active one (section 7.2).
            presets.entry(preset_id).or_insert(i);

            // The preset with the minimum ID number will be default.
            // If the SoundFont is GM compatible, the piano will be chosen.
            if preset_id < min_preset_id {
                default_preset = Some(i);
                min_preset_id = preset_id;
            }
        }

        // A font with nothing playable would sound a preset the file says cannot
        // be played, or nothing at all: it is refused.
        let Some(default_preset) = default_preset else {
            return Err(Error::NoPlayablePreset);
        };

        let channels = (0..MidiChannel::COUNT)
            .map(|i| Channel::new(i == Self::PERCUSSION_CHANNEL.get()))
            .collect();

        Ok(Self {
            font: Arc::clone(font),
            render,
            settings: *settings,
            presets,
            default_preset,
            channels,
            voicing: Voicing::new(rules(polyphony)),
            voices: Voices::new(&render, polyphony),
            block: std::array::from_fn(|_| vec![0_f32; render.block_len()]),
            block_heard: Heard::default(),
            heard: Heard::default(),
            block_read: render.block_len(),
        })
    }

    /// The Group this player's sixteen channels are.
    pub(crate) fn group(&self) -> MidiGroup {
        self.settings.group()
    }

    /// The preset `channel` plays: its bank and program, else General MIDI's
    /// program in bank 0 (the standard drum set, 128:0, for a drum bank), else
    /// the font's first.
    fn preset_of(&self, channel: MidiChannel) -> usize {
        let info = &self.channels[usize::from(channel.get())];
        let id = (info.bank_number() << 16) | info.patch_number();
        if let Some(&preset) = self.presets.get(&id) {
            return preset;
        }
        let gm_id = if info.bank_number() < 128 {
            info.patch_number()
        } else {
            128 << 16
        };
        self.presets
            .get(&gm_id)
            .copied()
            .unwrap_or(self.default_preset)
    }

    /// Reads one channel voice message.
    ///
    /// Followed: Note On and Off, program change (with its bank), pitch
    /// bend, channel pressure, the registered controllers and the channel
    /// controllers the note logic reads ([`Voicing`]: modulation, volume, pan,
    /// expression, portamento, legato, the pedals and Reset All Controllers),
    /// and the ones named at [`control`](Self::control) (bank select and the
    /// sends). Everything else is dropped.
    pub(crate) fn apply(&mut self, channel: MidiChannel, message: Voice2) {
        match message {
            // A Note On at velocity 0 is a Note On (M2-104 §7.4.2), struck at
            // the least MIDI 1.0 velocity (`Patch`'s `.max(1)`); a MIDI 1.0 one
            // arrived as a Note Off (`channel_voice` widens it, App. D.3.1).
            Voice2::NoteOn { .. } | Voice2::NoteOff { .. } => self.dispatch(channel, message),
            Voice2::ControlChange { controller, value } => {
                self.control(channel, controller, narrow_value(value));
                self.dispatch(channel, message);
            }
            // The wheel is the core's (it retunes the channel's voices), and so
            // is the channel's pressure, which it keeps (a SoundFont has no
            // generator to drive with it).
            Voice2::PitchBend(_) | Voice2::ChannelPressure(_) => self.dispatch(channel, message),
            // A bank, when it rides along (the translator puts a MIDI 1.0
            // Bank Select there), is the held Bank Select, and the Program
            // Change applies it (M1 p.13).
            Voice2::ProgramChange { program, bank } => {
                let info = &mut self.channels[usize::from(channel.get())];
                if let Some(bank) = bank {
                    info.select_bank_coarse(bank.msb);
                    info.select_bank_fine(bank.lsb);
                }
                self.program_change(channel, program);
            }
            // The registered controllers are the core's: the pitch bend
            // sensitivity (RPN 0), the channel tuning (RPN 1 and 2, which it
            // folds into the channel's pitch) and the Modulation Depth Range
            // (RPN 5).
            Voice2::Registered { .. } => self.dispatch(channel, message),
            // Per-note controllers and Per-Note Management are the core's: it
            // keeps what each Note Number was told (so a Pitch sent before its
            // Note On reaches the note) and which voices were detached; the
            // voice carries the ones it has a generator for (`Voice::control`).
            Voice2::RegisteredPerNote { .. }
            | Voice2::AssignablePerNote { .. }
            | Voice2::PerNotePitchBend { .. }
            | Voice2::PolyPressure { .. }
            | Voice2::PerNoteManagement { .. } => self.dispatch(channel, message),
            // An Assignable Controller (NRPN) names nothing here.
            _ => {}
        }
    }

    /// Selects the program `channel` plays, in the bank held by Bank Select
    /// (M1 p.13).
    pub(crate) fn program_change(&mut self, channel: MidiChannel, program: U7) {
        self.channels[usize::from(channel.get())].program_change(program);
    }

    /// Hands a note or pedal message to the core, and carries out what it
    /// says.
    fn dispatch(&mut self, channel: MidiChannel, message: Voice2) {
        self.voices.refresh();
        let patch = PresetPatch {
            font: &self.font,
            preset: self.preset_of(channel),
        };
        let commands = self
            .voicing
            .step(channel, &message, &patch, self.voices.views());
        self.voices.execute(commands, &self.font);
    }

    /// What a Control Change sets in the channel.
    ///
    /// Followed: bank select (held until the next program change) and the
    /// reverb and chorus send. Modulation, volume, pan and expression, and Reset
    /// All Controllers (RP-015), are the core's `ChannelState`. RPN and NRPN arrive
    /// assembled, as Registered and Assignable Controllers (the input edge's
    /// translator, design 017 §4), not as Data Entry. The pedals (64, 66), all sound off (120), all
    /// notes off (123) and the mode messages are the core's
    /// (`Voicing::step`), not the channel's. Any other controller is ignored.
    fn control(&mut self, channel: MidiChannel, controller: CCNumber, value: U7) {
        let data = i32::from(value.get());
        let info = &mut self.channels[usize::from(channel.get())];
        match controller {
            CCNumber::BANK_SELECT => info.select_bank_coarse(value),
            BANK_SELECT_LSB => info.select_bank_fine(value),
            CCNumber::REVERB_SEND => info.set_reverb_send(data),
            CCNumber::CHORUS_SEND => info.set_chorus_send(data),
            _ => (),
        }
    }

    /// Lets every voice go into its release, whatever holds it, and forgets
    /// the pedals and keys: a fork's start.
    pub(crate) fn release_all(&mut self) {
        self.voices.release_all();
        self.voicing.forget_notes();
    }

    /// Cuts every voice, forgets the pedals and keys, and returns every
    /// channel's performance controllers to their defaults (modulation,
    /// expression, bend and the RPN selection: Reset All Controllers). What the
    /// host selected stays: the bank, the program, the volume, the pan and the
    /// sends.
    pub(crate) fn reset(&mut self) {
        self.voices.clear();
        self.voicing.forget_notes();
        for channel in (0..MidiChannel::COUNT).filter_map(MidiChannel::new) {
            self.apply(
                channel,
                Voice2::ControlChange {
                    controller: CCNumber::RESET_ALL,
                    value: 0,
                },
            );
        }
        self.block_read = self.render.block_len();
    }

    /// Renders the five planes, which are the same length, advancing the
    /// player by that many frames.
    pub(crate) fn render(&mut self, planes: Planes<'_>) {
        let block = self.render.block_len();
        let mut planes = planes.0;
        let length = planes[0].len();
        debug_assert!(planes.iter().all(|p| p.len() == length));

        let mut wrote = 0;
        while wrote < length {
            if self.block_read == block {
                self.render_block();
                self.block_read = 0;
            }

            let src_rem = block - self.block_read;
            let dst_rem = length - wrote;
            let rem = src_rem.min(dst_rem);

            for (plane, source) in planes.iter_mut().zip(&self.block) {
                plane[wrote..wrote + rem]
                    .copy_from_slice(&source[self.block_read..self.block_read + rem]);
            }
            self.heard.union(self.block_heard);

            self.block_read += rem;
            wrote += rem;
        }
    }

    /// The planes written into since the last call: the others were exact
    /// silence over every frame rendered meanwhile.
    pub(crate) fn take_heard(&mut self) -> Heard {
        std::mem::take(&mut self.heard)
    }

    /// Whether the player has nothing left to sound: no voice, and the chunk
    /// being read out read out to its end. Silent until an event arrives.
    pub(crate) fn is_idle(&self) -> bool {
        self.voices.is_empty() && self.block_read == self.render.block_len()
    }

    fn render_block(&mut self) {
        let inverse_block_size = 1_f32 / self.render.block_len() as f32;
        let bends = std::array::from_fn(|i| {
            MidiChannel::new(i as u8).map_or(Semitones(0.0), |c| self.voicing.channel_pitch(c))
        });
        self.voices.process(
            self.font.wave_data(),
            &self.channels,
            self.voicing.channels(),
            &bends,
        );

        for plane in &mut self.block {
            plane.fill(0_f32);
        }
        self.block_heard = Heard::default();
        for voice in self.voices.iter() {
            let (prev_l, cur_l) = (
                MASTER_VOLUME * voice.previous_mix_gain_left,
                MASTER_VOLUME * voice.current_mix_gain_left,
            );
            let (prev_r, cur_r) = (
                MASTER_VOLUME * voice.previous_mix_gain_right,
                MASTER_VOLUME * voice.current_mix_gain_right,
            );
            let mut write = |plane: Plane, previous: f32, current: f32| {
                if write_block(
                    previous,
                    current,
                    voice.block(),
                    &mut self.block[plane as usize],
                    inverse_block_size,
                ) {
                    self.block_heard.insert(plane);
                }
            };
            write(Plane::DryLeft, prev_l, cur_l);
            write(Plane::DryRight, prev_r, cur_r);
            write(
                Plane::ChorusLeft,
                voice.previous_chorus_send * prev_l,
                voice.current_chorus_send * cur_l,
            );
            write(
                Plane::ChorusRight,
                voice.previous_chorus_send * prev_r,
                voice.current_chorus_send * cur_r,
            );
            write(
                Plane::Reverb,
                voice.previous_reverb_send * (prev_l + prev_r),
                voice.current_reverb_send * (cur_l + cur_r),
            );
        }
    }

    /// The rate the player renders at, in whole hertz.
    pub(crate) fn sample_rate(&self) -> SampleRate {
        SampleRate(f64::from(self.render.hz()))
    }

    /// A copy of this player that renders at `sample_rate`: the same
    /// SoundFont (shared, not reloaded), polyphony and
    /// per-channel state (bank, patch, controllers, pitch bend), with no voice
    /// sounding. Allocates.
    ///
    /// # Errors
    ///
    /// [`Error::SampleRateOutOfRange`] if `sample_rate` is outside 16 to
    /// 192 kHz.
    pub(crate) fn with_sample_rate(&self, sample_rate: SampleRate) -> Result<Self> {
        let mut player = Self::new(&self.font, &self.settings.with_sample_rate(sample_rate))?;
        player.channels = self.channels.clone();
        // The core's channel state (wheel, sensitivity); its notes are not
        // the new player's, which has no voice.
        player.voicing = self.voicing.clone();
        player.voicing.forget_notes();
        Ok(player)
    }

    /// The longest the player can keep sounding once its last key is up: the
    /// longest release of any voice a preset can start. A release time is the
    /// time to fall 100 dB (SF2 2.04 section 8.1.2, generator 38) and a voice is
    /// dropped at -60 dB, at 0.6 of it, so the figure is a sound bound, and a
    /// loose one. Each summed release is within the generator's range (section
    /// 10.5), so it is at most 8000 timecents, 101.6 s. `None` if a release
    /// cannot be bounded (it is not finite). The effects after the unit are the
    /// graph's: each node declares its own tail.
    pub(crate) fn ring_out(&self) -> Option<f64> {
        let mut longest = 0_f64;
        for preset in self.font.presets() {
            for preset_zone in preset.regions() {
                let instrument = &self.font.instruments()[preset_zone.instrument_id()];
                for instrument_zone in instrument.regions() {
                    let pair = crate::region_pair::RegionPair::new(preset_zone, instrument_zone);
                    longest = longest.max(f64::from(pair.release_time()));
                }
            }
        }
        longest.is_finite().then_some(longest)
    }
}

/// Adds `source` into `destination` at a gain that ramps from `previous_gain`
/// to `current_gain` across the chunk. Returns whether it wrote anything: not
/// when the gain stays under the audible floor.
fn write_block(
    previous_gain: f32,
    current_gain: f32,
    source: &[f32],
    destination: &mut [f32],
    inverse_block_size: f32,
) -> bool {
    if SoundFontMath::max(previous_gain, current_gain) < SoundFontMath::NON_AUDIBLE {
        return false;
    }

    if (current_gain - previous_gain).abs() < 1.0E-3_f32 {
        ArrayMath::multiply_add(current_gain, source, destination);
    } else {
        let step = inverse_block_size * (current_gain - previous_gain);
        ArrayMath::multiply_add_slope(previous_gain, step, source, destination);
    }
    true
}
