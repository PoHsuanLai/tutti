//! Automation *recording* — the write/touch/latch capture side, companion to
//! the playback-side [`AutomationLane`](crate::automation::AutomationLane).
//!
//! - [`RecordingTarget`] — trait downstream crates implement for their target
//!   enum; [`AutomationTarget`] is a ready-made default schema.
//! - [`Recorder`] — one automation lane's recording state machine (off / play /
//!   write / touch / latch) over an `audio_automation::AutomationEnvelope`.
//! - [`Manager`] — concurrent map of `target → Recorder`, the per-take
//!   automation recorder a host drives during recording.
//!
//! Both halves of automation (playback lanes + recording) now live in this
//! module, sharing the `audio_automation` envelope primitives.

use audio_automation::{AutomationEnvelope, AutomationPoint, AutomationState, CurveType};
use dashmap::DashMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ───────────────────────────── target ──────────────────────────────

/// Trait for types that can serve as automation recording targets.
///
/// Downstream crates implement this for their own target enums so
/// `Recorder<T>` and `Manager<T>` work with any target schema.
pub trait RecordingTarget: Clone + Send + Sync + 'static {
    /// `(min, max, default)` value range for this target.
    fn default_range(&self) -> (f32, f32, f32);
}

/// A ready-made automation-target schema (node params, master, tempo, custom).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AutomationTarget {
    NodeParam {
        node_id: u64,
        param_index: usize,
        param_name: Option<String>,
    },
    MasterVolume,
    MasterPan,
    Tempo,
    Custom(String),
}

impl AutomationTarget {
    pub fn node_param(node_id: u64, param_index: usize) -> Self {
        Self::NodeParam {
            node_id,
            param_index,
            param_name: None,
        }
    }

    pub fn node_param_named(node_id: u64, param_index: usize, name: impl Into<String>) -> Self {
        Self::NodeParam {
            node_id,
            param_index,
            param_name: Some(name.into()),
        }
    }

    pub fn custom(id: impl Into<String>) -> Self {
        Self::Custom(id.into())
    }

    pub fn key(&self) -> String {
        match self {
            Self::NodeParam {
                node_id,
                param_index,
                ..
            } => format!("node:{node_id}:{param_index}"),
            Self::MasterVolume => "master:volume".to_string(),
            Self::MasterPan => "master:pan".to_string(),
            Self::Tempo => "transport:tempo".to_string(),
            Self::Custom(id) => format!("custom:{id}"),
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::NodeParam {
                node_id,
                param_index,
                param_name,
            } => {
                if let Some(name) = param_name {
                    format!("Node {node_id}: {name}")
                } else {
                    format!("Node {node_id}: Param {param_index}")
                }
            }
            Self::MasterVolume => "Master Volume".to_string(),
            Self::MasterPan => "Master Pan".to_string(),
            Self::Tempo => "Tempo".to_string(),
            Self::Custom(id) => id.clone(),
        }
    }

    pub fn is_node_param(&self) -> bool {
        matches!(self, Self::NodeParam { .. })
    }

    pub fn is_master(&self) -> bool {
        matches!(self, Self::MasterVolume | Self::MasterPan)
    }

    pub fn is_tempo(&self) -> bool {
        matches!(self, Self::Tempo)
    }

    pub fn node_id(&self) -> Option<u64> {
        match self {
            Self::NodeParam { node_id, .. } => Some(*node_id),
            _ => None,
        }
    }
}

impl RecordingTarget for AutomationTarget {
    fn default_range(&self) -> (f32, f32, f32) {
        match self {
            Self::MasterVolume => (0.0, 2.0, 1.0),
            Self::MasterPan => (-1.0, 1.0, 0.0),
            Self::Tempo => (20.0, 300.0, 120.0),
            Self::NodeParam { .. } | Self::Custom(_) => (0.0, 1.0, 0.5),
        }
    }
}

impl fmt::Display for AutomationTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

impl Default for AutomationTarget {
    fn default() -> Self {
        Self::Custom("default".to_string())
    }
}

// ──────────────────────────── recorder ─────────────────────────────

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

/// One automation lane's recording state machine over an envelope.
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

// ──────────────────────────── manager ──────────────────────────────

/// Concurrent map of `target → Recorder`. The per-take automation recorder a
/// host drives while recording automation moves.
#[derive(Debug)]
pub struct Manager<T: RecordingTarget + Eq + Hash> {
    lanes: DashMap<T, Recorder<T>>,
    enabled: AtomicBool,
    default_config: AutomationRecordingConfig,
}

impl<T: RecordingTarget + Eq + Hash> Manager<T> {
    pub fn new() -> Self {
        Self {
            lanes: DashMap::new(),
            enabled: AtomicBool::new(true),
            default_config: AutomationRecordingConfig::default(),
        }
    }

    pub fn with_config(config: AutomationRecordingConfig) -> Self {
        Self {
            lanes: DashMap::new(),
            enabled: AtomicBool::new(true),
            default_config: config,
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_default_config(&mut self, config: AutomationRecordingConfig) {
        self.default_config = config;
    }

    pub fn get_or_create_lane(
        &self,
        target: T,
    ) -> dashmap::mapref::one::RefMut<'_, T, Recorder<T>> {
        self.lanes.entry(target.clone()).or_insert_with(|| {
            let mut lane = Recorder::new(target);
            lane.set_config(self.default_config);
            lane
        })
    }

    pub fn get_lane(&self, target: &T) -> Option<dashmap::mapref::one::Ref<'_, T, Recorder<T>>> {
        self.lanes.get(target)
    }

    pub fn get_lane_mut(
        &self,
        target: &T,
    ) -> Option<dashmap::mapref::one::RefMut<'_, T, Recorder<T>>> {
        self.lanes.get_mut(target)
    }

    pub fn create_lane(
        &self,
        target: T,
        config: AutomationRecordingConfig,
    ) -> dashmap::mapref::one::RefMut<'_, T, Recorder<T>> {
        self.lanes.entry(target.clone()).or_insert_with(|| {
            let mut lane = Recorder::new(target);
            lane.set_config(config);
            lane
        })
    }

    pub fn create_lane_with_envelope(&self, envelope: AutomationEnvelope<T>) {
        let target = envelope.target.clone();
        let lane = Recorder::with_envelope(envelope);
        self.lanes.insert(target, lane);
    }

    pub fn remove_lane(&self, target: &T) -> Option<(T, Recorder<T>)> {
        self.lanes.remove(target)
    }

    pub fn has_lane(&self, target: &T) -> bool {
        self.lanes.contains_key(target)
    }

    pub fn lane_count(&self) -> usize {
        self.lanes.len()
    }

    pub fn targets(&self) -> Vec<T> {
        self.lanes.iter().map(|r| r.key().clone()).collect()
    }

    pub fn clear(&self) {
        self.lanes.clear();
    }

    #[inline]
    pub fn get_value(&self, target: &T, beat: f64) -> Option<f32> {
        if !self.is_enabled() {
            return None;
        }
        self.lanes.get(target).map(|lane| lane.get_value_at(beat))
    }

    pub fn get_values(&self, targets: &[T], beat: f64) -> Vec<Option<f32>> {
        if !self.is_enabled() {
            return vec![None; targets.len()];
        }
        targets
            .iter()
            .map(|target| self.lanes.get(target).map(|lane| lane.get_value_at(beat)))
            .collect()
    }

    pub fn set_state(&self, target: &T, state: AutomationState) {
        if let Some(mut lane) = self.lanes.get_mut(target) {
            lane.set_state(state);
        }
    }

    pub fn get_state(&self, target: &T) -> Option<AutomationState> {
        self.lanes.get(target).map(|lane| lane.state())
    }

    pub fn set_all_states(&self, state: AutomationState) {
        for mut lane_ref in self.lanes.iter_mut() {
            lane_ref.set_state(state);
        }
    }

    pub fn touch(&self, target: &T, beat: f64, value: f32) {
        if let Some(mut lane) = self.lanes.get_mut(target) {
            lane.touch(beat, value);
        }
    }

    pub fn record(&self, target: &T, beat: f64, value: f32) {
        if let Some(mut lane) = self.lanes.get_mut(target) {
            lane.record(beat, value);
        }
    }

    pub fn release(&self, target: &T, beat: f64, value: f32) {
        if let Some(mut lane) = self.lanes.get_mut(target) {
            lane.release(beat, value);
        }
    }

    pub fn record_batch(&self, beat: f64, values: &[(T, f32)]) {
        for (target, value) in values {
            self.record(target, beat, *value);
        }
    }

    pub fn add_point(&self, target: &T, point: AutomationPoint) {
        if let Some(lane) = self.lanes.get(target) {
            lane.add_point(point);
        }
    }

    pub fn remove_point_at(&self, target: &T, beat: f64) {
        if let Some(lane) = self.lanes.get(target) {
            lane.remove_point_at(beat);
        }
    }

    pub fn clear_lane(&self, target: &T) {
        if let Some(lane) = self.lanes.get(target) {
            lane.clear();
        }
    }

    pub fn simplify_lane(&self, target: &T, tolerance: f32) {
        if let Some(lane) = self.lanes.get(target) {
            lane.simplify(tolerance);
        }
    }

    pub fn simplify_all(&self, tolerance: f32) {
        for lane_ref in self.lanes.iter() {
            lane_ref.simplify(tolerance);
        }
    }

    pub fn snapshot(&self) -> AutomationSnapshot<T> {
        let lanes: Vec<_> = self
            .lanes
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect();

        AutomationSnapshot {
            lanes,
            enabled: self.is_enabled(),
        }
    }

    pub fn restore(&self, snapshot: &AutomationSnapshot<T>) {
        self.lanes.clear();
        for (target, lane) in &snapshot.lanes {
            self.lanes.insert(target.clone(), lane.clone());
        }
        self.set_enabled(snapshot.enabled);
    }
}

impl<T: RecordingTarget + Eq + Hash> Default for Manager<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: RecordingTarget + Eq + Hash> Clone for Manager<T> {
    fn clone(&self) -> Self {
        let new_manager = Self::new();
        for lane_ref in self.lanes.iter() {
            new_manager
                .lanes
                .insert(lane_ref.key().clone(), lane_ref.value().clone());
        }
        new_manager.set_enabled(self.is_enabled());
        new_manager
    }
}

#[derive(Debug, Clone)]
pub struct AutomationSnapshot<T: RecordingTarget + Eq + Hash> {
    pub lanes: Vec<(T, Recorder<T>)>,
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // ── target ──

    #[test]
    fn test_node_param_named() {
        let target = AutomationTarget::node_param_named(42, 0, "cutoff");
        assert_eq!(target.display_name(), "Node 42: cutoff");
    }

    #[test]
    fn test_master_targets() {
        assert!(AutomationTarget::MasterVolume.is_master());
        assert!(AutomationTarget::MasterPan.is_master());
        assert!(!AutomationTarget::Tempo.is_master());
    }

    #[test]
    fn test_unique_keys() {
        let targets = [
            AutomationTarget::node_param(0, 0),
            AutomationTarget::node_param(0, 1),
            AutomationTarget::node_param(1, 0),
            AutomationTarget::MasterVolume,
            AutomationTarget::MasterPan,
            AutomationTarget::Tempo,
            AutomationTarget::custom("my_target"),
        ];

        let keys: std::collections::HashSet<_> = targets.iter().map(|t| t.key()).collect();
        assert_eq!(keys.len(), targets.len(), "All keys should be unique");
    }

    #[test]
    fn test_target_serialization() {
        let target = AutomationTarget::node_param_named(42, 0, "cutoff");
        let json = serde_json::to_string(&target).unwrap();
        let deserialized: AutomationTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(target, deserialized);
    }

    // ── recorder ──

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
    fn test_recorder_serialization() {
        let lane = Recorder::new(AutomationTarget::MasterVolume);
        lane.add_point(AutomationPoint::new(0.0, 0.0));
        lane.add_point(AutomationPoint::with_curve(4.0, 1.0, CurveType::SCurve));

        let json = serde_json::to_string(&lane).unwrap();
        let restored: Recorder<AutomationTarget> = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.len(), 2);
        assert_eq!(restored.state(), AutomationState::Off);
    }

    // ── manager ──

    #[test]
    fn test_create_manager() {
        let manager = Manager::<AutomationTarget>::new();
        assert!(manager.is_enabled());
        assert_eq!(manager.lane_count(), 0);
    }

    #[test]
    fn test_get_or_create_lane() {
        let manager = Manager::new();
        let target = AutomationTarget::MasterVolume;

        {
            let _lane = manager.get_or_create_lane(target.clone());
        }

        assert!(manager.has_lane(&target));
        assert_eq!(manager.lane_count(), 1);
    }

    #[test]
    fn test_get_value() {
        let manager = Manager::new();
        let target = AutomationTarget::MasterVolume;

        {
            let lane = manager.get_or_create_lane(target.clone());
            lane.add_point(AutomationPoint::new(0.0, 0.0));
            lane.add_point(AutomationPoint::new(4.0, 1.0));
            drop(lane);
        }

        manager.set_state(&target, AutomationState::Play);

        let val = manager.get_value(&target, 2.0);
        assert!(val.is_some());
        assert!((val.unwrap() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_global_disable() {
        let manager = Manager::new();
        let target = AutomationTarget::MasterVolume;

        {
            let lane = manager.get_or_create_lane(target.clone());
            lane.add_point(AutomationPoint::new(0.0, 0.5));
            drop(lane);
        }

        manager.set_state(&target, AutomationState::Play);
        assert!(manager.get_value(&target, 0.0).is_some());

        manager.set_enabled(false);
        assert!(manager.get_value(&target, 0.0).is_none());
    }

    #[test]
    fn test_recording() {
        let manager = Manager::new();
        let target = AutomationTarget::MasterVolume;

        {
            let mut lane = manager.get_or_create_lane(target.clone());
            lane.set_state(AutomationState::Write);
        }

        manager.record(&target, 0.0, 0.1);
        manager.record(&target, 1.0, 0.2);
        manager.record(&target, 2.0, 0.3);

        let lane = manager.get_lane(&target).unwrap();
        assert!(lane.len() >= 3);
    }

    #[test]
    fn test_snapshot_restore() {
        let manager = Manager::new();
        let target = AutomationTarget::MasterVolume;

        {
            let lane = manager.get_or_create_lane(target.clone());
            lane.add_point(AutomationPoint::new(0.0, 0.5));
            lane.add_point(AutomationPoint::new(4.0, 1.0));
        }

        let snapshot = manager.snapshot();

        manager.clear();
        assert_eq!(manager.lane_count(), 0);

        manager.restore(&snapshot);
        assert_eq!(manager.lane_count(), 1);
        assert!(manager.has_lane(&target));
    }

    #[test]
    fn test_set_all_states() {
        let manager = Manager::new();

        manager.get_or_create_lane(AutomationTarget::MasterVolume);
        manager.get_or_create_lane(AutomationTarget::MasterPan);
        manager.get_or_create_lane(AutomationTarget::Tempo);

        manager.set_all_states(AutomationState::Play);

        for target in manager.targets() {
            assert_eq!(manager.get_state(&target), Some(AutomationState::Play));
        }
    }

    #[test]
    fn test_concurrent_access() {
        use std::thread;

        let manager = Arc::new(Manager::new());
        let target = AutomationTarget::MasterVolume;

        manager.get_or_create_lane(target.clone());
        manager.set_state(&target, AutomationState::Write);

        let manager1 = Arc::clone(&manager);
        let target1 = target.clone();
        let t1 = thread::spawn(move || {
            for i in 0..100 {
                manager1.record(&target1, i as f64 * 0.01, i as f32 * 0.01);
            }
        });

        let manager2 = Arc::clone(&manager);
        let target2 = target.clone();
        let t2 = thread::spawn(move || {
            for i in 0..100 {
                let _ = manager2.get_value(&target2, i as f64 * 0.01);
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();

        let lane = manager.get_lane(&target).unwrap();
        assert!(!lane.is_empty());
    }
}
