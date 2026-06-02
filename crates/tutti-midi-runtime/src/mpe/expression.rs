use atomic_float::AtomicF32;
use std::sync::atomic::{AtomicBool, Ordering};

/// Lock-free per-note expression state. All methods are safe to call from the audio thread.
pub struct PerNoteExpression {
    pitch_bend: [AtomicF32; 128],
    pressure: [AtomicF32; 128],
    slide: [AtomicF32; 128],
    active: [AtomicBool; 128],
    global_pitch_bend: AtomicF32,
    global_pressure: AtomicF32,
}

impl Default for PerNoteExpression {
    fn default() -> Self {
        Self::new()
    }
}

impl PerNoteExpression {
    pub fn new() -> Self {
        Self {
            pitch_bend: core::array::from_fn(|_| AtomicF32::new(0.0)),
            pressure: core::array::from_fn(|_| AtomicF32::new(0.0)),
            slide: core::array::from_fn(|_| AtomicF32::new(0.5)), // CC74 default is center
            active: core::array::from_fn(|_| AtomicBool::new(false)),
            global_pitch_bend: AtomicF32::new(0.0),
            global_pressure: AtomicF32::new(0.0),
        }
    }

    /// Resets per-note pitch bend, pressure, and slide to defaults.
    #[inline]
    pub fn note_on(&self, note: u8) {
        if note < 128 {
            self.pitch_bend[note as usize].store(0.0, Ordering::Release);
            self.pressure[note as usize].store(0.0, Ordering::Release);
            self.slide[note as usize].store(0.5, Ordering::Release);
            self.active[note as usize].store(true, Ordering::Release);
        }
    }

    #[inline]
    pub fn note_off(&self, note: u8) {
        if note < 128 {
            self.active[note as usize].store(false, Ordering::Release);
        }
    }

    /// `value`: -1.0 to 1.0
    #[inline]
    pub fn set_pitch_bend(&self, note: u8, value: f32) {
        if note < 128 {
            self.pitch_bend[note as usize].store(value.clamp(-1.0, 1.0), Ordering::Release);
        }
    }

    /// `value`: 0.0 to 1.0
    #[inline]
    pub fn set_pressure(&self, note: u8, value: f32) {
        if note < 128 {
            self.pressure[note as usize].store(value.clamp(0.0, 1.0), Ordering::Release);
        }
    }

    /// CC74 slide. `value`: 0.0 to 1.0
    #[inline]
    pub fn set_slide(&self, note: u8, value: f32) {
        if note < 128 {
            self.slide[note as usize].store(value.clamp(0.0, 1.0), Ordering::Release);
        }
    }

    /// `value`: -1.0 to 1.0, added to per-note bend.
    #[inline]
    pub fn set_global_pitch_bend(&self, value: f32) {
        self.global_pitch_bend
            .store(value.clamp(-1.0, 1.0), Ordering::Release);
    }

    /// `value`: 0.0 to 1.0, combined with per-note via max().
    #[inline]
    pub fn set_global_pressure(&self, value: f32) {
        self.global_pressure
            .store(value.clamp(0.0, 1.0), Ordering::Release);
    }

    /// Combined per-note + global pitch bend, clamped to -1.0..1.0.
    #[inline]
    pub fn get_pitch_bend(&self, note: u8) -> f32 {
        if note < 128 {
            let per_note = self.pitch_bend[note as usize].load(Ordering::Acquire);
            let global = self.global_pitch_bend.load(Ordering::Acquire);
            (per_note + global).clamp(-1.0, 1.0)
        } else {
            0.0
        }
    }

    #[inline]
    pub fn get_pitch_bend_per_note(&self, note: u8) -> f32 {
        if note < 128 {
            self.pitch_bend[note as usize].load(Ordering::Acquire)
        } else {
            0.0
        }
    }

    #[inline]
    pub fn get_pitch_bend_global(&self) -> f32 {
        self.global_pitch_bend.load(Ordering::Acquire)
    }

    /// Returns max(per-note, global) pressure.
    #[inline]
    pub fn get_pressure(&self, note: u8) -> f32 {
        if note < 128 {
            let per_note = self.pressure[note as usize].load(Ordering::Acquire);
            let global = self.global_pressure.load(Ordering::Acquire);
            per_note.max(global)
        } else {
            0.0
        }
    }

    #[inline]
    pub fn get_pressure_per_note(&self, note: u8) -> f32 {
        if note < 128 {
            self.pressure[note as usize].load(Ordering::Acquire)
        } else {
            0.0
        }
    }

    /// Returns 0.5 (CC74 center) for inactive/out-of-range notes.
    #[inline]
    pub fn get_slide(&self, note: u8) -> f32 {
        if note < 128 {
            self.slide[note as usize].load(Ordering::Acquire)
        } else {
            0.5
        }
    }

    #[inline]
    pub fn is_active(&self, note: u8) -> bool {
        if note < 128 {
            self.active[note as usize].load(Ordering::Acquire)
        } else {
            false
        }
    }

    pub fn reset(&self) {
        for i in 0..128 {
            self.pitch_bend[i].store(0.0, Ordering::Release);
            self.pressure[i].store(0.0, Ordering::Release);
            self.slide[i].store(0.5, Ordering::Release);
            self.active[i].store(false, Ordering::Release);
        }
        self.global_pitch_bend.store(0.0, Ordering::Release);
        self.global_pressure.store(0.0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_per_note_expression() {
        let expr = PerNoteExpression::new();

        expr.note_on(60);
        assert!(expr.is_active(60));

        expr.set_pitch_bend(60, 0.5);
        assert!((expr.get_pitch_bend(60) - 0.5).abs() < 0.001);

        expr.set_pressure(60, 0.75);
        assert!((expr.get_pressure(60) - 0.75).abs() < 0.001);

        expr.set_slide(60, 0.3);
        assert!((expr.get_slide(60) - 0.3).abs() < 0.001);

        expr.note_off(60);
        assert!(!expr.is_active(60));
    }

    #[test]
    fn test_global_expression() {
        let expr = PerNoteExpression::new();

        expr.note_on(60);
        expr.set_pitch_bend(60, 0.2);
        expr.set_global_pitch_bend(0.3);

        assert!((expr.get_pitch_bend(60) - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_expression_clamping() {
        let expr = PerNoteExpression::new();

        expr.set_pitch_bend(60, 2.0);
        assert!((expr.get_pitch_bend(60) - 1.0).abs() < 0.001);

        expr.set_pitch_bend(60, -2.0);
        assert!((expr.get_pitch_bend(60) - (-1.0)).abs() < 0.001);

        expr.set_pressure(60, 1.5);
        assert!((expr.get_pressure(60) - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_reset_clears_everything() {
        let expr = PerNoteExpression::new();

        expr.note_on(60);
        expr.note_on(72);
        expr.set_pitch_bend(60, 0.5);
        expr.set_pressure(72, 0.8);
        expr.set_slide(60, 0.9);
        expr.set_global_pitch_bend(0.3);
        expr.set_global_pressure(0.6);

        expr.reset();

        assert!(!expr.is_active(60));
        assert!(!expr.is_active(72));
        assert!((expr.get_pitch_bend_per_note(60)).abs() < 0.001);
        assert!((expr.get_pressure_per_note(72)).abs() < 0.001);
        assert!((expr.get_slide(60) - 0.5).abs() < 0.001);
        assert!((expr.get_pitch_bend_global()).abs() < 0.001);
        assert!((expr.get_pressure(60)).abs() < 0.001);
    }

    #[test]
    fn test_out_of_range_note_returns_defaults() {
        let expr = PerNoteExpression::new();

        assert!(!expr.is_active(128));
        assert!((expr.get_pitch_bend(128)).abs() < 0.001);
        assert!((expr.get_pressure(128)).abs() < 0.001);
        assert!((expr.get_slide(128) - 0.5).abs() < 0.001);
    }
}
