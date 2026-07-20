use super::config::{Config, Mode, Source};
use super::events::Buffer;
use crate::butler::{CaptureId, CaptureWriter};
use arc_swap::ArcSwap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Armed = 0,
    Recording = 1,
    Overdubbing = 2,
    Stopped = 3,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PunchEvent {
    PunchIn { beat: f64, sample: Option<u64> },
    PunchOut { beat: f64, sample: Option<u64> },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct XRun {
    pub sample_position: u64,
    pub beat: Option<f64>,
    pub xrun_type: XRunType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XRunType {
    Underrun,
    Overrun,
}

impl From<u8> for State {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Armed,
            1 => Self::Recording,
            2 => Self::Overdubbing,
            _ => Self::Stopped,
        }
    }
}

pub struct Session {
    state: AtomicU8,
    config: Config,
    buffer: ArcSwap<Buffer>,
    preroll_remaining: AtomicU64,
    capture_id: AtomicU64,
    recording_file: ArcSwap<Option<PathBuf>>,
    capture_producer: ArcSwap<Option<Arc<CaptureWriter>>>,
    is_active: AtomicBool,
    record_safe: AtomicBool,
    punch_events: ArcSwap<Vec<PunchEvent>>,
    xrun_events: ArcSwap<Vec<XRun>>,
}

impl Session {
    pub fn new(config: Config, sample_rate: f64, start_beat: f64) -> Self {
        let buffer = Buffer::new(start_beat, sample_rate);
        let preroll_beats = config.preroll_beats;

        Self {
            state: AtomicU8::new(State::Armed as u8),
            config,
            buffer: ArcSwap::new(Arc::new(buffer)),
            preroll_remaining: AtomicU64::new(preroll_beats.to_bits()),
            capture_id: AtomicU64::new(0),
            recording_file: ArcSwap::new(Arc::new(None)),
            capture_producer: ArcSwap::new(Arc::new(None)),
            is_active: AtomicBool::new(true),
            record_safe: AtomicBool::new(false),
            punch_events: ArcSwap::new(Arc::new(Vec::new())),
            xrun_events: ArcSwap::new(Arc::new(Vec::new())),
        }
    }

    pub fn get_state(&self) -> State {
        State::from(self.state.load(Ordering::Acquire))
    }

    pub fn set_state(&self, state: State) {
        self.state.store(state as u8, Ordering::Release);
    }

    pub fn is_active(&self) -> bool {
        self.is_active.load(Ordering::Acquire)
    }

    pub fn deactivate(&self) {
        self.is_active.store(false, Ordering::Release);
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn channel_index(&self) -> usize {
        self.config.channel_index
    }

    pub fn source(&self) -> Source {
        self.config.source
    }

    pub fn mode(&self) -> Mode {
        self.config.mode
    }

    pub fn with_buffer<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Buffer) -> R,
    {
        let current = self.buffer.load_full();
        let mut modified = (*current).clone();
        let result = f(&mut modified);
        self.buffer.store(Arc::new(modified));

        result
    }

    pub fn get_buffer(&self) -> Arc<Buffer> {
        self.buffer.load_full()
    }

    pub fn swap_buffer(&self, new_buffer: Buffer) -> Arc<Buffer> {
        self.buffer.swap(Arc::new(new_buffer))
    }

    pub fn update_preroll(&self, delta_beats: f64) -> bool {
        let mut current_bits = self.preroll_remaining.load(Ordering::Acquire);
        loop {
            let current = f64::from_bits(current_bits);
            let new_value = current - delta_beats;
            let new_bits = new_value.to_bits();

            match self.preroll_remaining.compare_exchange_weak(
                current_bits,
                new_bits,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if new_value <= 0.0 {
                        let state = self.get_state();
                        if state == State::Armed {
                            let new_state = match self.config.mode {
                                Mode::Replace => State::Recording,
                                Mode::Overdub => State::Overdubbing,
                                Mode::Loop => State::Recording,
                            };
                            self.set_state(new_state);
                            return true;
                        }
                    }
                    return false;
                }
                Err(updated_bits) => {
                    current_bits = updated_bits;
                }
            }
        }
    }

    pub fn is_in_preroll(&self) -> bool {
        let bits = self.preroll_remaining.load(Ordering::Acquire);
        f64::from_bits(bits) > 0.0
    }

    pub fn preroll_remaining(&self) -> f64 {
        let bits = self.preroll_remaining.load(Ordering::Acquire);
        f64::from_bits(bits)
    }

    pub fn set_capture_id(&self, id: CaptureId) {
        self.capture_id.store(id.0, Ordering::Release);
    }

    pub fn get_capture_id(&self) -> Option<CaptureId> {
        let id_value = self.capture_id.load(Ordering::Acquire);
        if id_value == 0 {
            None
        } else {
            Some(CaptureId(id_value))
        }
    }

    pub fn set_recording_file(&self, path: PathBuf) {
        self.recording_file.store(Arc::new(Some(path)));
    }

    pub fn get_recording_file(&self) -> Option<PathBuf> {
        let guard = self.recording_file.load();
        guard.as_ref().clone()
    }

    pub fn set_capture_producer(&self, producer: CaptureWriter) {
        self.capture_producer
            .store(Arc::new(Some(Arc::new(producer))));
    }

    pub fn get_capture_producer(&self) -> Option<Arc<CaptureWriter>> {
        let guard = self.capture_producer.load();
        guard.as_ref().clone()
    }

    pub fn check_punch_in(&self, current_beat: f64) -> bool {
        self.config
            .punch_in
            .is_some_and(|punch_in| current_beat >= punch_in && self.get_state() == State::Armed)
    }

    pub fn check_punch_out(&self, current_beat: f64) -> bool {
        self.config.punch_out.is_some_and(|punch_out| {
            current_beat >= punch_out
                && (self.get_state() == State::Recording || self.get_state() == State::Overdubbing)
        })
    }

    pub fn set_record_safe(&self, safe: bool) {
        self.record_safe.store(safe, Ordering::Release);
    }

    pub fn is_record_safe(&self) -> bool {
        self.record_safe.load(Ordering::Acquire)
    }

    pub fn can_record(&self) -> bool {
        !self.is_record_safe()
    }

    pub fn process_punch(
        &self,
        current_beat: f64,
        sample_position: Option<u64>,
    ) -> Option<PunchEvent> {
        if self.is_record_safe() {
            return None;
        }

        let state = self.get_state();

        match state {
            State::Armed => {
                if self.check_punch_in(current_beat) {
                    let new_state = match self.config.mode {
                        Mode::Replace => State::Recording,
                        Mode::Overdub => State::Overdubbing,
                        Mode::Loop => State::Recording,
                    };
                    self.set_state(new_state);

                    let event = PunchEvent::PunchIn {
                        beat: current_beat,
                        sample: sample_position,
                    };
                    self.record_punch_event(event);
                    return Some(event);
                }
            }
            State::Recording | State::Overdubbing => {
                if self.check_punch_out(current_beat) {
                    self.set_state(State::Stopped);

                    let event = PunchEvent::PunchOut {
                        beat: current_beat,
                        sample: sample_position,
                    };
                    self.record_punch_event(event);
                    return Some(event);
                }
            }
            State::Stopped => {}
        }

        None
    }

    fn record_punch_event(&self, event: PunchEvent) {
        push_cow(&self.punch_events, event);
    }

    pub fn get_punch_events(&self) -> Vec<PunchEvent> {
        let events = self.punch_events.load_full();
        (*events).clone()
    }

    pub fn record_xrun(&self, sample_position: u64, beat: Option<f64>, xrun_type: XRunType) {
        let event = XRun {
            sample_position,
            beat,
            xrun_type,
        };

        push_cow(&self.xrun_events, event);
    }

    pub fn get_xrun_events(&self) -> Vec<XRun> {
        let events = self.xrun_events.load_full();
        (*events).clone()
    }

    pub fn xrun_count(&self) -> usize {
        self.xrun_events.load().len()
    }

    pub fn has_xruns(&self) -> bool {
        !self.xrun_events.load().is_empty()
    }
}

fn push_cow<T: Clone>(slot: &ArcSwap<Vec<T>>, item: T) {
    let current = slot.load_full();
    let mut items = (*current).clone();
    items.push(item);
    slot.store(Arc::new(items));
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("state", &self.get_state())
            .field("channel_index", &self.config.channel_index)
            .field("source", &self.config.source)
            .field("mode", &self.config.mode)
            .field("is_active", &self.is_active())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub enum Recorded {
    Midi {
        buffer: Buffer,
        punch_events: Vec<PunchEvent>,
        xrun_events: Vec<XRun>,
    },
    Audio {
        file_path: PathBuf,
        duration_seconds: f64,
        /// Frames dropped by capture ring overruns during this recording.
        /// Nonzero means the capture ring overran and samples were lost.
        frames_dropped: u64,
        punch_events: Vec<PunchEvent>,
        xrun_events: Vec<XRun>,
    },
    InternalAudio {
        buffer: Buffer,
        punch_events: Vec<PunchEvent>,
        xrun_events: Vec<XRun>,
    },
    Pattern {
        buffer: Buffer,
        punch_events: Vec<PunchEvent>,
        xrun_events: Vec<XRun>,
    },
}

impl Recorded {
    pub fn punch_events(&self) -> &[PunchEvent] {
        match self {
            Self::Midi { punch_events, .. }
            | Self::Audio { punch_events, .. }
            | Self::InternalAudio { punch_events, .. }
            | Self::Pattern { punch_events, .. } => punch_events,
        }
    }

    pub fn xrun_events(&self) -> &[XRun] {
        match self {
            Self::Midi { xrun_events, .. }
            | Self::Audio { xrun_events, .. }
            | Self::InternalAudio { xrun_events, .. }
            | Self::Pattern { xrun_events, .. } => xrun_events,
        }
    }

    pub fn has_xruns(&self) -> bool {
        !self.xrun_events().is_empty()
    }

    pub fn xrun_count(&self) -> usize {
        self.xrun_events().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recording_state_conversion() {
        assert_eq!(State::from(0), State::Armed);
        assert_eq!(State::from(1), State::Recording);
        assert_eq!(State::from(2), State::Overdubbing);
        assert_eq!(State::from(3), State::Stopped);
        assert_eq!(State::from(99), State::Stopped);
    }

    #[test]
    fn test_recording_session_creation() {
        let config = Config::default();
        let session = Session::new(config, 44100.0, 0.0);

        assert_eq!(session.get_state(), State::Armed);
        assert!(session.is_active());
        assert_eq!(session.channel_index(), 0);
    }

    #[test]
    fn test_preroll_countdown() {
        let config = Config {
            preroll_beats: 4.0,
            ..Default::default()
        };

        let session = Session::new(config, 44100.0, 0.0);

        assert!(session.is_in_preroll());
        assert!(!session.update_preroll(2.0));
        assert!(session.is_in_preroll());
        assert!(session.update_preroll(2.5));
        assert!(!session.is_in_preroll());
        assert_eq!(session.get_state(), State::Recording);
    }

    #[test]
    fn test_punch_in_out() {
        let config = Config {
            punch_in: Some(4.0),
            punch_out: Some(8.0),
            ..Default::default()
        };

        let session = Session::new(config, 44100.0, 0.0);

        assert!(!session.check_punch_in(0.0));
        assert!(session.check_punch_in(4.0));
        assert!(!session.check_punch_out(4.0));

        session.set_state(State::Recording);
        assert!(session.check_punch_out(8.0));
    }

    #[test]
    fn test_automatic_punch_processing() {
        let config = Config {
            punch_in: Some(4.0),
            punch_out: Some(8.0),
            preroll_beats: 0.0, // No preroll for this test
            ..Default::default()
        };

        let session = Session::new(config, 44100.0, 0.0);

        assert!(session.process_punch(2.0, Some(88200)).is_none());
        assert_eq!(session.get_state(), State::Armed);

        let event = session.process_punch(4.0, Some(176400));
        assert!(event.is_some());
        assert!(matches!(
            event.unwrap(),
            PunchEvent::PunchIn { beat: 4.0, .. }
        ));
        assert_eq!(session.get_state(), State::Recording);

        assert!(session.process_punch(6.0, Some(264600)).is_none());
        assert_eq!(session.get_state(), State::Recording);

        let event = session.process_punch(8.0, Some(352800));
        assert!(event.is_some());
        assert!(matches!(
            event.unwrap(),
            PunchEvent::PunchOut { beat: 8.0, .. }
        ));
        assert_eq!(session.get_state(), State::Stopped);

        // Verify punch events were recorded
        let events = session.get_punch_events();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn test_record_safe_mode() {
        let config = Config {
            punch_in: Some(4.0),
            ..Default::default()
        };

        let session = Session::new(config, 44100.0, 0.0);

        assert!(!session.is_record_safe());
        assert!(session.can_record());

        session.set_record_safe(true);
        assert!(session.is_record_safe());
        assert!(!session.can_record());

        let event = session.process_punch(4.0, None);
        assert!(event.is_none());
        assert_eq!(session.get_state(), State::Armed);

        session.set_record_safe(false);
        assert!(session.can_record());

        let event = session.process_punch(4.0, None);
        assert!(event.is_some());
        assert_eq!(session.get_state(), State::Recording);
    }

    #[test]
    fn test_xrun_tracking() {
        let config = Config::default();
        let session = Session::new(config, 44100.0, 0.0);

        assert!(!session.has_xruns());
        assert_eq!(session.xrun_count(), 0);

        session.record_xrun(44100, Some(1.0), XRunType::Underrun);
        session.record_xrun(88200, Some(2.0), XRunType::Overrun);

        assert!(session.has_xruns());
        assert_eq!(session.xrun_count(), 2);

        let events = session.get_xrun_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].xrun_type, XRunType::Underrun);
        assert_eq!(events[1].xrun_type, XRunType::Overrun);
        assert_eq!(events[0].sample_position, 44100);
    }
}
