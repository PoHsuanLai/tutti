use atomic_float::AtomicF32;
use core::sync::atomic::{AtomicBool, Ordering};

use tutti_midi_types::NoteId;

use super::per_note_map::{AtomicPerNoteMap, AtomicSlot};

/// Maximum number of simultaneously-sounding notes tracked for expression.
const MAX_NOTES: usize = 128;

/// CC74 (slide / brightness) rests at centre when a note starts.
const SLIDE_DEFAULT: f32 = 0.5;

/// The per-note expression payload: three continuous dimensions plus a liveness
/// flag, all atomic so the store can be read on the audio thread.
pub struct PerNoteAtomics {
    pitch_bend: AtomicF32,
    pressure: AtomicF32,
    slide: AtomicF32,
    active: AtomicBool,
}

impl AtomicSlot for PerNoteAtomics {
    fn new_default() -> Self {
        Self {
            pitch_bend: AtomicF32::new(0.0),
            pressure: AtomicF32::new(0.0),
            slide: AtomicF32::new(SLIDE_DEFAULT),
            active: AtomicBool::new(false),
        }
    }

    fn clear(&self) {
        self.pitch_bend.store(0.0, Ordering::Release);
        self.pressure.store(0.0, Ordering::Release);
        self.slide.store(SLIDE_DEFAULT, Ordering::Release);
        self.active.store(false, Ordering::Release);
    }
}

/// Lock-free per-note expression state, keyed by [`NoteId`]. All methods are safe
/// to call from the audio thread.
///
/// Two live notes sharing a note number (same pitch on different channels, or a
/// rotation-minted id) occupy distinct slots, so their pitch bend / pressure /
/// slide stay independent — the point of MIDI 2.0 per-note addressing. `global_*`
/// values are channel-wide (MPE master channel) and are combined with the per-note
/// value on read.
pub struct PerNoteExpression {
    notes: AtomicPerNoteMap<PerNoteAtomics, MAX_NOTES>,
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
            notes: AtomicPerNoteMap::new(),
            global_pitch_bend: AtomicF32::new(0.0),
            global_pressure: AtomicF32::new(0.0),
        }
    }

    /// Claim a slot for `id` and reset its expression to defaults.
    #[inline]
    pub fn note_on(&self, id: NoteId) {
        if let Some(slot) = self.notes.entry(id) {
            slot.pitch_bend.store(0.0, Ordering::Release);
            slot.pressure.store(0.0, Ordering::Release);
            slot.slide.store(SLIDE_DEFAULT, Ordering::Release);
            slot.active.store(true, Ordering::Release);
        }
    }

    /// Release `id`'s slot, freeing its capacity.
    #[inline]
    pub fn note_off(&self, id: NoteId) {
        self.notes.remove(id);
    }

    /// `value`: -1.0 to 1.0
    #[inline]
    pub fn set_pitch_bend(&self, id: NoteId, value: f32) {
        if let Some(slot) = self.notes.entry(id) {
            slot.pitch_bend
                .store(value.clamp(-1.0, 1.0), Ordering::Release);
        }
    }

    /// `value`: 0.0 to 1.0
    #[inline]
    pub fn set_pressure(&self, id: NoteId, value: f32) {
        if let Some(slot) = self.notes.entry(id) {
            slot.pressure.store(value.clamp(0.0, 1.0), Ordering::Release);
        }
    }

    /// CC74 slide. `value`: 0.0 to 1.0
    #[inline]
    pub fn set_slide(&self, id: NoteId, value: f32) {
        if let Some(slot) = self.notes.entry(id) {
            slot.slide.store(value.clamp(0.0, 1.0), Ordering::Release);
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
    pub fn get_pitch_bend(&self, id: NoteId) -> f32 {
        let per_note = self.get_pitch_bend_per_note(id);
        let global = self.global_pitch_bend.load(Ordering::Acquire);
        (per_note + global).clamp(-1.0, 1.0)
    }

    #[inline]
    pub fn get_pitch_bend_per_note(&self, id: NoteId) -> f32 {
        self.notes
            .get(id)
            .map_or(0.0, |s| s.pitch_bend.load(Ordering::Acquire))
    }

    #[inline]
    pub fn get_pitch_bend_global(&self) -> f32 {
        self.global_pitch_bend.load(Ordering::Acquire)
    }

    /// Returns max(per-note, global) pressure.
    #[inline]
    pub fn get_pressure(&self, id: NoteId) -> f32 {
        let per_note = self.get_pressure_per_note(id);
        let global = self.global_pressure.load(Ordering::Acquire);
        per_note.max(global)
    }

    #[inline]
    pub fn get_pressure_per_note(&self, id: NoteId) -> f32 {
        self.notes
            .get(id)
            .map_or(0.0, |s| s.pressure.load(Ordering::Acquire))
    }

    /// Returns 0.5 (CC74 center) for inactive/absent notes.
    #[inline]
    pub fn get_slide(&self, id: NoteId) -> f32 {
        self.notes
            .get(id)
            .map_or(SLIDE_DEFAULT, |s| s.slide.load(Ordering::Acquire))
    }

    #[inline]
    pub fn is_active(&self, id: NoteId) -> bool {
        self.notes
            .get(id)
            .is_some_and(|s| s.active.load(Ordering::Acquire))
    }

    pub fn reset(&self) {
        self.notes.clear_all();
        self.global_pitch_bend.store(0.0, Ordering::Release);
        self.global_pressure.store(0.0, Ordering::Release);
    }

    /// Reset one note's per-note expression to defaults (pitch bend 0, pressure
    /// 0, slide centre), leaving the note active and every other note untouched.
    /// Backs MIDI 2.0 Per-Note Management **Reset** (M2-104 §7.4.15). No-op for a
    /// note with no live slot.
    #[inline]
    pub fn reset_note(&self, id: NoteId) {
        if self.notes.get(id).is_some() {
            self.set_pitch_bend(id, 0.0);
            self.set_pressure(id, 0.0);
            self.set_slide(id, SLIDE_DEFAULT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: classic-MPE identity for a note on the given channel.
    fn id(channel: u8, note: u8) -> NoteId {
        NoteId::from_channel_note(channel, note)
    }

    #[test]
    fn test_per_note_expression() {
        let expr = PerNoteExpression::new();
        let n60 = id(0, 60);

        expr.note_on(n60);
        assert!(expr.is_active(n60));

        expr.set_pitch_bend(n60, 0.5);
        assert!((expr.get_pitch_bend(n60) - 0.5).abs() < 0.001);

        expr.set_pressure(n60, 0.75);
        assert!((expr.get_pressure(n60) - 0.75).abs() < 0.001);

        expr.set_slide(n60, 0.3);
        assert!((expr.get_slide(n60) - 0.3).abs() < 0.001);

        expr.note_off(n60);
        assert!(!expr.is_active(n60));
    }

    #[test]
    fn test_global_expression() {
        let expr = PerNoteExpression::new();
        let n60 = id(0, 60);

        expr.note_on(n60);
        expr.set_pitch_bend(n60, 0.2);
        expr.set_global_pitch_bend(0.3);

        assert!((expr.get_pitch_bend(n60) - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_expression_clamping() {
        let expr = PerNoteExpression::new();
        let n60 = id(0, 60);

        expr.set_pitch_bend(n60, 2.0);
        assert!((expr.get_pitch_bend(n60) - 1.0).abs() < 0.001);

        expr.set_pitch_bend(n60, -2.0);
        assert!((expr.get_pitch_bend(n60) - (-1.0)).abs() < 0.001);

        expr.set_pressure(n60, 1.5);
        assert!((expr.get_pressure(n60) - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_reset_clears_everything() {
        let expr = PerNoteExpression::new();
        let n60 = id(0, 60);
        let n72 = id(0, 72);

        expr.note_on(n60);
        expr.note_on(n72);
        expr.set_pitch_bend(n60, 0.5);
        expr.set_pressure(n72, 0.8);
        expr.set_slide(n60, 0.9);
        expr.set_global_pitch_bend(0.3);
        expr.set_global_pressure(0.6);

        expr.reset();

        assert!(!expr.is_active(n60));
        assert!(!expr.is_active(n72));
        assert!((expr.get_pitch_bend_per_note(n60)).abs() < 0.001);
        assert!((expr.get_pressure_per_note(n72)).abs() < 0.001);
        assert!((expr.get_slide(n60) - 0.5).abs() < 0.001);
        assert!((expr.get_pitch_bend_global()).abs() < 0.001);
        assert!((expr.get_pressure(n60)).abs() < 0.001);
    }

    #[test]
    fn test_absent_note_returns_defaults() {
        let expr = PerNoteExpression::new();
        let absent = id(0, 100);

        assert!(!expr.is_active(absent));
        assert!((expr.get_pitch_bend(absent)).abs() < 0.001);
        assert!((expr.get_pressure(absent)).abs() < 0.001);
        assert!((expr.get_slide(absent) - 0.5).abs() < 0.001);
    }

    #[test]
    fn same_pitch_different_channel_is_independent() {
        // Two notes at pitch 60 on different member channels must not alias in
        // the per-note store.
        let expr = PerNoteExpression::new();
        let a = id(1, 60);
        let b = id(2, 60);

        expr.note_on(a);
        expr.note_on(b);
        expr.set_pitch_bend(a, 0.4);
        expr.set_pitch_bend(b, -0.4);

        assert!((expr.get_pitch_bend_per_note(a) - 0.4).abs() < 0.001);
        assert!((expr.get_pitch_bend_per_note(b) - (-0.4)).abs() < 0.001);

        // Releasing one leaves the other untouched.
        expr.note_off(a);
        assert!(!expr.is_active(a));
        assert!(expr.is_active(b));
        assert!((expr.get_pitch_bend_per_note(b) - (-0.4)).abs() < 0.001);
    }
}
