//! External synchronization support for transport.
//!
//! Supports syncing to external time sources:
//! - MIDI Time Code (MTC) - SMPTE timecode over MIDI
//! - MIDI Clock - 24 PPQN beat clock
//! - Linear Timecode (LTC) - Audio-embedded SMPTE timecode

use crate::compat::Ordering;
use crate::{AtomicBool, AtomicF64, AtomicU8};

/// External sync source type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum SyncSource {
    /// Internal clock (default) - transport runs independently
    #[default]
    Internal = 0,
    /// MIDI Time Code - follows SMPTE timecode from external device
    MidiTimecode = 1,
    /// MIDI Clock - follows 24 PPQN beat clock from external device
    MidiClock = 2,
    /// Linear Timecode - follows audio-embedded SMPTE timecode
    Ltc = 3,
}

impl SyncSource {
    fn from_u8(val: u8) -> Self {
        debug_assert!(val <= 3, "invalid SyncSource discriminant: {val}");
        match val {
            1 => Self::MidiTimecode,
            2 => Self::MidiClock,
            3 => Self::Ltc,
            _ => Self::Internal,
        }
    }
}

/// Sync lock status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum SyncStatus {
    /// Not synced to external source
    #[default]
    Unlocked = 0,
    /// Attempting to lock to external source
    Locking = 1,
    /// Locked and following external source
    Locked = 2,
    /// Locked but drifting (losing sync)
    Drifting = 3,
}

impl SyncStatus {
    fn from_u8(val: u8) -> Self {
        debug_assert!(val <= 3, "invalid SyncStatus discriminant: {val}");
        match val {
            1 => Self::Locking,
            2 => Self::Locked,
            3 => Self::Drifting,
            _ => Self::Unlocked,
        }
    }
}

pub use tutti_midi_types::sync::SmpteFrameRate;

/// External sync state - lock-free for RT access.
#[repr(align(64))]
pub struct SyncState {
    source: AtomicU8,
    status: AtomicU8,
    external_position_beats: AtomicF64,
    external_tempo: AtomicF64,
    offset_samples: AtomicF64,
    following: AtomicBool,
    smpte_frame_rate: AtomicU8,
}

impl Default for SyncState {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncState {
    pub fn new() -> Self {
        Self {
            source: AtomicU8::new(SyncSource::Internal as u8),
            status: AtomicU8::new(SyncStatus::Unlocked as u8),
            external_position_beats: AtomicF64::new(0.0),
            external_tempo: AtomicF64::new(120.0),
            offset_samples: AtomicF64::new(0.0),
            following: AtomicBool::new(false),
            smpte_frame_rate: AtomicU8::new(SmpteFrameRate::Fps2997Df as u8),
        }
    }

    pub fn source(&self) -> SyncSource {
        SyncSource::from_u8(self.source.load(Ordering::Acquire))
    }

    pub fn set_source(&self, source: SyncSource) {
        self.source.store(source as u8, Ordering::Release);
        if source == SyncSource::Internal {
            self.status
                .store(SyncStatus::Unlocked as u8, Ordering::Release);
            self.following.store(false, Ordering::Release);
        } else {
            self.status
                .store(SyncStatus::Locking as u8, Ordering::Release);
        }
    }

    pub fn is_internal(&self) -> bool {
        self.source() == SyncSource::Internal
    }

    pub fn is_external(&self) -> bool {
        !self.is_internal()
    }

    pub fn status(&self) -> SyncStatus {
        SyncStatus::from_u8(self.status.load(Ordering::Acquire))
    }

    pub fn set_status(&self, status: SyncStatus) {
        self.status.store(status as u8, Ordering::Release);
    }

    pub fn is_locked(&self) -> bool {
        self.status() == SyncStatus::Locked
    }

    pub fn external_position(&self) -> f64 {
        self.external_position_beats.load(Ordering::Acquire)
    }

    pub fn set_external_position(&self, beats: f64) {
        self.external_position_beats.store(beats, Ordering::Release);
    }

    pub fn external_tempo(&self) -> f64 {
        self.external_tempo.load(Ordering::Acquire)
    }

    pub fn set_external_tempo(&self, bpm: f64) {
        self.external_tempo
            .store(bpm.clamp(20.0, 300.0), Ordering::Release);
    }

    pub fn offset_samples(&self) -> f64 {
        self.offset_samples.load(Ordering::Acquire)
    }

    /// Positive = delay internal, negative = advance internal.
    pub fn set_offset_samples(&self, samples: f64) {
        self.offset_samples.store(samples, Ordering::Release);
    }

    pub fn is_following(&self) -> bool {
        self.following.load(Ordering::Acquire)
    }

    pub fn set_following(&self, follow: bool) {
        self.following.store(follow, Ordering::Release);
    }

    pub fn smpte_frame_rate(&self) -> SmpteFrameRate {
        SmpteFrameRate::from_u8(self.smpte_frame_rate.load(Ordering::Acquire))
    }

    pub fn set_smpte_frame_rate(&self, rate: SmpteFrameRate) {
        self.smpte_frame_rate.store(rate as u8, Ordering::Release);
    }

    pub fn snapshot(&self) -> SyncSnapshot {
        SyncSnapshot {
            source: self.source(),
            status: self.status(),
            external_position: self.external_position(),
            external_tempo: self.external_tempo(),
            offset_samples: self.offset_samples(),
            following: self.is_following(),
            smpte_frame_rate: self.smpte_frame_rate(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SyncSnapshot {
    pub source: SyncSource,
    pub status: SyncStatus,
    pub external_position: f64,
    pub external_tempo: f64,
    pub offset_samples: f64,
    pub following: bool,
    pub smpte_frame_rate: SmpteFrameRate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_state_default() {
        let state = SyncState::new();
        assert_eq!(state.source(), SyncSource::Internal);
        assert_eq!(state.status(), SyncStatus::Unlocked);
        assert!(!state.is_following());
        assert!(state.is_internal());
    }

    #[test]
    fn test_set_sync_source() {
        let state = SyncState::new();

        state.set_source(SyncSource::MidiTimecode);
        assert_eq!(state.source(), SyncSource::MidiTimecode);
        assert_eq!(state.status(), SyncStatus::Locking);
        assert!(state.is_external());

        state.set_source(SyncSource::Internal);
        assert_eq!(state.source(), SyncSource::Internal);
        assert_eq!(state.status(), SyncStatus::Unlocked);
    }

    #[test]
    fn test_external_position() {
        let state = SyncState::new();
        state.set_external_position(32.5);
        assert!((state.external_position() - 32.5).abs() < 0.001);
    }

    #[test]
    fn test_external_tempo() {
        let state = SyncState::new();
        state.set_external_tempo(140.0);
        assert!((state.external_tempo() - 140.0).abs() < 0.001);

        // Test clamping
        state.set_external_tempo(10.0);
        assert!((state.external_tempo() - 20.0).abs() < 0.001);

        state.set_external_tempo(400.0);
        assert!((state.external_tempo() - 300.0).abs() < 0.001);
    }

    #[test]
    fn test_smpte_frame_rate() {
        assert!((SmpteFrameRate::Fps24.fps() - 24.0).abs() < 0.001);
        assert!((SmpteFrameRate::Fps25.fps() - 25.0).abs() < 0.001);
        assert!((SmpteFrameRate::Fps30.fps() - 30.0).abs() < 0.001);
        assert!(SmpteFrameRate::Fps2997Df.is_drop_frame());
        assert!(!SmpteFrameRate::Fps2997Ndf.is_drop_frame());
    }

    #[test]
    fn test_snapshot() {
        let state = SyncState::new();
        state.set_source(SyncSource::MidiClock);
        state.set_external_position(16.0);
        state.set_external_tempo(128.0);
        state.set_following(true);

        let snap = state.snapshot();
        assert_eq!(snap.source, SyncSource::MidiClock);
        assert!((snap.external_position - 16.0).abs() < 0.001);
        assert!((snap.external_tempo - 128.0).abs() < 0.001);
        assert!(snap.following);
    }
}
