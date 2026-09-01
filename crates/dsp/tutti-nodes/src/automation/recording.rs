//! Automation *recording* — the capture side, companion to the playback-side
//! [`AutomationLaneNode`](crate::automation::AutomationLaneNode).
//!
//! [`Recorder`] is the **write** side of a [`Curve`]: where a curve is
//! `beat -> value`, a recorder is fed `(beat, value)` samples by whoever watches
//! the param and hands back an [`AutomationEnvelope`] — itself a `Curve` — when
//! the take ends. Like every other curve in the engine it holds no clock and
//! consults no loop range: the caller supplies an already-resolved, already
//! loop-wrapped [`Beat`].
//!
//! A recorder is also a `Curve` in its own right, reading the take *in progress*
//! (see the [`Curve`] impl). That is what makes latch-hold a plain layer on the
//! app's `LayeredCurve` rather than a second, disagreeing accumulator: the
//! in-flight take is installed under [`LayerKey::AUTOMATION`](tutti_mod::LayerKey)
//! and outranks the saved envelope for the take's duration, using the same
//! `base + Σ layers` rule as everything else.
//!
//! ## What lives where
//!
//! One recorder is one lane under capture. The *map* from whatever a host calls a
//! target to its recorder is the host's business, not the engine's — a
//! `HashMap<YourTarget, Recorder>` in whatever the host already owns. The engine
//! has no opinion on how targets are addressed, so it holds no registry and
//! carries no target trait.

use audio_automation::{AutomationEnvelope, AutomationPoint, CurveType};
use tutti_mod::Curve;
use tutti_types::{Beat, BeatDuration};

// ───────────────────────────── mode ──────────────────────────────

/// How a [`Recorder`] captures — the three recording disciplines every DAW
/// spells the same way.
///
/// There is no `Off` and no `Play`: not recording is the absence of a recorder
/// (the host's map simply has no entry), and *playback* is
/// [`Curve::value_at`] on the saved envelope. Both were states on
/// `audio_automation::AutomationState`, whose extra arms this type deliberately
/// drops — a recorder that also answered reads would be a second accumulator
/// competing with `LayeredCurve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordMode {
    /// Capture continuously from the first sample, no touch required.
    Write,
    /// Capture only while the control is held; the take ends on release.
    Touch,
    /// Capture while held, then hold the released value until the take ends.
    Latch,
}

impl RecordMode {
    /// Whether a take begins on [`touch`](Recorder::touch) rather than on the
    /// first [`record`](Recorder::record).
    #[inline]
    pub fn starts_on_touch(self) -> bool {
        matches!(self, Self::Touch | Self::Latch)
    }

    /// Whether [`release`](Recorder::release) ends the take outright (`Touch`)
    /// rather than holding the released value (`Latch`).
    #[inline]
    pub fn stops_on_release(self) -> bool {
        matches!(self, Self::Touch)
    }
}

// ──────────────────────────── config ─────────────────────────────

/// Capture policy: how densely points are written, and whether the take is
/// thinned when it ends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordingConfig {
    /// Minimum beat gap between recorded points. A `record` closer than this to
    /// the previous point is dropped, so a control dragged at frame rate does
    /// not write one point per frame.
    pub min_point_interval: BeatDuration,
    /// Douglas-Peucker tolerance applied by [`Recorder::finish`] when
    /// [`auto_simplify`](Self::auto_simplify) is set. In value-space (`f32`, the
    /// unit-erased space the points live in), not beats.
    pub simplify_tolerance: f32,
    /// Whether [`Recorder::finish`] thins the take before handing it back.
    pub auto_simplify: bool,
    /// Curve type stamped on each recorded point.
    pub default_curve: CurveType,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            min_point_interval: BeatDuration(0.01),
            simplify_tolerance: 0.01,
            auto_simplify: true,
            default_curve: CurveType::Linear,
        }
    }
}

// ───────────────────────────── take ──────────────────────────────

/// The take in progress. Absent between takes.
///
/// The two variants are the *whole* difference between `Touch` and `Latch`:
/// both capture while held, and on release `Touch` drops to `None` while `Latch`
/// drops to `Held`. Stating that as a variant is what keeps the mode check out
/// of the read path.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Take {
    /// The control is held. Samples land in the envelope.
    Capturing { last_beat: Beat, last_value: f32 },
    /// Released under `Latch` — the value is frozen until the take ends.
    Held { value: f32 },
}

// ──────────────────────────── recorder ───────────────────────────

/// One automation lane under capture: the write side of a [`Curve`].
///
/// Takes `&mut self` throughout. The previous incarnation held its envelope in
/// an `Arc<RwLock<_>>` so a concurrent map could hand out `&self` recorders;
/// with the map host-side that lock had no reader to protect, and the borrow
/// checker does the job for free. (Contrast [`ModTarget`](tutti_mod::ModTarget),
/// which *is* `&self` — a driver holds it as `Arc<dyn ModTarget>` and writes it
/// once per frame. A recorder has no such driver.)
#[derive(Debug, Clone)]
pub struct Recorder {
    envelope: AutomationEnvelope<f32>,
    mode: RecordMode,
    take: Option<Take>,
    config: RecordingConfig,
}

impl Recorder {
    /// A recorder over an empty envelope clamped to `[min, max]`.
    pub fn new(mode: RecordMode, min: f32, max: f32) -> Self {
        Self::with_config(mode, min, max, RecordingConfig::default())
    }

    /// [`new`](Self::new) with an explicit capture policy.
    pub fn with_config(mode: RecordMode, min: f32, max: f32, config: RecordingConfig) -> Self {
        Self {
            // `0.0` is the envelope's *target label*, not a value — the label is
            // unused here (the host's map key addresses the lane), and `Curve`'s
            // blanket impl ignores it beyond its thread bounds.
            envelope: AutomationEnvelope::new(0.0).with_range(min, max),
            mode,
            take: None,
            config,
        }
    }

    /// A recorder that overdubs onto an existing envelope — a second pass over a
    /// lane that already has points.
    pub fn overdub(mode: RecordMode, envelope: AutomationEnvelope<f32>) -> Self {
        Self {
            envelope,
            mode,
            take: None,
            config: RecordingConfig::default(),
        }
    }

    /// The capture discipline currently in force.
    pub fn mode(&self) -> RecordMode {
        self.mode
    }

    /// Switch capture discipline. Changing mode mid-take ends the take (the new
    /// discipline's release semantics never applied to it), keeping whatever was
    /// already captured.
    pub fn set_mode(&mut self, mode: RecordMode) {
        if self.mode != mode {
            self.take = None;
            self.mode = mode;
        }
    }

    /// The capture tuning currently in force — thinning tolerance and the like.
    pub fn config(&self) -> &RecordingConfig {
        &self.config
    }

    /// Replaces the capture tuning.
    ///
    /// Unlike [`set_mode`](Self::set_mode) this does **not** end a take in
    /// progress: the new settings apply to points captured from here on, and
    /// what is already recorded stays as it was.
    pub fn set_config(&mut self, config: RecordingConfig) {
        self.config = config;
    }

    /// The envelope written so far, including the take in progress.
    pub fn envelope(&self) -> &AutomationEnvelope<f32> {
        &self.envelope
    }

    /// Whether a take is currently open (capturing, or latch-held).
    pub fn is_taking(&self) -> bool {
        self.take.is_some()
    }

    /// The control was grabbed: open a take at `(beat, value)`.
    ///
    /// A no-op in [`Write`](RecordMode::Write), which captures from the first
    /// [`record`](Self::record) without waiting to be touched.
    pub fn touch(&mut self, beat: Beat, value: f32) {
        if !self.mode.starts_on_touch() {
            return;
        }
        self.take = Some(Take::Capturing {
            last_beat: beat,
            last_value: value,
        });
        self.write_point(beat, value);
    }

    /// A sample from the watched param. Written only while a take is capturing,
    /// and only if `min_point_interval` beats have passed since the last point.
    pub fn record(&mut self, beat: Beat, value: f32) {
        match self.take {
            // Write opens its own take on first sample — nothing to touch.
            None if self.mode == RecordMode::Write => {
                self.take = Some(Take::Capturing {
                    last_beat: beat,
                    last_value: value,
                });
            }
            Some(Take::Capturing { last_beat, .. }) => {
                // Thin at the configured density. Only a *forward* gap counts:
                // a backwards jump is a seek, which should write immediately
                // rather than be swallowed as "too soon".
                let gap = beat - last_beat;
                if gap > BeatDuration(0.0) && gap < self.config.min_point_interval {
                    return;
                }
            }
            // No take (Touch/Latch untouched), or latch-held — neither captures.
            _ => return,
        }

        self.write_point(beat, value);
        self.take = Some(Take::Capturing {
            last_beat: beat,
            last_value: value,
        });
    }

    /// The control was let go.
    ///
    /// `Touch` ends the take. `Latch` freezes `value` until [`finish`](Self::finish).
    /// `Write` ignores this — it captures until the take ends.
    pub fn release(&mut self, beat: Beat, value: f32) {
        if self.take.is_none() {
            return;
        }
        if self.mode.stops_on_release() {
            self.write_point(beat, value);
            self.take = None;
        } else if self.mode == RecordMode::Latch {
            self.write_point(beat, value);
            self.take = Some(Take::Held { value });
        }
    }

    /// End the take and hand back the envelope, thinned if
    /// [`auto_simplify`](RecordingConfig::auto_simplify) is set.
    ///
    /// Takes `self`: the take is over, so consuming the recorder is what the
    /// host's map does anyway (remove the entry, keep the envelope). This is the
    /// only way to get the envelope by value.
    pub fn finish(mut self) -> AutomationEnvelope<f32> {
        if self.config.auto_simplify {
            self.envelope.simplify(self.config.simplify_tolerance);
        }
        self.envelope
    }

    fn write_point(&mut self, beat: Beat, value: f32) {
        self.envelope.add_point(AutomationPoint::with_curve(
            beat.get(),
            value,
            self.config.default_curve,
        ));
    }
}

/// The take **in progress** — not the saved lane.
///
/// This is what makes latch-hold composable: the host installs the live recorder
/// as the [`LayerKey::AUTOMATION`](tutti_mod::LayerKey) layer on the param's
/// `LayeredCurve` for the take's duration, and it outranks the saved envelope
/// under the ordinary keyed-upsert rule — no second summation path.
///
/// `None` when no take is open, preserving the empty-vs-zero distinction
/// [`Curve`] requires: between takes the recorder contributes *nothing* and the
/// saved envelope shows through, rather than pinning the param to `0.0`.
impl Curve for Recorder {
    fn value_at(&self, beat: Beat) -> Option<f32> {
        match self.take {
            Some(Take::Held { value }) => Some(value),
            Some(Take::Capturing { .. }) => self.envelope.value_at(beat),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorder(mode: RecordMode) -> Recorder {
        let mut r = Recorder::new(mode, 0.0, 1.0);
        // Point-count assertions below are about capture, not thinning.
        r.config.auto_simplify = false;
        r
    }

    // ── mode ──

    #[test]
    fn write_captures_without_touch() {
        let mut r = recorder(RecordMode::Write);
        r.record(Beat(0.0), 0.1);
        r.record(Beat(1.0), 0.2);
        r.record(Beat(2.0), 0.3);
        assert_eq!(r.envelope().len(), 3);
    }

    #[test]
    fn touch_and_latch_ignore_record_until_touched() {
        for mode in [RecordMode::Touch, RecordMode::Latch] {
            let mut r = recorder(mode);
            r.record(Beat(0.0), 0.5);
            r.record(Beat(1.0), 0.6);
            assert!(
                r.envelope().is_empty(),
                "{mode:?} must not capture before touch"
            );
            assert!(!r.is_taking());
        }
    }

    #[test]
    fn touch_on_write_is_a_noop() {
        let mut r = recorder(RecordMode::Write);
        r.touch(Beat(0.0), 0.5);
        assert!(r.envelope().is_empty());
        assert!(!r.is_taking());
    }

    // ── take lifecycle ──

    #[test]
    fn touch_records_then_release_ends_the_take() {
        let mut r = recorder(RecordMode::Touch);
        r.touch(Beat(0.0), 0.5);
        assert_eq!(r.envelope().len(), 1);

        r.record(Beat(1.0), 0.6);
        r.record(Beat(2.0), 0.7);
        assert_eq!(r.envelope().len(), 3);

        r.release(Beat(3.0), 0.8);
        assert_eq!(r.envelope().len(), 4, "release writes a final point");
        assert!(!r.is_taking(), "Touch ends the take on release");
    }

    #[test]
    fn touch_stops_capturing_after_release() {
        let mut r = recorder(RecordMode::Touch);
        r.touch(Beat(0.0), 0.5);
        r.release(Beat(1.0), 0.6);
        let after_release = r.envelope().len();

        r.record(Beat(2.0), 0.9);
        assert_eq!(
            r.envelope().len(),
            after_release,
            "a released Touch take must not resume on further samples"
        );
    }

    #[test]
    fn latch_holds_after_release() {
        let mut r = recorder(RecordMode::Latch);
        r.touch(Beat(2.0), 0.5);
        r.record(Beat(3.0), 0.6);
        r.release(Beat(4.0), 0.7);

        assert!(r.is_taking(), "Latch keeps the take open");
        assert_eq!(
            r.value_at(Beat(5.0)),
            Some(0.7),
            "held value reads at any later beat"
        );
        assert_eq!(
            r.value_at(Beat(100.0)),
            Some(0.7),
            "the hold is beat-independent"
        );
    }

    #[test]
    fn latch_does_not_capture_while_held() {
        let mut r = recorder(RecordMode::Latch);
        r.touch(Beat(0.0), 0.5);
        r.release(Beat(1.0), 0.7);
        let held = r.envelope().len();

        r.record(Beat(2.0), 0.9);
        assert_eq!(
            r.envelope().len(),
            held,
            "a held latch take freezes; samples do not land"
        );
        assert_eq!(r.value_at(Beat(2.0)), Some(0.7), "still reading the hold");
    }

    #[test]
    fn release_without_a_take_is_a_noop() {
        let mut r = recorder(RecordMode::Touch);
        r.release(Beat(1.0), 0.5);
        assert!(r.envelope().is_empty());
        assert!(!r.is_taking());
    }

    // ── curve impl ──

    #[test]
    fn no_take_contributes_nothing() {
        // The empty-vs-zero distinction: between takes the saved envelope must
        // show through, so the recorder reads None rather than 0.0.
        let mut r = recorder(RecordMode::Touch);
        assert_eq!(r.value_at(Beat(0.0)), None);

        r.touch(Beat(0.0), 0.5);
        r.release(Beat(1.0), 0.6);
        assert_eq!(r.value_at(Beat(2.0)), None, "Touch releases to nothing");
    }

    #[test]
    fn capturing_reads_the_envelope_being_written() {
        let mut r = recorder(RecordMode::Touch);
        r.touch(Beat(0.0), 0.0);
        r.record(Beat(4.0), 1.0);
        // Mid-take the recorder reads its own in-progress curve, interpolated.
        let mid = r.value_at(Beat(2.0)).expect("capturing take has a value");
        assert!((mid - 0.5).abs() < 0.01, "expected ~0.5, got {mid}");
    }

    // ── thinning ──

    #[test]
    fn min_interval_drops_dense_samples() {
        let mut r = Recorder::with_config(
            RecordMode::Write,
            0.0,
            1.0,
            RecordingConfig {
                min_point_interval: BeatDuration(0.5),
                auto_simplify: false,
                ..Default::default()
            },
        );

        r.record(Beat(0.0), 0.5); // opens the take, writes
        r.record(Beat(0.1), 0.6); // +0.1 — too soon
        r.record(Beat(0.4), 0.7); // +0.4 from 0.0 — still too soon
        r.record(Beat(0.5), 0.8); // +0.5 — writes
        assert_eq!(r.envelope().len(), 2);
    }

    #[test]
    fn a_backwards_jump_always_writes() {
        // A seek during a take is not "too soon" — the gap is negative, so the
        // density check must not swallow it.
        let mut r = Recorder::with_config(
            RecordMode::Write,
            0.0,
            1.0,
            RecordingConfig {
                min_point_interval: BeatDuration(0.5),
                auto_simplify: false,
                ..Default::default()
            },
        );
        r.record(Beat(4.0), 0.5);
        r.record(Beat(0.0), 0.9);
        assert_eq!(r.envelope().len(), 2, "the seek-back point must land");
    }

    #[test]
    fn finish_simplifies_when_configured() {
        let mut r = Recorder::new(RecordMode::Write, 0.0, 1.0);
        r.config.auto_simplify = true;
        // A dead-straight ramp: every interior point is redundant.
        r.config.min_point_interval = BeatDuration(0.0);
        for i in 0..=10 {
            r.record(Beat(i as f64), i as f32 / 10.0);
        }
        let dense = r.envelope().len();
        let env = r.finish();
        assert!(
            env.len() < dense,
            "collinear points should thin: {dense} -> {}",
            env.len()
        );
    }

    #[test]
    fn finish_preserves_points_when_not_simplifying() {
        let mut r = recorder(RecordMode::Write);
        r.record(Beat(0.0), 0.1);
        r.record(Beat(1.0), 0.9);
        r.record(Beat(2.0), 0.2);
        let env = r.finish();
        assert_eq!(env.len(), 3);
    }

    // ── mode switch ──

    #[test]
    fn changing_mode_ends_the_take_but_keeps_points() {
        let mut r = recorder(RecordMode::Latch);
        r.touch(Beat(0.0), 0.5);
        r.record(Beat(1.0), 0.6);
        let captured = r.envelope().len();

        r.set_mode(RecordMode::Write);
        assert!(
            !r.is_taking(),
            "the old discipline's take does not carry over"
        );
        assert_eq!(r.envelope().len(), captured, "captured points survive");
    }

    #[test]
    fn setting_the_same_mode_leaves_the_take_alone() {
        let mut r = recorder(RecordMode::Latch);
        r.touch(Beat(0.0), 0.5);
        r.set_mode(RecordMode::Latch);
        assert!(r.is_taking());
    }

    // ── overdub ──

    #[test]
    fn overdub_starts_from_an_existing_envelope() {
        let mut env = AutomationEnvelope::new(0.0f32).with_range(0.0, 1.0);
        env.add_point(AutomationPoint::new(0.0, 0.2));
        env.add_point(AutomationPoint::new(8.0, 0.4));

        let mut r = Recorder::overdub(RecordMode::Touch, env);
        assert_eq!(r.envelope().len(), 2);
        assert!(!r.is_taking());

        r.touch(Beat(4.0), 0.9);
        assert_eq!(
            r.envelope().len(),
            3,
            "the new point joins the existing ones"
        );
    }
}
