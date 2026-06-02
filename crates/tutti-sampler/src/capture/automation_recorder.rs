use super::automation_target::RecordingTarget;
use audio_automation::{AutomationEnvelope, AutomationPoint, AutomationState, CurveType};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AutomationRecordingConfig {
    pub min_point_interval: f64,
    pub simplify_tolerance: f32,
    pub auto_simplify: bool,
    pub default_curve: CurveType,
}

impl Default for AutomationRecordingConfig {
    fn default() -> Self {
        Self {
            min_point_interval: 0.01,
            simplify_tolerance: 0.01,
            auto_simplify: true,
            default_curve: CurveType::Linear,
        }
    }
}

#[derive(Debug, Clone)]
struct Session {
    last_recorded_beat: f64,
    last_value: f32,
    is_touching: bool,
}

#[derive(Debug)]
pub struct Recorder<T: RecordingTarget> {
    envelope: Arc<RwLock<AutomationEnvelope<T>>>,
    state: AutomationState,
    config: AutomationRecordingConfig,
    recording_session: Option<Session>,
    manual_value: f32,
}

impl<T: RecordingTarget> Recorder<T> {
    pub fn new(target: T) -> Self {
        let (min, max, default) = target.default_range();
        let envelope = AutomationEnvelope::new(target).with_range(min, max);

        Self {
            envelope: Arc::new(RwLock::new(envelope)),
            state: AutomationState::Off,
            config: AutomationRecordingConfig::default(),
            recording_session: None,
            manual_value: default,
        }
    }

    pub fn with_envelope(envelope: AutomationEnvelope<T>) -> Self {
        let manual_value = envelope.get_value_at(0.0).unwrap_or(0.5);
        Self {
            envelope: Arc::new(RwLock::new(envelope)),
            state: AutomationState::Off,
            config: AutomationRecordingConfig::default(),
            recording_session: None,
            manual_value,
        }
    }

    pub fn envelope(&self) -> Arc<RwLock<AutomationEnvelope<T>>> {
        Arc::clone(&self.envelope)
    }

    pub fn state(&self) -> AutomationState {
        self.state
    }

    pub fn set_state(&mut self, state: AutomationState) {
        if self.state != state {
            if self.state.can_record() && !state.can_record() {
                self.stop_recording();
            }
            self.state = state;
        }
    }

    pub fn config(&self) -> &AutomationRecordingConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut AutomationRecordingConfig {
        &mut self.config
    }

    pub fn set_config(&mut self, config: AutomationRecordingConfig) {
        self.config = config;
    }

    pub fn manual_value(&self) -> f32 {
        self.manual_value
    }

    pub fn set_manual_value(&mut self, value: f32) {
        self.manual_value = value;
    }

    pub fn get_value_at(&self, beat: f64) -> f32 {
        match self.state {
            AutomationState::Off => self.manual_value,
            AutomationState::Write => self
                .recording_session
                .as_ref()
                .map_or(self.manual_value, |s| s.last_value),
            AutomationState::Play | AutomationState::Touch | AutomationState::Latch => {
                if self.state == AutomationState::Latch {
                    if let Some(ref session) = self.recording_session {
                        if !session.is_touching {
                            return session.last_value;
                        }
                    }
                }

                self.envelope
                    .read()
                    .get_value_at(beat)
                    .unwrap_or(self.manual_value)
            }
        }
    }

    pub fn touch(&mut self, beat: f64, value: f32) {
        if !self.state.starts_on_touch() {
            return;
        }

        self.recording_session = Some(Session {
            last_recorded_beat: beat,
            last_value: value,
            is_touching: true,
        });

        self.record_point(beat, value);
    }

    pub fn record(&mut self, beat: f64, value: f32) {
        if !self.state.can_record() {
            return;
        }

        if self.state == AutomationState::Write {
            if self.recording_session.is_none() {
                self.recording_session = Some(Session {
                    last_recorded_beat: beat,
                    last_value: value,
                    is_touching: true,
                });
            }
        } else if let Some(ref session) = self.recording_session {
            if !session.is_touching {
                return;
            }
        } else {
            return;
        }

        if let Some(ref session) = self.recording_session {
            let interval = beat - session.last_recorded_beat;
            if interval < self.config.min_point_interval && interval > 0.0 {
                return;
            }
        }

        self.record_point(beat, value);

        if let Some(ref mut session) = self.recording_session {
            session.last_recorded_beat = beat;
            session.last_value = value;
        }
    }

    pub fn release(&mut self, beat: f64, value: f32) {
        if let Some(ref mut session) = self.recording_session {
            session.is_touching = false;
            session.last_value = value;

            if self.state.stops_on_release() {
                self.record_point(beat, value);
                self.stop_recording();
            }
        }
    }

    fn stop_recording(&mut self) {
        if self.recording_session.take().is_some() && self.config.auto_simplify {
            self.envelope
                .write()
                .simplify(self.config.simplify_tolerance);
        }
    }

    fn record_point(&self, beat: f64, value: f32) {
        self.envelope.write().add_point(AutomationPoint::with_curve(
            beat,
            value,
            self.config.default_curve,
        ));
    }

    pub fn add_point(&self, point: AutomationPoint) {
        self.envelope.write().add_point(point);
    }

    pub fn remove_point_at(&self, beat: f64) {
        self.envelope.write().remove_point_at(beat);
    }

    pub fn clear(&self) {
        self.envelope.write().clear();
    }

    pub fn len(&self) -> usize {
        self.envelope.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.envelope.read().is_empty()
    }

    pub fn simplify(&self, tolerance: f32) {
        self.envelope.write().simplify(tolerance);
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.envelope.write().enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.envelope.read().enabled
    }
}

impl<T: RecordingTarget> Clone for Recorder<T> {
    fn clone(&self) -> Self {
        Self {
            envelope: Arc::new(RwLock::new(self.envelope.read().clone())),
            state: self.state,
            config: self.config,
            recording_session: self.recording_session.clone(),
            manual_value: self.manual_value,
        }
    }
}

impl<T: RecordingTarget + Serialize> Serialize for Recorder<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct LaneData<'a, U: Serialize> {
            envelope: &'a AutomationEnvelope<U>,
            config: &'a AutomationRecordingConfig,
            manual_value: f32,
        }

        let envelope = self.envelope.read();
        let data = LaneData {
            envelope: &envelope,
            config: &self.config,
            manual_value: self.manual_value,
        };
        data.serialize(serializer)
    }
}

impl<'de, T: RecordingTarget + Deserialize<'de>> Deserialize<'de> for Recorder<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct LaneData<U> {
            envelope: AutomationEnvelope<U>,
            config: AutomationRecordingConfig,
            manual_value: f32,
        }

        let data = LaneData::<T>::deserialize(deserializer)?;
        Ok(Self {
            envelope: Arc::new(RwLock::new(data.envelope)),
            state: AutomationState::Off,
            config: data.config,
            recording_session: None,
            manual_value: data.manual_value,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_impl::automation_target::AutomationTarget;

    #[test]
    fn test_new_lane() {
        let lane = Recorder::new(AutomationTarget::MasterVolume);
        assert_eq!(lane.state(), AutomationState::Off);
        assert!(lane.is_empty());
        assert!((lane.manual_value() - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_manual_value_when_off() {
        let mut lane = Recorder::new(AutomationTarget::MasterVolume);
        lane.set_manual_value(0.5);
        assert!((lane.get_value_at(0.0) - 0.5).abs() < 0.001);
        assert!((lane.get_value_at(100.0) - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_playback_mode() {
        let mut lane = Recorder::new(AutomationTarget::MasterVolume);
        lane.add_point(AutomationPoint::new(0.0, 0.0));
        lane.add_point(AutomationPoint::new(4.0, 1.0));

        lane.set_manual_value(0.5);
        assert!((lane.get_value_at(2.0) - 0.5).abs() < 0.001);

        lane.set_state(AutomationState::Play);
        assert!((lane.get_value_at(2.0) - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_touch_recording() {
        let mut lane = Recorder::new(AutomationTarget::MasterVolume);
        lane.config_mut().auto_simplify = false;
        lane.set_state(AutomationState::Touch);

        lane.touch(0.0, 0.5);
        assert_eq!(lane.len(), 1);

        lane.record(1.0, 0.6);
        lane.record(2.0, 0.7);
        assert_eq!(lane.len(), 3);

        lane.release(3.0, 0.8);
        assert_eq!(lane.len(), 4);
    }

    #[test]
    fn test_latch_continuation() {
        let mut lane = Recorder::new(AutomationTarget::MasterVolume);
        lane.add_point(AutomationPoint::new(0.0, 0.0));
        lane.add_point(AutomationPoint::new(10.0, 1.0));

        lane.set_state(AutomationState::Latch);
        lane.touch(2.0, 0.5);
        lane.record(3.0, 0.6);
        lane.release(4.0, 0.7);

        assert!((lane.get_value_at(5.0) - 0.7).abs() < 0.001);
    }

    #[test]
    fn test_write_mode() {
        let mut lane = Recorder::new(AutomationTarget::MasterVolume);
        lane.set_state(AutomationState::Write);

        lane.record(0.0, 0.1);
        lane.record(1.0, 0.2);
        lane.record(2.0, 0.3);

        assert!(lane.len() >= 3);
    }

    #[test]
    fn test_minimum_interval() {
        let mut lane = Recorder::new(AutomationTarget::MasterVolume);

        let config = AutomationRecordingConfig {
            min_point_interval: 0.5,
            auto_simplify: false,
            ..Default::default()
        };
        lane.set_config(config);
        lane.set_state(AutomationState::Write);

        lane.record(0.0, 0.5);
        lane.record(0.1, 0.6);
        lane.record(0.4, 0.7);
        lane.record(0.5, 0.8);

        assert_eq!(lane.len(), 2);
    }

    #[test]
    fn test_serialization() {
        let lane = Recorder::new(AutomationTarget::MasterVolume);
        lane.add_point(AutomationPoint::new(0.0, 0.0));
        lane.add_point(AutomationPoint::with_curve(4.0, 1.0, CurveType::SCurve));

        let json = serde_json::to_string(&lane).unwrap();
        let restored: Recorder<AutomationTarget> = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.len(), 2);
        assert_eq!(restored.state(), AutomationState::Off);
    }
}
