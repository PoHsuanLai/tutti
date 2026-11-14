//! [`NoteAdsr`]: an ADSR driven by note-ons and note-offs.

use tutti_types::{Adsr, Frame, SampleRate, Samples};
use tutti_ump::{Key, KeyEvent, Ump};

use super::{ControlSource, Leg, PlayGrid};

/// Held notes: one bit per (group, channel, note), 16 × 16 × 128.
const NOTE_WORDS: usize = 16 * 16 * 128 / 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// The held-note table's index of `key`: its group, channel and note, the
/// protocol dropped, so the same note in either protocol is one note.
fn slot(key: Key) -> usize {
    key.address().index() * 128 + usize::from(key.note().get())
}

/// A **note envelope**: an ADSR driven by note-ons and note-offs, sending a
/// leg at each note and at the attack's end, with no grid.
///
/// - **Exact.** A linear attack to 1 over `attack`, a linear decay to
///   `sustain` over `decay`, held, and a linear release to 0 over `release`,
///   each length rounded to whole frames (`Seconds::to_samples`). So the peak
///   lands on the frame `attack` after the note-on, and the sustain level
///   `decay` after that; a leg holds its end, so the sustain and the
///   release's end need no leg of their own. A zero-length stage is skipped on
///   its frame. A NaN sustain is 0.
/// - **Mono, last-note priority, retrigger.** Every note-on restarts the
///   attack, from the level there (no jump to 0, so no click). A note-off
///   releases only when it lifts the last held note: lifting one of several
///   leaves the envelope where it is, sounding for the note still held. A
///   note-off for a note not held does nothing. Notes are told apart by group,
///   channel and note number (a MIDI 1.0 note-on at velocity 0 is a note-off,
///   [`Ump::key_event`]).
/// - **Release from the level there**: a note-off mid-attack releases from the
///   attack's current level, over the full `release`.
/// - **A play start** forgets every held note and leaves it idle at 0.
#[derive(Clone, Debug)]
pub struct NoteAdsr {
    adsr: Adsr,
    /// Attack, decay and release in frames at the prepared rate; zero-length
    /// until `prepare`.
    attack: Samples,
    decay: Samples,
    release: Samples,
    sustain: f32,
    held: [u64; NOTE_WORDS],
    holding: u32,
    stage: Stage,
    /// The play frame the current stage ends on, if it does.
    boundary: Option<Frame>,
}

impl NoteAdsr {
    /// An envelope of `adsr`, idle. The rate comes at
    /// [`prepare`](ControlSource::prepare).
    #[must_use]
    pub fn new(adsr: Adsr) -> Self {
        let s = adsr.sustain.get();
        Self {
            adsr,
            attack: Samples(0),
            decay: Samples(0),
            release: Samples(0),
            // A NaN sustain is 0: the envelope never sends a NaN.
            sustain: if s >= 0.0 { s.min(1.0) } else { 0.0 },
            held: [0; NOTE_WORDS],
            holding: 0,
            stage: Stage::Idle,
            boundary: None,
        }
    }

    /// Enters `stage` on play frame `at` from level `from`, through any
    /// zero-length stage: its leg.
    fn enter(&mut self, mut stage: Stage, at: Frame, mut from: f32) -> Leg {
        loop {
            let (to, length, next) = match stage {
                Stage::Attack => (1.0, self.attack, Stage::Decay),
                Stage::Decay => (self.sustain, self.decay, Stage::Sustain),
                Stage::Release => (0.0, self.release, Stage::Idle),
                Stage::Sustain | Stage::Idle => unreachable!("entered only as a boundary"),
            };
            if length.get() == 0 {
                // Skipped on its frame: the next stage starts at its end.
                from = to;
                stage = next;
                if matches!(stage, Stage::Sustain | Stage::Idle) {
                    self.stage = stage;
                    self.boundary = None;
                    return Leg::hold(to);
                }
                continue;
            }
            self.stage = stage;
            self.boundary = Some(at + length);
            return Leg::linear(from, to, length);
        }
    }
}

impl ControlSource for NoteAdsr {
    const GRID: Option<PlayGrid> = None;

    fn prepare(&mut self, rate: SampleRate) {
        self.attack = self.adsr.attack.to_samples(rate);
        self.decay = self.adsr.decay.to_samples(rate);
        self.release = self.adsr.release.to_samples(rate);
    }

    /// Every note forgotten, idle at 0: a 0 to send unless it was idle.
    fn start(&mut self) -> Option<Leg> {
        self.held = [0; NOTE_WORDS];
        self.holding = 0;
        self.boundary = None;
        let was = std::mem::replace(&mut self.stage, Stage::Idle);
        (was != Stage::Idle).then_some(Leg::hold(0.0))
    }

    fn next_corner(&self) -> Option<Frame> {
        self.boundary
    }

    /// A stage ends: the attack enters the decay; the decay and the release
    /// have arrived and hold their ends.
    fn corner(&mut self, t: Frame) -> Option<Leg> {
        self.boundary = None;
        match self.stage {
            Stage::Attack => Some(self.enter(Stage::Decay, t, 1.0)),
            Stage::Decay => {
                self.stage = Stage::Sustain;
                None
            }
            Stage::Release => {
                self.stage = Stage::Idle;
                None
            }
            Stage::Idle | Stage::Sustain => None,
        }
    }

    fn heard(&mut self, t: Frame, level: f32, ump: Ump) -> Option<Leg> {
        let (on, key) = match ump.key_event()? {
            KeyEvent::On(key) => (true, key),
            KeyEvent::Off(key) => (false, key),
            KeyEvent::Sustain { .. } => return None,
        };
        let key = slot(key);
        let (word, bit) = (key / 64, 1u64 << (key % 64));
        let was = self.held[word] & bit != 0;
        if on {
            if !was {
                self.held[word] |= bit;
                self.holding += 1;
            }
            Some(self.enter(Stage::Attack, t, level))
        } else if was {
            self.held[word] &= !bit;
            self.holding -= 1;
            (self.holding == 0 && self.stage != Stage::Release && self.stage != Stage::Idle)
                .then(|| self.enter(Stage::Release, t, level))
        } else {
            None
        }
    }
}
