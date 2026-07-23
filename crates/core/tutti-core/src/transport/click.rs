//! Metronome click AudioUnit - generates click sounds synced to transport.
//!
//! The click node reads transport state via [`Timeline`] and settings
//! (volume, accent, mode) from [`ClickSettings`].

use super::Transport;
use crate::{AtomicF32, AtomicU32, AtomicU8, Ordering};
use fundsp::audionode::AudioNode;
use fundsp::prelude::*;
use std::sync::Arc;

/// Metronome operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum MetronomeMode {
    /// Metronome is disabled.
    #[default]
    Off,
    /// Only play during preroll count-in.
    PrerollOnly,
    /// Only play while recording.
    RecordingOnly,
    /// Always play when transport is running.
    Always,
}

impl From<u8> for MetronomeMode {
    fn from(value: u8) -> Self {
        debug_assert!(value <= 3, "invalid MetronomeMode discriminant: {value}");
        match value {
            1 => MetronomeMode::PrerollOnly,
            2 => MetronomeMode::RecordingOnly,
            3 => MetronomeMode::Always,
            _ => MetronomeMode::Off,
        }
    }
}

impl From<MetronomeMode> for u8 {
    #[inline]
    fn from(mode: MetronomeMode) -> u8 {
        mode as u8
    }
}

impl core::fmt::Display for MetronomeMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::PrerollOnly => "preroll only",
            Self::RecordingOnly => "recording only",
            Self::Always => "always",
        })
    }
}

/// Click-specific settings (volume, accent pattern, mode).
///
/// Transport state (beat, playing, recording, preroll) comes from `Timeline`.
#[repr(align(64))]
pub struct ClickSettings {
    volume: AtomicF32,
    accent_every: AtomicU32,
    mode: AtomicU8,
}

impl ClickSettings {
    pub fn new() -> Self {
        Self {
            volume: AtomicF32::new(0.5),
            accent_every: AtomicU32::new(4),
            mode: AtomicU8::new(MetronomeMode::Off as u8),
        }
    }

    pub fn set_volume(&self, volume: f32) {
        self.volume.store(volume.clamp(0.0, 1.0), Ordering::Release);
    }

    pub fn volume(&self) -> f32 {
        self.volume.load(Ordering::Acquire)
    }

    pub fn set_accent_every(&self, beats: u32) {
        self.accent_every.store(beats, Ordering::Release);
    }

    pub fn accent_every(&self) -> u32 {
        self.accent_every.load(Ordering::Acquire)
    }

    pub fn set_mode(&self, mode: MetronomeMode) {
        self.mode.store(mode.into(), Ordering::Release);
    }

    pub fn mode(&self) -> MetronomeMode {
        MetronomeMode::from(self.mode.load(Ordering::Acquire))
    }
}

impl Default for ClickSettings {
    fn default() -> Self {
        Self::new()
    }
}

pub type ClickState = ClickSettings;

/// Click generator AudioNode.
///
/// Outputs stereo click sounds synced to the transport beat.
///
/// Takes the live [`Transport`] concretely rather than a [`Timeline`]: the
/// metronome's modes depend on `recording` / `in_preroll`, which are
/// live-session facts an offline timeline has no answer for. It only ever
/// reads — no transport control.
///
/// A click node does end up inside offline-cloned nets (the clone copies every
/// node), but `Net::clone_isolated` repoints the output bus away from it, so its
/// samples go nowhere.
#[derive(Clone)]
pub struct ClickNode {
    transport: Transport,
    settings: Arc<ClickSettings>,
    sample_rate: f64,
    click_normal: Vec<f32>,
    click_accent: Vec<f32>,
    click_pos: usize,
    is_accent: bool,
    last_click_beat: i64,
}

impl ClickNode {
    pub fn with_transport(
        transport: Transport,
        settings: Arc<ClickSettings>,
        sample_rate: impl Into<crate::SampleRate>,
    ) -> Self {
        let sample_rate = sample_rate.into().get();
        let click_normal = Self::generate_click(sample_rate, false);
        let click_accent = Self::generate_click(sample_rate, true);

        Self {
            transport,
            settings,
            sample_rate,
            click_normal,
            click_accent,
            click_pos: 0,
            is_accent: false,
            last_click_beat: -1,
        }
    }

    fn generate_click(sample_rate: f64, is_accent: bool) -> Vec<f32> {
        let click_duration = 0.03; // 30ms
        let num_samples = (sample_rate * click_duration) as usize;

        let freq = if is_accent { 1200.0 } else { 1000.0 };
        let accent_volume = if is_accent { 1.0 } else { 0.7 };

        (0..num_samples)
            .map(|i| {
                let t = i as f64 / sample_rate;
                let env = if t < 0.001 {
                    t / 0.001
                } else if t < 0.02 {
                    1.0
                } else {
                    1.0 - (t - 0.02) / 0.01
                };
                let phase = 2.0 * core::f64::consts::PI * freq * t;
                (phase.sin() * env * accent_volume) as f32
            })
            .collect()
    }

    fn is_accent_beat(&self, beat: i64) -> bool {
        let accent_every = self.settings.accent_every();
        if accent_every == 0 {
            return false;
        }
        (beat as u32).is_multiple_of(accent_every)
    }
}

impl AudioNode for ClickNode {
    const ID: u64 = 0x436c69636b_u64; // "Click"

    type Inputs = U0;
    type Outputs = U2;

    #[inline]
    fn tick(&mut self, _input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        let mode = self.settings.mode();
        let is_playing = self.transport.motion.is_playing();
        let is_recording = self.transport.settings.is_recording();
        let is_in_preroll = self.transport.settings.is_in_preroll();

        let should_play = match mode {
            MetronomeMode::Off => false,
            MetronomeMode::Always => is_playing,
            MetronomeMode::PrerollOnly => is_in_preroll,
            MetronomeMode::RecordingOnly => is_recording && !is_in_preroll,
        };

        if !should_play {
            self.click_pos = 0;
            self.last_click_beat = -1;
            return [0.0, 0.0].into();
        }

        let current_beat = self.transport.settings.beat();
        let beat_int = current_beat.floor() as i64;

        // Trigger click on new beat. The `beat_int != self.last_click_beat` check
        // handles both forward advancement AND backward jumps from loop wrapping.
        if beat_int != self.last_click_beat {
            self.last_click_beat = beat_int;
            self.click_pos = 0;
            self.is_accent = self.is_accent_beat(beat_int);
        }

        let click_buffer = if self.is_accent {
            &self.click_accent
        } else {
            &self.click_normal
        };

        if self.click_pos < click_buffer.len() {
            let sample = click_buffer[self.click_pos] * self.settings.volume();
            self.click_pos += 1;
            [sample, sample].into()
        } else {
            [0.0, 0.0].into()
        }
    }

    fn reset(&mut self) {
        self.click_pos = 0;
        self.last_click_beat = -1;
    }

    fn set_sample_rate(&mut self, sample_rate: crate::params::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        if (self.sample_rate - sample_rate).abs() > 0.1 {
            self.sample_rate = sample_rate;
            self.click_normal = Self::generate_click(sample_rate, false);
            self.click_accent = Self::generate_click(sample_rate, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the real `Transport` rather than a mock: the click node needs
    /// recording/preroll, which only the live transport has, and a mock would
    /// just restate its fields.
    fn playing(t: &Transport) {
        let _ = t.motion.try_send(super::super::MotionEvent::Play);
        t.motion.drain();
    }

    fn make_click() -> (Transport, Arc<ClickSettings>, ClickNode) {
        let transport = Transport::new(44100.0);
        let settings = Arc::new(ClickSettings::new());
        let node = ClickNode::with_transport(transport.clone(), Arc::clone(&settings), 44100.0);
        (transport, settings, node)
    }

    #[test]
    fn test_click_node_creation() {
        let (_, _, node) = make_click();
        assert!(!node.click_normal.is_empty());
        assert!(!node.click_accent.is_empty());
    }

    #[test]
    fn test_click_node_silent_when_paused() {
        let (_, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::Always);

        // transport.playing is false by default
        let output = node.tick(&Frame::default());
        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
    }

    #[test]
    fn test_click_node_plays_on_beat() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);

        let mut found_nonzero = false;
        for _ in 0..100 {
            let output = node.tick(&Frame::default());
            if output[0] != 0.0 || output[1] != 0.0 {
                found_nonzero = true;
                break;
            }
        }
        assert!(found_nonzero, "Click should produce non-zero output");

        // Advance to beat 1
        transport.settings.set_beat(1.0);
        found_nonzero = false;
        for _ in 0..100 {
            let output = node.tick(&Frame::default());
            if output[0] != 0.0 || output[1] != 0.0 {
                found_nonzero = true;
                break;
            }
        }
        assert!(
            found_nonzero,
            "Click should produce non-zero output on new beat"
        );
    }

    #[test]
    fn test_click_plays_after_loop_wrap() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);

        // Advance to beat 7
        transport.settings.set_beat(7.0);
        let _ = node.tick(&Frame::default());
        assert_eq!(node.last_click_beat, 7);

        // Simulate loop wrap: beat jumps backward from 7 to 4
        transport.settings.set_beat(4.0);
        let mut found_nonzero = false;
        for _ in 0..100 {
            let output = node.tick(&Frame::default());
            if output[0] != 0.0 {
                found_nonzero = true;
                break;
            }
        }
        assert_eq!(
            node.last_click_beat, 4,
            "Should reset to beat 4 after loop wrap"
        );
        assert!(found_nonzero, "Click should play after loop wrap");
    }

    #[test]
    fn test_accent_pattern() {
        let (_, settings, node) = make_click();
        settings.set_accent_every(4);

        assert!(node.is_accent_beat(0));
        assert!(!node.is_accent_beat(1));
        assert!(node.is_accent_beat(4));
    }

    #[test]
    fn test_preroll_only_mode() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::PrerollOnly);
        settings.set_volume(1.0);

        // Should be silent when not in preroll
        let output = node.tick(&Frame::default());
        assert_eq!(output[0], 0.0);

        // Enable preroll - should play
        transport.settings.set_in_preroll(true);
        node.reset();
        let mut found_nonzero = false;
        for _ in 0..100 {
            let output = node.tick(&Frame::default());
            if output[0] != 0.0 {
                found_nonzero = true;
                break;
            }
        }
        assert!(found_nonzero, "Click should play during preroll");
    }

    #[test]
    fn test_recording_only_mode() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::RecordingOnly);
        settings.set_volume(1.0);

        // Should be silent when not recording
        let output = node.tick(&Frame::default());
        assert_eq!(output[0], 0.0);

        // Enable recording - should play
        transport.settings.set_recording(true);
        node.reset();
        let mut found_nonzero = false;
        for _ in 0..100 {
            let output = node.tick(&Frame::default());
            if output[0] != 0.0 {
                found_nonzero = true;
                break;
            }
        }
        assert!(found_nonzero, "Click should play during recording");

        // In preroll while recording - should NOT play
        transport.settings.set_in_preroll(true);
        node.reset();
        let output = node.tick(&Frame::default());
        assert_eq!(
            output[0], 0.0,
            "Click should not play during preroll in RecordingOnly mode"
        );
    }
}
