//! The voices that sound, and the commands that start and end them.

use tutti_midi_protocol::narrow_velocity;
use tutti_sf2::SoundFont;
use tutti_types::Amplitude;
use tutti_voicing::{Command, VoiceId, VoiceView};

use crate::channel::Channel;
use crate::patch::PresetPatch;
use crate::region_pair::RegionPair;
use crate::render::Render;
use crate::voice::Voice;

/// A sounding voice and the id the core knows it by.
#[derive(Debug, Clone)]
struct Slot {
    id: VoiceId,
    voice: Voice,
}

/// The pool of voices: `active` in id order (the order they started, which is
/// the order the core wants them listed in), `free` the rest. Both are sized
/// at construction to the polyphony, so nothing here allocates afterwards.
#[derive(Debug)]
pub(crate) struct Voices {
    polyphony: usize,
    active: Vec<Slot>,
    free: Vec<Voice>,
    views: Vec<VoiceView>,
}

impl Clone for Voices {
    /// A copy with its lists as roomy as the original's: a derived clone would
    /// shrink them to their length, and a list that grows is an allocation on
    /// the audio thread.
    fn clone(&self) -> Self {
        let mut copy = Self {
            polyphony: self.polyphony,
            active: Vec::with_capacity(self.polyphony),
            free: Vec::with_capacity(self.polyphony),
            views: Vec::with_capacity(self.polyphony),
        };
        copy.active.extend(self.active.iter().cloned());
        copy.free.extend(self.free.iter().cloned());
        copy.views.extend_from_slice(&self.views);
        copy
    }
}

impl Voices {
    pub(crate) fn new(render: &Render, polyphony: usize) -> Self {
        Self {
            polyphony,
            active: Vec::with_capacity(polyphony),
            free: {
                let mut free = Vec::with_capacity(polyphony);
                free.extend((0..polyphony).map(|_| Voice::new(render)));
                free
            },
            views: Vec::with_capacity(polyphony),
        }
    }

    /// The voices as the core reads them, with their levels now: the list
    /// [`views`](Self::views) hands back.
    pub(crate) fn refresh(&mut self) {
        self.views.clear();
        self.views.extend(self.active.iter().map(|s| VoiceView {
            id: s.id,
            level: Amplitude(s.voice.level()),
        }));
    }

    /// The list [`refresh`](Self::refresh) made, sorted by id.
    pub(crate) fn views(&self) -> &[VoiceView] {
        &self.views
    }

    /// Carries out what the core said, in order. A command that names a voice
    /// that has ended is ignored, as the core's contract says.
    pub(crate) fn execute(&mut self, commands: &[Command], font: &SoundFont) {
        for command in commands {
            match *command {
                Command::Start {
                    id,
                    channel,
                    key,
                    velocity,
                    spec,
                    steal,
                } => {
                    let voice = steal.and_then(|v| self.take(v)).or_else(|| self.free.pop());
                    let Some(mut voice) = voice else { continue };
                    let (p, i) = PresetPatch::zones(spec.tag);
                    let preset_zone = &font.presets()[spec.scope as usize].regions()[p];
                    let instrument_zone =
                        &font.instruments()[preset_zone.instrument_id()].regions()[i];
                    voice.start(
                        &RegionPair::new(preset_zone, instrument_zone),
                        i32::from(channel.get()),
                        i32::from(key.get()),
                        i32::from(narrow_velocity(velocity).get().max(1)),
                    );
                    self.active.push(Slot { id, voice });
                }
                Command::Release { id, .. } => {
                    if let Some(slot) = self.slot(id) {
                        slot.voice.release();
                    }
                }
                Command::Damp(id) => {
                    if let Some(slot) = self.slot(id) {
                        slot.voice.damp();
                    }
                }
                Command::Cut(id) => {
                    if let Some(voice) = self.take(id) {
                        self.free.push(voice);
                    }
                }
                // `Rules::mode` is `Poly`, which never moves a voice to a new
                // key; and the pitch the voices sound is read from the channel
                // each chunk, so a `Retune` has nothing to tell them.
                Command::Retarget { .. } | Command::Retune { .. } => {}
            }
        }
    }

    fn slot(&mut self, id: VoiceId) -> Option<&mut Slot> {
        let i = self.active.binary_search_by_key(&id, |s| s.id).ok()?;
        Some(&mut self.active[i])
    }

    /// Takes the voice out of play, keeping the others in order.
    fn take(&mut self, id: VoiceId) -> Option<Voice> {
        let i = self.active.binary_search_by_key(&id, |s| s.id).ok()?;
        Some(self.active.remove(i).voice)
    }

    /// Renders one chunk of every voice, and drops the ones that have ended.
    pub(crate) fn process(&mut self, data: &[i16], channels: &[Channel]) {
        let mut i = 0;
        while i < self.active.len() {
            if self.active[i].voice.process(data, channels) {
                i += 1;
            } else {
                let slot = self.active.remove(i);
                self.free.push(slot.voice);
            }
        }
    }

    /// The voices, for the mix.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Voice> {
        self.active.iter().map(|s| &s.voice)
    }

    /// Lets every voice go into its release, whatever holds it.
    pub(crate) fn release_all(&mut self) {
        for slot in &mut self.active {
            slot.voice.release();
        }
    }

    /// Silences every voice at once.
    pub(crate) fn clear(&mut self) {
        while let Some(slot) = self.active.pop() {
            self.free.push(slot.voice);
        }
    }
}
