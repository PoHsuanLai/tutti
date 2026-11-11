//! A SoundFont unit driven the way the old synthesizer tests drove it: typed
//! messages in, summed magnitudes out. A message is queued as MIDI 1.0 and
//! applied at the next block, as the node applies it.

#![allow(dead_code)] // each test binary uses its own subset

use std::sync::Arc;

use super::{BLOCK, Hand};
use tutti_fixtures::midi::m1_cc;
use tutti_sf2::SoundFont;
use tutti_soundfont::{Settings, SoundFontUnit};
use tutti_types::SampleRate;
use tutti_ump::{CCNumber, MidiChannel, NoteNumber, U7, U14, Voice1, Voice2};

pub const RATE: SampleRate = SampleRate::SR_44K1;

pub fn timgm6mb() -> Arc<SoundFont> {
    let mut file = std::fs::File::open(tutti_fixtures::asset("soundfonts/TimGM6mb.sf2")).unwrap();
    Arc::new(SoundFont::new(&mut file).unwrap())
}

pub use tutti_fixtures::midi::{m1, m2};

/// A unit over a font, with the messages of the old synthesizer's surface.
pub struct Rig {
    pub hand: Hand,
}

impl Rig {
    /// A unit at the default settings. Its dry pair is true silence once a voice stops: the effects are not in it.
    pub fn dry(font: &Arc<SoundFont>) -> Self {
        let settings = Settings::new(RATE);
        Self::with(font, &settings)
    }

    pub fn with(font: &Arc<SoundFont>, settings: &Settings) -> Self {
        Self {
            hand: Hand::new(SoundFontUnit::new(Arc::clone(font), settings).unwrap()),
        }
    }

    pub fn note_on(&mut self, ch: u8, k: u8, velocity: u8) {
        let e = m1(
            MidiChannel::of(ch),
            Voice1::NoteOn {
                note: NoteNumber::of(k),
                velocity: U7::of(velocity),
            },
        );
        self.hand.queue_midi(&[e]);
    }

    pub fn note_off(&mut self, ch: u8, k: u8) {
        let e = m1(
            MidiChannel::of(ch),
            Voice1::NoteOff {
                note: NoteNumber::of(k),
                velocity: U7::of(0),
            },
        );
        self.hand.queue_midi(&[e]);
    }

    pub fn cc(&mut self, ch: u8, controller: CCNumber, value: u8) {
        let e = m1_cc(MidiChannel::of(ch), controller, U7::of(value));
        self.hand.queue_midi(&[e]);
    }

    /// A MIDI 2.0 message, as the input edge's translator sends an RPN,
    /// an NRPN or a Program Change with its bank.
    pub fn voice2(&mut self, ch: u8, voice: Voice2) {
        self.hand.queue_midi(&[m2(MidiChannel::of(ch), voice)]);
    }

    pub fn program(&mut self, ch: u8, program: u8) {
        let e = m1(MidiChannel::of(ch), Voice1::ProgramChange(U7::of(program)));
        self.hand.queue_midi(&[e]);
    }

    pub fn bend(&mut self, ch: u8, bend: u16) {
        let e = m1(MidiChannel::of(ch), Voice1::PitchBend(U14::of(bend)));
        self.hand.queue_midi(&[e]);
    }

    /// All Sound Off on every channel.
    pub fn cut_all(&mut self) {
        for ch in 0..16 {
            self.cc(ch, CCNumber::ALL_SOUND_OFF, 0);
        }
    }

    /// Renders `frames` frames, left then right.
    pub fn render(&mut self, frames: usize) -> (Vec<f32>, Vec<f32>) {
        let (mut left, mut right) = (Vec::with_capacity(frames), Vec::with_capacity(frames));
        while left.len() < frames {
            let n = (frames - left.len()).min(BLOCK);
            let (l, r) = self.hand.block(n);
            left.extend_from_slice(l);
            right.extend_from_slice(r);
        }
        (left, right)
    }

    /// Renders `frames` frames of all five outputs: dry left and right, chorus
    /// send left and right, reverb send. Each block's outputs are what the
    /// executor would hand on (zeros for a block that returned `Silent` or
    /// `Idle`). The status of the last block is `self.hand.status()`.
    pub fn render_all(&mut self, frames: usize) -> [Vec<f32>; 5] {
        let mut planes: [Vec<f32>; 5] = std::array::from_fn(|_| Vec::with_capacity(frames));
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(BLOCK);
            self.hand.run(n);
            for (c, plane) in planes.iter_mut().enumerate() {
                plane.extend_from_slice(self.hand.plane(c));
            }
            done += n;
        }
        planes
    }

    /// The summed magnitude of the next `frames` frames.
    pub fn energy(&mut self, frames: usize) -> f32 {
        let (l, r) = self.render(frames);
        l.iter().chain(&r).map(|s| s.abs()).sum()
    }

    /// The summed magnitude of the next `frames` frames, per channel.
    pub fn energy_lr(&mut self, frames: usize) -> (f32, f32) {
        let (l, r) = self.render(frames);
        let sum = |b: &[f32]| b.iter().map(|s| s.abs()).sum();
        (sum(&l), sum(&r))
    }
}
