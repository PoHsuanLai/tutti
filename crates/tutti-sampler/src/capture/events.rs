use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct RecordedMidiEvent {
    pub note: u8,
    pub velocity: u8,
    pub velocity_16bit: Option<u16>,
    pub start_beat: f64,
    pub duration: f64,
    pub channel: u8,
    pub start_sample: Option<u64>,
    pub end_sample: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RecordedCCEvent {
    pub channel: u8,
    pub controller: u8,
    pub value: u8,
    pub value_32bit: Option<u32>,
    pub beat: f64,
    pub sample: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RecordedPitchBendEvent {
    pub channel: u8,
    pub value: i16,
    pub beat: f64,
    pub sample: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RecordedPressureEvent {
    pub channel: u8,
    pub pressure: u8,
    pub beat: f64,
    pub note: Option<u8>,
    pub sample: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RecordedProgramChangeEvent {
    pub channel: u8,
    pub program: u8,
    pub beat: f64,
    pub sample: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RecordedPerNotePitchBendEvent {
    pub channel: u8,
    pub note: u8,
    pub bend: u32,
    pub beat: f64,
    pub sample: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RecordedPerNoteControllerEvent {
    pub channel: u8,
    pub note: u8,
    pub index: u8,
    pub value: u32,
    pub is_registered: bool,
    pub beat: f64,
    pub sample: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct RecordedAudioChunk {
    pub samples: Vec<f32>,
    pub start_beat: f64,
}

impl RecordedAudioChunk {
    pub fn new(capacity: usize, start_beat: f64) -> Self {
        Self {
            samples: Vec::with_capacity(capacity),
            start_beat,
        }
    }

    pub fn push_sample(&mut self, left: f32, right: f32) {
        self.samples.push(left);
        self.samples.push(right);
    }

    pub fn frame_count(&self) -> usize {
        self.samples.len() / 2
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn is_full(&self, max_frames: usize) -> bool {
        self.frame_count() >= max_frames
    }
}

#[derive(Debug, Clone)]
pub struct RecordedPatternEvent {
    pub symbol: String,
    pub step: u32,
    pub beat: f64,
    pub velocity: f32,
}

#[derive(Debug, Clone)]
struct ActiveNoteData {
    start_beat: f64,
    velocity: u8,
    velocity_16bit: Option<u16>,
    channel: u8,
    start_sample: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Buffer {
    pub midi_events: Vec<RecordedMidiEvent>,
    active_notes: std::collections::HashMap<u8, ActiveNoteData>,
    pub cc_events: Vec<RecordedCCEvent>,
    pub pitch_bend_events: Vec<RecordedPitchBendEvent>,
    pub pressure_events: Vec<RecordedPressureEvent>,
    pub program_change_events: Vec<RecordedProgramChangeEvent>,
    pub per_note_pitch_bend_events: Vec<RecordedPerNotePitchBendEvent>,
    pub per_note_controller_events: Vec<RecordedPerNoteControllerEvent>,
    pub audio_chunks: VecDeque<RecordedAudioChunk>,
    pub max_chunk_frames: usize,
    pub audio_frame_count: usize,
    pub pattern_events: Vec<RecordedPatternEvent>,
    pub start_beat: f64,
    pub current_beat: f64,
    pub sample_rate: f64,
}

impl Buffer {
    pub fn new(start_beat: f64, sample_rate: f64) -> Self {
        Self {
            midi_events: Vec::new(),
            active_notes: std::collections::HashMap::new(),
            cc_events: Vec::new(),
            pitch_bend_events: Vec::new(),
            pressure_events: Vec::new(),
            program_change_events: Vec::new(),
            per_note_pitch_bend_events: Vec::new(),
            per_note_controller_events: Vec::new(),
            audio_chunks: VecDeque::new(),
            max_chunk_frames: 8192, // ~186ms at 44.1kHz
            audio_frame_count: 0,
            pattern_events: Vec::new(),
            start_beat,
            current_beat: start_beat,
            sample_rate,
        }
    }

    pub fn record_midi_note_on(
        &mut self,
        note: u8,
        velocity: u8,
        beat: f64,
        channel: u8,
        sample: Option<u64>,
    ) {
        self.active_notes.insert(
            note,
            ActiveNoteData {
                start_beat: beat,
                velocity,
                velocity_16bit: None,
                channel,
                start_sample: sample,
            },
        );
        self.current_beat = beat.max(self.current_beat);
    }

    pub fn record_midi2_note_on(
        &mut self,
        note: u8,
        velocity: u8,
        velocity_16bit: u16,
        beat: f64,
        channel: u8,
        sample_position: Option<u64>,
    ) {
        self.active_notes.insert(
            note,
            ActiveNoteData {
                start_beat: beat,
                velocity,
                velocity_16bit: Some(velocity_16bit),
                channel,
                start_sample: sample_position,
            },
        );
        self.current_beat = beat.max(self.current_beat);
    }

    pub fn record_midi_note_off(&mut self, note: u8, beat: f64, _channel: u8, sample: Option<u64>) {
        if let Some(data) = self.active_notes.remove(&note) {
            let duration = beat - data.start_beat;
            if duration > 0.0 {
                self.midi_events.push(RecordedMidiEvent {
                    note,
                    velocity: data.velocity,
                    velocity_16bit: data.velocity_16bit,
                    start_beat: data.start_beat,
                    duration,
                    channel: data.channel,
                    start_sample: data.start_sample,
                    end_sample: sample,
                });
            }
        }
        self.current_beat = beat.max(self.current_beat);
    }

    pub fn record_midi_cc(
        &mut self,
        channel: u8,
        controller: u8,
        value: u8,
        beat: f64,
        sample: Option<u64>,
    ) {
        self.cc_events.push(RecordedCCEvent {
            channel,
            controller,
            value,
            value_32bit: None,
            beat,
            sample,
        });
        self.current_beat = beat.max(self.current_beat);
    }

    pub fn record_midi2_cc(
        &mut self,
        channel: u8,
        controller: u8,
        value: u8,
        value_32bit: u32,
        beat: f64,
        sample_position: Option<u64>,
    ) {
        self.cc_events.push(RecordedCCEvent {
            channel,
            controller,
            value,
            value_32bit: Some(value_32bit),
            beat,
            sample: sample_position,
        });
        self.current_beat = beat.max(self.current_beat);
    }

    pub fn record_midi2_per_note_pitch_bend(
        &mut self,
        channel: u8,
        note: u8,
        bend: u32,
        beat: f64,
        sample_position: Option<u64>,
    ) {
        self.per_note_pitch_bend_events
            .push(RecordedPerNotePitchBendEvent {
                channel,
                note,
                bend,
                beat,
                sample: sample_position,
            });
        self.current_beat = beat.max(self.current_beat);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_midi2_per_note_controller(
        &mut self,
        channel: u8,
        note: u8,
        index: u8,
        value: u32,
        is_registered: bool,
        beat: f64,
        sample_position: Option<u64>,
    ) {
        self.per_note_controller_events
            .push(RecordedPerNoteControllerEvent {
                channel,
                note,
                index,
                value,
                is_registered,
                beat,
                sample: sample_position,
            });
        self.current_beat = beat.max(self.current_beat);
    }

    pub fn record_midi_pitch_bend(
        &mut self,
        channel: u8,
        value: i16,
        beat: f64,
        sample: Option<u64>,
    ) {
        self.pitch_bend_events.push(RecordedPitchBendEvent {
            channel,
            value,
            beat,
            sample,
        });
        self.current_beat = beat.max(self.current_beat);
    }

    pub fn record_midi_channel_pressure(
        &mut self,
        channel: u8,
        pressure: u8,
        beat: f64,
        sample: Option<u64>,
    ) {
        self.pressure_events.push(RecordedPressureEvent {
            channel,
            pressure,
            beat,
            note: None,
            sample,
        });
        self.current_beat = beat.max(self.current_beat);
    }

    pub fn record_midi_poly_pressure(
        &mut self,
        channel: u8,
        note: u8,
        pressure: u8,
        beat: f64,
        sample: Option<u64>,
    ) {
        self.pressure_events.push(RecordedPressureEvent {
            channel,
            pressure,
            beat,
            note: Some(note),
            sample,
        });
        self.current_beat = beat.max(self.current_beat);
    }

    pub fn record_midi_program_change(
        &mut self,
        channel: u8,
        program: u8,
        beat: f64,
        sample: Option<u64>,
    ) {
        self.program_change_events.push(RecordedProgramChangeEvent {
            channel,
            program,
            beat,
            sample,
        });
        self.current_beat = beat.max(self.current_beat);
    }

    pub fn record_audio(&mut self, left: f32, right: f32, beat: f64) {
        if self
            .audio_chunks
            .back()
            .is_none_or(|chunk| chunk.is_full(self.max_chunk_frames))
        {
            self.audio_chunks
                .push_back(RecordedAudioChunk::new(self.max_chunk_frames * 2, beat));
        }

        if let Some(chunk) = self.audio_chunks.back_mut() {
            chunk.push_sample(left, right);
        }

        self.audio_frame_count += 1;
        self.current_beat = beat.max(self.current_beat);
    }

    pub fn record_pattern_trigger(&mut self, symbol: String, step: u32, beat: f64, velocity: f32) {
        self.pattern_events.push(RecordedPatternEvent {
            symbol,
            step,
            beat,
            velocity,
        });
        self.current_beat = beat.max(self.current_beat);
    }

    pub fn duration_beats(&self) -> f64 {
        self.current_beat - self.start_beat
    }

    pub fn duration_seconds(&self) -> f64 {
        if self.audio_frame_count == 0 {
            0.0
        } else {
            self.audio_frame_count as f64 / self.sample_rate
        }
    }

    pub fn is_empty(&self) -> bool {
        let base_empty = self.midi_events.is_empty()
            && self.active_notes.is_empty()
            && self.cc_events.is_empty()
            && self.pitch_bend_events.is_empty()
            && self.pressure_events.is_empty()
            && self.program_change_events.is_empty()
            && self.audio_chunks.is_empty()
            && self.pattern_events.is_empty();

        base_empty
            && self.per_note_pitch_bend_events.is_empty()
            && self.per_note_controller_events.is_empty()
    }

    pub fn clear(&mut self) {
        self.midi_events.clear();
        self.active_notes.clear();
        self.cc_events.clear();
        self.pitch_bend_events.clear();
        self.pressure_events.clear();
        self.program_change_events.clear();
        self.per_note_pitch_bend_events.clear();
        self.per_note_controller_events.clear();
        self.audio_chunks.clear();
        self.audio_frame_count = 0;
        self.pattern_events.clear();
        self.current_beat = self.start_beat;
    }

    pub fn active_note_count(&self) -> usize {
        self.active_notes.len()
    }

    pub fn has_active_notes(&self) -> bool {
        !self.active_notes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_midi_note_recording() {
        let mut buffer = Buffer::new(0.0, 44100.0);

        // Record note on
        buffer.record_midi_note_on(60, 100, 0.0, 0, None);
        assert_eq!(buffer.active_note_count(), 1);
        assert_eq!(buffer.midi_events.len(), 0);

        // Record note off
        buffer.record_midi_note_off(60, 1.0, 0, None);
        assert_eq!(buffer.active_note_count(), 0);
        assert_eq!(buffer.midi_events.len(), 1);
        assert_eq!(buffer.midi_events[0].duration, 1.0);
    }

    #[test]
    fn test_audio_chunking() {
        let mut buffer = Buffer::new(0.0, 44100.0);
        buffer.max_chunk_frames = 4; // Small chunks for testing

        // Record samples
        for i in 0..10 {
            buffer.record_audio(i as f32, i as f32, i as f64);
        }

        // Should create multiple chunks
        assert!(buffer.audio_chunks.len() > 1);
        assert_eq!(buffer.audio_frame_count, 10);
    }

    #[test]
    fn test_pattern_recording() {
        let mut buffer = Buffer::new(0.0, 44100.0);

        buffer.record_pattern_trigger("bd".to_string(), 0, 0.0, 1.0);
        buffer.record_pattern_trigger("sn".to_string(), 4, 1.0, 0.8);

        assert_eq!(buffer.pattern_events.len(), 2);
        assert_eq!(buffer.pattern_events[0].symbol, "bd");
        assert_eq!(buffer.pattern_events[1].step, 4);
    }
}
