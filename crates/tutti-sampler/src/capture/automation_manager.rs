use super::automation_recorder::{AutomationRecordingConfig, Recorder};
use super::automation_target::RecordingTarget;
use audio_automation::{AutomationEnvelope, AutomationPoint, AutomationState};
use dashmap::DashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};

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

    pub fn get_lane(
        &self,
        target: &T,
    ) -> Option<dashmap::mapref::one::Ref<'_, T, Recorder<T>>> {
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
    use crate::capture_impl::automation_target::AutomationTarget;
    use std::sync::Arc;

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
