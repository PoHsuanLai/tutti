use crate::butler::{ButlerCommand, CaptureBuffer, CaptureIdGen};
use crate::capture::{Buffer, Config, Mode, PunchEvent, Recorded, Session, Source, State, XRun};
use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub struct Recorder {
    sessions: Arc<dashmap::DashMap<usize, Arc<Session>>>,
    butler_tx: smol::channel::Sender<ButlerCommand>,
    sample_rate: f64,
    capture_ids: CaptureIdGen,
}

impl Recorder {
    pub(crate) fn new(
        _initial_track_count: usize,
        butler_tx: smol::channel::Sender<ButlerCommand>,
        sample_rate: f64,
        capture_ids: CaptureIdGen,
    ) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            butler_tx,
            sample_rate,
            capture_ids,
        }
    }

    pub fn resize(&self, _new_track_count: usize) {}

    pub fn start_recording(
        &self,
        channel_index: usize,
        source: Source,
        mode: Mode,
        current_beat: f64,
    ) -> crate::error::Result<()> {
        let config = Config {
            channel_index,
            source,
            mode,
            ..Default::default()
        };
        self.start_recording_with_config(config, current_beat)
    }

    pub fn start_recording_with_config(
        &self,
        config: Config,
        current_beat: f64,
    ) -> crate::error::Result<()> {
        let source = config.source;
        let channel_index = config.channel_index;

        let session = Session::new(config, self.sample_rate, current_beat);

        if source == Source::AudioInput {
            self.setup_audio_input_capture(&session)?;
        }

        self.sessions.insert(channel_index, Arc::new(session));

        Ok(())
    }

    pub fn stop_recording(&self, channel_index: usize) -> crate::error::Result<Recorded> {
        let session = self
            .sessions
            .get(&channel_index)
            .ok_or_else(|| {
                crate::error::Error::Recording(format!(
                    "No recording session on channel {}",
                    channel_index
                ))
            })
            .map(|s| Arc::clone(&s))?;

        session.set_state(State::Stopped);
        session.deactivate();

        let punch_events = session.get_punch_events();
        let xrun_events = session.get_xrun_events();

        let data = match session.source() {
            Source::MidiInput => {
                let buffer =
                    Arc::unwrap_or_clone(session.swap_buffer(Buffer::new(0.0, self.sample_rate)));
                Recorded::Midi {
                    buffer,
                    punch_events,
                    xrun_events,
                }
            }
            Source::AudioInput => {
                self.stop_audio_input_capture(&session, punch_events, xrun_events)?
            }
            Source::InternalAudio => {
                let buffer =
                    Arc::unwrap_or_clone(session.swap_buffer(Buffer::new(0.0, self.sample_rate)));
                Recorded::InternalAudio {
                    buffer,
                    punch_events,
                    xrun_events,
                }
            }
            Source::Pattern => {
                let buffer =
                    Arc::unwrap_or_clone(session.swap_buffer(Buffer::new(0.0, self.sample_rate)));
                Recorded::Pattern {
                    buffer,
                    punch_events,
                    xrun_events,
                }
            }
        };

        self.sessions.remove(&channel_index);

        Ok(data)
    }

    pub fn is_recording(&self, channel_index: usize) -> bool {
        self.sessions
            .get(&channel_index)
            .is_some_and(|s| s.is_active())
    }

    pub fn get_recording_state(&self, channel_index: usize) -> Option<State> {
        self.sessions.get(&channel_index).map(|s| s.get_state())
    }

    pub fn update_prerolls(&self, delta_beats: f64) -> usize {
        let mut completed_count = 0;
        for session in self.sessions.iter() {
            if session.get_state() == State::Armed && session.update_preroll(delta_beats) {
                completed_count += 1;
            }
        }
        completed_count
    }

    pub fn preroll_sessions(&self) -> Vec<Arc<Session>> {
        self.sessions
            .iter()
            .filter(|entry| entry.is_in_preroll())
            .map(|entry| Arc::clone(entry.value()))
            .collect()
    }

    pub fn has_preroll_sessions(&self) -> bool {
        self.sessions.iter().any(|entry| entry.is_in_preroll())
    }

    pub fn has_active_recording(&self) -> bool {
        self.sessions
            .iter()
            .any(|entry| matches!(entry.get_state(), State::Recording | State::Overdubbing))
    }

    pub fn process_punch_all(&self, current_beat: f64, sample_position: Option<u64>) -> usize {
        let mut transitions = 0;
        for session in self.sessions.iter() {
            if session
                .process_punch(current_beat, sample_position)
                .is_some()
            {
                transitions += 1;
            }
        }
        transitions
    }

    pub fn set_record_safe(&self, channel_index: usize, safe: bool) -> crate::error::Result<()> {
        self.with_session(channel_index, |s| s.set_record_safe(safe))
    }

    pub fn is_record_safe(&self, channel_index: usize) -> bool {
        self.sessions
            .get(&channel_index)
            .is_some_and(|s| s.is_record_safe())
    }

    pub fn record_xrun(
        &self,
        channel_index: usize,
        sample_position: u64,
        beat: Option<f64>,
        xrun_type: crate::recording::capture::session::XRunType,
    ) -> crate::error::Result<()> {
        self.with_session(channel_index, |s| {
            s.record_xrun(sample_position, beat, xrun_type)
        })
    }

    pub fn xrun_count(&self, channel_index: usize) -> usize {
        self.sessions
            .get(&channel_index)
            .map_or(0, |s| s.xrun_count())
    }

    pub fn has_xruns(&self) -> bool {
        self.sessions.iter().any(|entry| entry.has_xruns())
    }

    pub fn record_midi_note_on(
        &self,
        channel_index: usize,
        note: u8,
        velocity: u8,
        beat: f64,
        channel: u8,
        sample_position: Option<u64>,
    ) -> crate::error::Result<()> {
        self.with_active_buffer(channel_index, Source::MidiInput, |buffer| {
            buffer.record_midi_note_on(note, velocity, beat, channel, sample_position);
        })
    }

    pub fn record_midi_note_off(
        &self,
        channel_index: usize,
        note: u8,
        beat: f64,
        channel: u8,
        sample_position: Option<u64>,
    ) -> crate::error::Result<()> {
        self.with_active_buffer(channel_index, Source::MidiInput, |buffer| {
            buffer.record_midi_note_off(note, beat, channel, sample_position);
        })
    }

    pub fn record_midi_cc(
        &self,
        channel_index: usize,
        channel: u8,
        controller: u8,
        value: u8,
        beat: f64,
        sample_position: Option<u64>,
    ) -> crate::error::Result<()> {
        self.with_active_buffer(channel_index, Source::MidiInput, |buffer| {
            buffer.record_midi_cc(channel, controller, value, beat, sample_position);
        })
    }

    pub fn record_pattern_trigger(
        &self,
        channel_index: usize,
        symbol: String,
        step: u32,
        beat: f64,
        velocity: f32,
    ) -> crate::error::Result<()> {
        self.with_active_buffer(channel_index, Source::Pattern, |buffer| {
            buffer.record_pattern_trigger(symbol, step, beat, velocity);
        })
    }

    /// Look up a session by channel; error if absent.
    fn with_session<R>(
        &self,
        channel_index: usize,
        f: impl FnOnce(&Session) -> R,
    ) -> crate::error::Result<R> {
        let session = self.sessions.get(&channel_index).ok_or_else(|| {
            crate::error::Error::Recording(format!(
                "No recording session on channel {}",
                channel_index
            ))
        })?;
        Ok(f(&session))
    }

    /// Look up a session, assert it's active and recording the expected source,
    /// then run `f` against its recording buffer.
    fn with_active_buffer<F: FnOnce(&mut Buffer)>(
        &self,
        channel_index: usize,
        expected_source: Source,
        f: F,
    ) -> crate::error::Result<()> {
        self.with_session(channel_index, |session| {
            if !session.is_active() {
                return Err(crate::error::Error::Recording(format!(
                    "Recording session on channel {} is not active",
                    channel_index
                )));
            }
            if session.source() != expected_source {
                return Err(crate::error::Error::Recording(format!(
                    "Channel {} is not recording {:?} (source: {:?})",
                    channel_index,
                    expected_source,
                    session.source()
                )));
            }
            session.with_buffer(f);
            Ok(())
        })?
    }

    #[inline]
    pub fn get_session(&self, channel_index: usize) -> Option<Arc<Session>> {
        self.sessions.get(&channel_index).map(|r| Arc::clone(&*r))
    }

    #[inline]
    pub fn get_capture_producer(
        &self,
        channel_index: usize,
    ) -> Option<Arc<crate::butler::CaptureWriter>> {
        self.sessions.get(&channel_index)?.get_capture_producer()
    }

    fn setup_audio_input_capture(&self, session: &Session) -> crate::error::Result<()> {
        let capture_id = self.capture_ids.mint();

        let file_path = PathBuf::from(format!(
            "recordings/track_{}_{}.wav",
            session.channel_index(),
            capture_id.0
        ));

        let (producer, consumer) = CaptureBuffer::new(
            file_path.clone(),
            self.sample_rate,
            1000.0, // 1 second buffer
        );

        self.butler_tx
            .send_blocking(ButlerCommand::RegisterCapture {
                capture_id,
                consumer,
                file_path: file_path.clone(),
                sample_rate: self.sample_rate,
                channels: 2,
            })
            .map_err(|e| {
                crate::error::Error::Recording(format!("Failed to send RegisterCapture: {}", e))
            })?;

        session.set_capture_id(capture_id);
        session.set_recording_file(file_path);
        session.set_capture_producer(producer);

        Ok(())
    }

    fn stop_audio_input_capture(
        &self,
        session: &Session,
        punch_events: Vec<PunchEvent>,
        xrun_events: Vec<XRun>,
    ) -> crate::error::Result<Recorded> {
        let capture_id = session.get_capture_id().ok_or_else(|| {
            crate::error::Error::Recording("No capture ID for audio input recording".to_string())
        })?;

        let file_path = session
            .get_recording_file()
            .ok_or_else(|| crate::error::Error::Recording("No recording file path".to_string()))?;

        self.butler_tx
            .send_blocking(ButlerCommand::Flush(capture_id))
            .map_err(|e| crate::error::Error::Recording(format!("Failed to send Flush: {}", e)))?;

        self.butler_tx
            .send_blocking(ButlerCommand::RemoveCapture(capture_id))
            .map_err(|e| {
                crate::error::Error::Recording(format!("Failed to send RemoveCapture: {}", e))
            })?;

        // Probe the just-written WAV header for its duration.
        let duration_seconds = hound::WavReader::open(&file_path)
            .map(|reader| {
                let spec = reader.spec();
                if spec.sample_rate == 0 {
                    0.0
                } else {
                    reader.duration() as f64 / f64::from(spec.sample_rate)
                }
            })
            .unwrap_or(0.0);

        Ok(Recorded::Audio {
            file_path,
            duration_seconds,
            punch_events,
            xrun_events,
        })
    }
}

impl Default for Recorder {
    fn default() -> Self {
        let (tx, _rx) = smol::channel::unbounded();
        Self::new(0, tx, 44100.0, CaptureIdGen::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_manager() -> Recorder {
        let (tx, _rx) = smol::channel::unbounded();
        Recorder::new(8, tx, 44100.0, CaptureIdGen::new())
    }

    #[test]
    fn test_recording_manager_creation() {
        let manager = create_test_manager();
        assert!(!manager.is_recording(0));
        assert!(!manager.is_recording(7));
        assert_eq!(manager.get_recording_state(0), None);
    }

    #[test]
    fn test_resize() {
        let manager = create_test_manager();
        manager.resize(16);
        assert!(!manager.is_recording(0));
    }

    #[test]
    fn test_start_stop_midi_recording() {
        let manager = create_test_manager();

        manager
            .start_recording(0, Source::MidiInput, Mode::Replace, 0.0)
            .unwrap();

        assert!(manager.is_recording(0));
        assert_eq!(manager.get_recording_state(0), Some(State::Armed));

        // Record some MIDI events
        manager
            .record_midi_note_on(0, 60, 100, 0.0, 0, None)
            .unwrap();
        manager.record_midi_note_off(0, 60, 1.0, 0, None).unwrap();

        let data = manager.stop_recording(0).unwrap();
        assert!(!manager.is_recording(0));

        match data {
            Recorded::Midi { buffer, .. } => {
                assert_eq!(buffer.midi_events.len(), 1);
                assert_eq!(buffer.midi_events[0].note, 60);
                assert_eq!(buffer.midi_events[0].duration, 1.0);
            }
            _ => panic!("Expected MIDI data"),
        }
    }

    #[test]
    fn test_pattern_recording() {
        let manager = create_test_manager();

        manager
            .start_recording(0, Source::Pattern, Mode::Replace, 0.0)
            .unwrap();

        let _ = manager.record_pattern_trigger(0, "bd".to_string(), 0, 0.0, 1.0);
        let _ = manager.record_pattern_trigger(0, "sn".to_string(), 4, 1.0, 0.8);

        let data = manager.stop_recording(0).unwrap();

        match data {
            Recorded::Pattern { buffer, .. } => {
                assert_eq!(buffer.pattern_events.len(), 2);
                assert_eq!(buffer.pattern_events[0].symbol, "bd");
                assert_eq!(buffer.pattern_events[1].step, 4);
            }
            _ => panic!("Expected Pattern data"),
        }
    }

    #[test]
    fn test_concurrent_recording() {
        let manager = create_test_manager();

        manager
            .start_recording(0, Source::MidiInput, Mode::Replace, 0.0)
            .unwrap();
        manager
            .start_recording(1, Source::Pattern, Mode::Overdub, 0.0)
            .unwrap();

        assert!(manager.is_recording(0));
        assert!(manager.is_recording(1));
        assert!(!manager.is_recording(2));

        manager
            .record_midi_note_on(0, 60, 100, 0.0, 0, None)
            .unwrap();
        let _ = manager.record_pattern_trigger(1, "bd".to_string(), 0, 0.0, 1.0);

        let data0 = manager.stop_recording(0).unwrap();
        assert!(!manager.is_recording(0));
        assert!(manager.is_recording(1));

        match data0 {
            Recorded::Midi { buffer, .. } => {
                assert_eq!(buffer.active_note_count(), 1);
            }
            _ => panic!("Expected MIDI data"),
        }
    }

    #[test]
    fn test_record_safe_mode() {
        let manager = create_test_manager();

        manager
            .start_recording(0, Source::MidiInput, Mode::Replace, 0.0)
            .unwrap();

        assert!(!manager.is_record_safe(0));

        manager.set_record_safe(0, true).unwrap();
        assert!(manager.is_record_safe(0));

        manager.set_record_safe(0, false).unwrap();
        assert!(!manager.is_record_safe(0));

        assert!(!manager.is_record_safe(99));
    }

    #[test]
    fn test_xrun_tracking() {
        use crate::recording::capture::session::XRunType;

        let manager = create_test_manager();

        manager
            .start_recording(0, Source::MidiInput, Mode::Replace, 0.0)
            .unwrap();

        assert_eq!(manager.xrun_count(0), 0);
        assert!(!manager.has_xruns());

        manager
            .record_xrun(0, 44100, Some(1.0), XRunType::Underrun)
            .unwrap();

        assert_eq!(manager.xrun_count(0), 1);
        assert!(manager.has_xruns());

        manager
            .record_xrun(0, 88200, Some(2.0), XRunType::Overrun)
            .unwrap();

        assert_eq!(manager.xrun_count(0), 2);
    }

    #[test]
    fn test_punch_all_sessions() {
        let manager = create_test_manager();

        let config = crate::capture::Config {
            channel_index: 0,
            source: Source::MidiInput,
            punch_in: Some(4.0),
            punch_out: Some(8.0),
            ..Default::default()
        };

        let session = crate::capture::Session::new(config, 44100.0, 0.0);
        manager.sessions.insert(0, std::sync::Arc::new(session));

        assert_eq!(manager.get_recording_state(0), Some(State::Armed));
        assert_eq!(manager.process_punch_all(0.0, None), 0);

        assert_eq!(manager.process_punch_all(4.0, None), 1);
        assert_eq!(manager.get_recording_state(0), Some(State::Recording));

        assert_eq!(manager.process_punch_all(6.0, None), 0);

        assert_eq!(manager.process_punch_all(8.0, None), 1);
        assert_eq!(manager.get_recording_state(0), Some(State::Stopped));
    }

    #[test]
    fn test_punch_events_in_recorded_data() {
        let manager = create_test_manager();

        let config = crate::capture::Config {
            channel_index: 0,
            source: Source::MidiInput,
            punch_in: Some(4.0),
            punch_out: Some(8.0),
            ..Default::default()
        };

        let session = crate::capture::Session::new(config, 44100.0, 0.0);
        manager.sessions.insert(0, std::sync::Arc::new(session));

        manager.process_punch_all(4.0, Some(176400));
        manager.process_punch_all(8.0, Some(352800));

        let data = manager.stop_recording(0).unwrap();

        match data {
            Recorded::Midi { punch_events, .. } => {
                assert_eq!(punch_events.len(), 2);
                assert!(matches!(
                    punch_events[0],
                    crate::capture::PunchEvent::PunchIn { beat: 4.0, .. }
                ));
                assert!(matches!(
                    punch_events[1],
                    crate::capture::PunchEvent::PunchOut { beat: 8.0, .. }
                ));
            }
            _ => panic!("Expected MIDI data"),
        }
    }
}
