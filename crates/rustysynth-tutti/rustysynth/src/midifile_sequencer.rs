#![allow(dead_code)]

use std::cmp;
use std::sync::Arc;

use crate::midifile::Message;
use crate::midifile::MidiFile;
use crate::synthesizer::Synthesizer;

/// Plays a MIDI file through a synthesizer, handling tempo and looping.
#[derive(Debug)]
#[non_exhaustive]
pub struct MidiFileSequencer {
    synthesizer: Synthesizer,

    speed: f64,

    midi_file: Option<Arc<MidiFile>>,
    play_loop: bool,

    block_wrote: usize,

    current_time: f64,
    msg_index: usize,
    loop_index: usize,
}

impl MidiFileSequencer {
    pub fn new(synthesizer: Synthesizer) -> Self {
        Self {
            synthesizer,
            speed: 1.0,
            midi_file: None,
            play_loop: false,
            block_wrote: 0,
            current_time: 0.0,
            msg_index: 0,
            loop_index: 0,
        }
    }

    pub fn play(&mut self, midi_file: &Arc<MidiFile>, play_loop: bool) {
        self.midi_file = Some(Arc::clone(midi_file));
        self.play_loop = play_loop;

        self.block_wrote = self.synthesizer.block_size;

        self.current_time = 0.0;
        self.msg_index = 0;
        self.loop_index = 0;

        self.synthesizer.reset()
    }

    pub fn stop(&mut self) {
        self.midi_file = None;
        self.synthesizer.reset();
    }

    /// Renders interleaved stereo audio. Both buffers must be the same length.
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        if left.len() != right.len() {
            panic!("The output buffers for the left and right must be the same length.");
        }

        let left_length = left.len();
        let mut wrote: usize = 0;
        while wrote < left_length {
            if self.block_wrote == self.synthesizer.block_size {
                self.process_events();
                self.block_wrote = 0;
                self.current_time += self.speed * self.synthesizer.block_size as f64
                    / self.synthesizer.sample_rate as f64;
            }

            let src_rem = self.synthesizer.block_size - self.block_wrote;
            let dst_rem = left_length - wrote;
            let rem = cmp::min(src_rem, dst_rem);

            self.synthesizer.render(
                &mut left[wrote..wrote + rem],
                &mut right[wrote..wrote + rem],
            );

            self.block_wrote += rem;
            wrote += rem;
        }
    }

    fn process_events(&mut self) {
        let midi_file = match self.midi_file.as_ref() {
            Some(value) => value,
            None => return,
        };

        while self.msg_index < midi_file.messages.len() {
            let time = midi_file.times[self.msg_index];
            let msg = midi_file.messages[self.msg_index];

            if time <= self.current_time {
                match msg {
                    Message::Normal {
                        status,
                        data1,
                        data2,
                    } => {
                        let channel = status & 0x0F;
                        let command = status & 0xF0;
                        self.synthesizer.process_midi_message(
                            channel as i32,
                            command as i32,
                            data1 as i32,
                            data2 as i32,
                        );
                    }
                    Message::LoopStart if self.play_loop => self.loop_index = self.msg_index,
                    Message::LoopEnd if self.play_loop => {
                        self.current_time = midi_file.times[self.loop_index];
                        self.msg_index = self.loop_index;
                        self.synthesizer.note_off_all(false);
                    }
                    _ => (),
                }
                self.msg_index += 1;
            } else {
                break;
            }
        }

        if self.msg_index == midi_file.messages.len() && self.play_loop {
            self.current_time = midi_file.times[self.loop_index];
            self.msg_index = self.loop_index;
            self.synthesizer.note_off_all(false);
        }
    }

    pub fn get_synthesizer(&self) -> &Synthesizer {
        &self.synthesizer
    }

    pub fn get_midi_file(&self) -> Option<&MidiFile> {
        match &self.midi_file {
            None => None,
            Some(value) => Some(value),
        }
    }

    /// Current playback position in seconds.
    pub fn get_position(&self) -> f64 {
        self.current_time
    }

    /// Returns `true` if playback has reached the end (or `play` was never called).
    /// Always `false` when looping is enabled.
    pub fn end_of_sequence(&self) -> bool {
        match &self.midi_file {
            None => true,
            Some(value) => self.msg_index == value.messages.len(),
        }
    }

    /// Playback speed multiplier (default 1.0).
    pub fn get_speed(&self) -> f64 {
        self.speed
    }

    /// Sets the playback speed. Must be non-negative.
    pub fn set_speed(&mut self, value: f64) {
        if value < 0.0 {
            panic!("The playback speed must be a non-negative value.");
        }

        self.speed = value;
    }
}
