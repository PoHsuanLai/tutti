//! Following an outside tempo (feature `midi`; design 020, "Following an outside
//! tempo"; design 017, L7c): the session's transport takes the tempo and the
//! meter a MIDI 2.0 source sends as Flex Data Set Tempo and Set Time Signature
//! (M2-104 §7.5.3, §7.5.4).
//!
//! The pure core is `tutti_midi_runtime::TempoFollower`: it decodes and says
//! *what* to apply. This module says *where* and *how*: it drains an event tap
//! on the followed Group's channelless port of the ingest node in
//! [`Phase::Poll`](crate::Phase::Poll), places each change at the next block
//! start, builds the new [`TempoMap`] and [`MeterMap`] and publishes them.
//!
//! **A follow mode overlays the project's maps and never replaces them.** The
//! maps the project had when following began are kept apart; the published map
//! is the project's up to the beat of the first change, then the live steps.
//! Each live step truncates what came after it: a project point or ramp ahead
//! of the playhead is not played while following, because two authorities for
//! the same future beats is the bug to avoid. A ramp a step lands in is cut at
//! that beat. [`Midi::unfollow_tempo`](crate::Midi::unfollow_tempo) publishes the
//! project's maps again; the clock re-seats at its current beat, so the
//! playhead does not jump.
//!
//! **Where a change lands.** A change heard in a block is drained in the
//! `Poll` after it and takes effect at the start of the next block, never
//! behind the playhead: placing it at the frame it arrived on would be
//! retroactive. The step's beat is the transport's next block start, a frame
//! (the playhead's transport time plus the call's length) read through the map
//! in force (`TempoView::beat_at_frame`), never `Playhead::beat` extrapolated at
//! a tempo. A publish that lands after the audio thread has rendered past that
//! beat (a slow poll, a big block) moves no beat: the clock re-seats at its
//! current beat on a new map (`tempo_for_block`), so the beat stays continuous
//! and the new tempo applies from the block that reads the map. The step is
//! then behind the playhead in the map, and a bounce made from it renders the
//! tempo from the step's beat as the map says.
//!
//! **The meter** lands at the next bar line at or after that beat (a meter
//! change always begins a bar; M2-104 §7.5.4), on the same overlay.

use std::sync::Arc;

use tutti_engine::Playhead;
use tutti_graph::EventTapReader;
use tutti_midi_runtime::{Followed, TempoFollower};
use tutti_tempo::{TempoError, TempoMap, Transition};
use tutti_transport::{ProjectMeter, TransportSettings};
use tutti_types::{BarCount, Beat, Bpm, Frame, Meter, MeterChange, MeterMap, TimeSignature};
use tutti_ump::MidiGroup;

use crate::MidiEvent;

/// How many events the follower's tap holds between two polls. Set Tempo and
/// Set Time Signature are rare; a source that floods them loses the oldest
/// beyond this, counted and logged.
pub(crate) const FOLLOW_TAP_CAPACITY: usize = 256;

/// What [`Midi::follow_tempo`](crate::Midi::follow_tempo) follows.
///
/// The only source today is a Group's Flex Data Set Tempo (and Set Time
/// Signature) on the session's MIDI input node
/// ([`MidiNodes::input`](crate::MidiNodes::input)). **MIDI Clock and MIDI
/// Time Code are not followed yet**: a device that sends only those changes
/// nothing, and this struct has no field to choose a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowTempo {
    /// The Group followed. A Group is a cable of its own (M2-104 §2.1.2,
    /// Appendix B.1); the Group must be one the input hears
    /// ([`SessionBuilder::midi_groups`](crate::SessionBuilder::midi_groups)).
    pub group: MidiGroup,
    /// Whether Set Time Signature is followed too.
    pub meter: bool,
}

/// The one follower a session has: its core, its tap and the maps.
pub(crate) struct Follow {
    core: TempoFollower,
    tap: EventTapReader,
    /// The tempo map the project had when following began: what
    /// `unfollow_tempo` publishes again. The session has no project tempo
    /// apart from the published map, so this is the map it found.
    project_tempo: Arc<TempoMap>,
    /// The project's meter, likewise.
    project_meter: Arc<MeterMap>,
    /// The map this follower last published (the project's until a first
    /// step): what the next step builds on.
    tempo: Arc<TempoMap>,
    meter: Arc<MeterMap>,
}

/// What a `Poll` phase reads besides the follower.
pub(crate) struct FollowCx<'a> {
    pub(crate) settings: &'a TransportSettings,
    pub(crate) meter: &'a ProjectMeter,
    pub(crate) playhead: Playhead,
}

impl Follow {
    /// A follower reading `tap`, over the project's maps.
    pub(crate) fn new(
        core: TempoFollower,
        tap: EventTapReader,
        project_tempo: Arc<TempoMap>,
        project_meter: Arc<MeterMap>,
    ) -> Self {
        Self {
            core,
            tap,
            tempo: Arc::clone(&project_tempo),
            meter: Arc::clone(&project_meter),
            project_tempo,
            project_meter,
        }
    }

    /// A follower reading `tap` in place of this one: the project's maps and the
    /// overlay published so far carry over, so a second source replaces the
    /// first with the tempo in force staying until it sends one.
    pub(crate) fn replaced_by(self, core: TempoFollower, tap: EventTapReader) -> Self {
        Self { core, tap, ..self }
    }

    /// What the project had, to publish again, for the maps this follower
    /// changed (`None` for one it never touched).
    pub(crate) fn restore(&self) -> (Option<Arc<TempoMap>>, Option<Arc<MeterMap>>) {
        (
            (!Arc::ptr_eq(&self.tempo, &self.project_tempo))
                .then(|| Arc::clone(&self.project_tempo)),
            (!Arc::ptr_eq(&self.meter, &self.project_meter))
                .then(|| Arc::clone(&self.project_meter)),
        )
    }

    /// The tap's rate, to compare with the graph's.
    pub(crate) fn tap_rate(&self) -> tutti_types::SampleRate {
        self.tap.rate()
    }

    /// The tap's id.
    pub(crate) fn tap_id(&self) -> tutti_graph::EventTapId {
        self.tap.id()
    }

    /// Replaces a tap the graph re-rated.
    pub(crate) fn set_tap(&mut self, tap: EventTapReader) {
        self.tap = tap;
    }

    /// What the tap held, decoded, in order; counts what it lost.
    pub(crate) fn heard(&mut self) -> Vec<Followed> {
        let core = self.core;
        let heard = self.tap.drain().filter_map(|e| core.step(e.ump)).collect();
        let lost = self.tap.take_lost();
        if lost > 0 {
            tracing::warn!("midi: the tempo follower's tap lost {lost} events");
        }
        heard
    }

    /// Places what was heard at the next block start and publishes the new
    /// maps. The events say what was applied.
    pub(crate) fn apply(
        &mut self,
        heard: &[Followed],
        cx: &FollowCx<'_>,
        out: &mut Vec<crate::SessionEvent>,
    ) {
        if heard.is_empty() {
            return;
        }
        let (tempo_before, meter_before) = (Arc::clone(&self.tempo), Arc::clone(&self.meter));
        let beat = next_block_beat(&cx.playhead, &self.tempo);
        for change in heard {
            match *change {
                Followed::Tempo(tempo) => match step_at(&self.tempo, beat, tempo) {
                    Ok(Some(map)) => {
                        self.tempo = Arc::new(map);
                        out.push(MidiEvent::TempoFollowed { tempo, beat }.into());
                    }
                    Ok(None) => {}
                    // The builder refuses only a tempo whose segment duration overflows
                    // an `f64` (below ~1e-300 bpm); a `Bpm` from `flex_tempo` is at least
                    // 1.397 bpm (field `u32::MAX`), so this is not reached: nothing is
                    // published and nothing reported.
                    Err(_) => {}
                },
                Followed::Meter(signature) => {
                    let at = next_bar_line(&self.meter, beat);
                    if let Some(map) = meter_at(&self.meter, at, signature) {
                        self.meter = Arc::new(map);
                        out.push(
                            MidiEvent::MeterFollowed {
                                signature,
                                beat: at,
                            }
                            .into(),
                        );
                    }
                }
                _ => {}
            }
        }
        if !Arc::ptr_eq(&tempo_before, &self.tempo) {
            cx.settings.set_tempo_map(Arc::clone(&self.tempo));
        }
        if !Arc::ptr_eq(&meter_before, &self.meter) {
            cx.meter.set_meter(Arc::clone(&self.meter));
        }
    }
}

/// The beat the transport is at on the first frame of the next block, read
/// from a frame: the playhead's transport time plus the call's length (the
/// transport advances by it while it rolls) as a frame on `map`'s timeline, and
/// that frame's beat through `map` (`TempoView::beat_at_frame`). Never the
/// playhead's own beat extrapolated: that is one block stale and, after a ramp
/// or a locate in the last block, wrong.
///
/// A stopped transport does not advance: its next block starts where it is.
pub(crate) fn next_block_beat(playhead: &Playhead, map: &TempoMap) -> Beat {
    let rate = playhead.rate();
    let mut at = playhead.seconds();
    if playhead.playing() {
        at += playhead.span().to_seconds(rate);
    }
    let frame = Frame(at.to_samples(rate).get() as u64);
    map.view(rate).beat_at_frame(frame)
}

/// `map` with `tempo` taking over at `beat`: the map up to `beat` as it stands,
/// then a step to `tempo`, and nothing after. `Ok(None)` when that changes
/// nothing (the map already holds `tempo` from `beat` on, with no point after
/// it), so a source that repeats its tempo grows no map.
///
/// A ramp `beat` lands in is cut there: the ramp is kept up to `beat` (it reaches
/// the tempo `map` has at `beat`, so every beat before `beat` keeps its time),
/// and the step follows one ulp of a beat later, because a map holds one tempo
/// at a beat and a tempo that jumps needs two points. A `beat` before 0 is
/// beat 0.
///
/// # Errors
///
/// A [`TempoError`] when the map cannot hold the result (a tempo so small that a
/// segment's duration overflows).
pub(crate) fn step_at(
    map: &TempoMap,
    beat: Beat,
    tempo: Bpm,
) -> Result<Option<TempoMap>, TempoError> {
    let beat = Beat(beat.get().max(0.0));
    let points = map.points();
    if points
        .last()
        .is_some_and(|last| last.beat <= beat && last.tempo == tempo)
    {
        return Ok(None);
    }
    let kept = points.partition_point(|p| p.beat < beat);
    let Some(first) = points[..kept].first() else {
        return TempoMap::builder(tempo).build().map(Some);
    };
    let mut builder = TempoMap::builder(first.tempo);
    for pair in points[..kept].windows(2) {
        builder = extend(builder, pair[0].transition, pair[1].beat, pair[1].tempo);
    }
    let last = &points[kept - 1];
    builder = if last.transition == Transition::Step {
        builder.step_to(beat, tempo)
    } else {
        // The ramp is cut at `beat`: it reaches what the map has there.
        extend(builder, last.transition, beat, map.tempo_at(beat))
            .step_to(Beat(beat.get().next_up()), tempo)
    };
    builder.build().map(Some)
}

/// The next point of a builder, reached from the last by `transition`.
fn extend(
    builder: tutti_tempo::TempoMapBuilder,
    transition: Transition,
    beat: Beat,
    tempo: Bpm,
) -> tutti_tempo::TempoMapBuilder {
    match transition {
        Transition::Step => builder.step_to(beat, tempo),
        Transition::RampTime => builder.ramp_time_to(beat, tempo),
        Transition::RampBeats => builder.ramp_beats_to(beat, tempo),
    }
}

/// The first bar line at or after `beat` in `meter` (a downbeat, within the
/// meter's own tolerance, is its own bar line).
pub(crate) fn next_bar_line(meter: &MeterMap, beat: Beat) -> Beat {
    let position = meter.bar_at(beat);
    if position.is_downbeat() {
        position.bar_start
    } else {
        meter.bar_start(position.bar + BarCount(1))
    }
}

/// `meter` with `signature` from the bar line `at`: the meter before `at` as it
/// stands, then the change, and nothing after. `None` when that changes nothing.
pub(crate) fn meter_at(meter: &MeterMap, at: Beat, signature: TimeSignature) -> Option<MeterMap> {
    if meter.at(at) == signature && meter.changes().last().is_some_and(|c| c.beat <= at) {
        return None;
    }
    let kept = meter.changes().iter().filter(|c| c.beat < at).copied();
    Some(MeterMap::new(kept.chain([MeterChange::new(at, signature)])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_tempo::TempoPoint;

    fn bpm(x: f64) -> Bpm {
        Bpm::of(x)
    }

    fn ramp_60_to_180() -> TempoMap {
        TempoMap::builder(bpm(60.0))
            .ramp_time_to(Beat(8.0), bpm(180.0))
            .step_to(Beat(16.0), bpm(90.0))
            .build()
            .unwrap()
    }

    /// **A step on a constant map keeps every beat before it and holds the new
    /// tempo after**: the map is the old one up to the beat, then one step.
    #[test]
    fn a_step_on_a_constant_map_is_one_point() {
        let old = TempoMap::constant(bpm(120.0)).unwrap();
        let new = step_at(&old, Beat(3.0), bpm(90.0)).unwrap().unwrap();
        assert_eq!(
            new.points(),
            [
                TempoPoint::step(Beat(0.0), bpm(120.0)),
                TempoPoint::step(Beat(3.0), bpm(90.0)),
            ]
        );
        assert_eq!(new.seconds_at(Beat(3.0)), old.seconds_at(Beat(3.0)));
    }

    /// **A step truncates what came after it**: a project point ahead of the
    /// step is not in the overlay.
    #[test]
    fn a_step_drops_the_points_after_it() {
        let old = TempoMap::builder(bpm(100.0))
            .step_to(Beat(4.0), bpm(110.0))
            .step_to(Beat(8.0), bpm(120.0))
            .build()
            .unwrap();
        let new = step_at(&old, Beat(5.0), bpm(70.0)).unwrap().unwrap();
        let beats: Vec<f64> = new.points().iter().map(|p| p.beat.get()).collect();
        assert_eq!(beats, [0.0, 4.0, 5.0]);
        assert_eq!(new.tempo_at(Beat(100.0)), bpm(70.0));
    }

    /// **A ramp a step lands in is cut there**: the overlay reaches the tempo the
    /// ramp had at the beat, so every earlier beat keeps its time; the step
    /// follows, the map ends on it, and the ramp's own end (beat 8) and the
    /// point after it are gone.
    #[test]
    fn a_ramp_a_step_lands_in_is_cut_at_its_beat() {
        let old = ramp_60_to_180();
        let at = Beat(3.0);
        let cut = old.tempo_at(at);
        assert!(cut > bpm(60.0) && cut < bpm(180.0), "inside the ramp");
        let new = step_at(&old, at, bpm(100.0)).unwrap().unwrap();

        let points = new.points();
        assert_eq!(points.len(), 3, "{points:?}");
        assert_eq!(points[0], TempoPoint::ramp_time(Beat(0.0), bpm(60.0)));
        assert_eq!(
            points[1],
            TempoPoint::step(at, cut),
            "the ramp ends at the cut"
        );
        assert_eq!(points[2].beat, Beat(at.get().next_up()));
        assert_eq!(points[2].tempo, bpm(100.0));
        assert_eq!(
            points[2].transition,
            Transition::Step,
            "the map ends on a step"
        );

        for b in [0.0, 0.5, 1.0, 2.0, 2.999, 3.0] {
            let (a, n) = (old.seconds_at(Beat(b)).get(), new.seconds_at(Beat(b)).get());
            assert!((a - n).abs() < 1e-9, "beat {b}: {a} vs {n}");
        }
        assert_eq!(new.tempo_at(Beat(10.0)), bpm(100.0));
    }

    /// The same cut of a ramp that is linear in beats.
    #[test]
    fn a_beat_linear_ramp_is_cut_too() {
        let old = TempoMap::builder(bpm(60.0))
            .ramp_beats_to(Beat(8.0), bpm(180.0))
            .build()
            .unwrap();
        let new = step_at(&old, Beat(5.0), bpm(100.0)).unwrap().unwrap();
        assert_eq!(new.points()[0].transition, Transition::RampBeats);
        for b in [1.0, 4.0, 5.0] {
            let (a, n) = (old.seconds_at(Beat(b)).get(), new.seconds_at(Beat(b)).get());
            assert!((a - n).abs() < 1e-9, "beat {b}: {a} vs {n}");
        }
    }

    /// A step at beat 0 replaces the first tempo; a beat before 0 is beat 0.
    #[test]
    fn a_step_at_the_origin_replaces_the_first_tempo() {
        let old = ramp_60_to_180();
        for at in [Beat(0.0), Beat(-2.0)] {
            let new = step_at(&old, at, bpm(75.0)).unwrap().unwrap();
            assert_eq!(new.points(), [TempoPoint::step(Beat(0.0), bpm(75.0))]);
        }
    }

    /// **A repeated tempo grows no map**: the tempo already in force from the beat
    /// on, with nothing after, is no change; with a point after it, the step still
    /// truncates, so it is one.
    #[test]
    fn a_repeated_tempo_is_no_change() {
        let old = TempoMap::constant(bpm(120.0)).unwrap();
        assert!(step_at(&old, Beat(4.0), bpm(120.0)).unwrap().is_none());
        let with_tail = TempoMap::builder(bpm(120.0))
            .step_to(Beat(8.0), bpm(90.0))
            .build()
            .unwrap();
        assert!(
            step_at(&with_tail, Beat(4.0), bpm(120.0))
                .unwrap()
                .is_some()
        );
    }

    /// A tempo the map cannot hold is refused, not published.
    #[test]
    fn a_tempo_the_map_cannot_hold_is_refused() {
        let old = TempoMap::constant(bpm(120.0)).unwrap();
        let slowest = Bpm::new(f64::from_bits(1)).expect("positive and finite");
        assert!(step_at(&old, Beat(4.0), slowest).is_err());
    }

    fn four_four() -> MeterMap {
        MeterMap::default()
    }

    /// **The next bar line at or after a beat**: mid-bar goes to the next one, a
    /// downbeat is its own, in whatever meter is in force there.
    #[test]
    fn the_next_bar_line_is_at_or_after_the_beat() {
        let m = four_four();
        assert_eq!(next_bar_line(&m, Beat(1.3)), Beat(4.0));
        assert_eq!(next_bar_line(&m, Beat(4.0)), Beat(4.0), "on a bar line");
        assert_eq!(next_bar_line(&m, Beat(0.0)), Beat(0.0));
        assert_eq!(next_bar_line(&m, Beat(7.99)), Beat(8.0));
        let three_four = MeterMap::new([
            MeterChange::new(Beat(0.0), TimeSignature::of(4, 4)),
            MeterChange::new(Beat(4.0), TimeSignature::of(3, 4)),
        ]);
        assert_eq!(next_bar_line(&three_four, Beat(5.0)), Beat(7.0));
    }

    /// **The meter overlay keeps the meter before the bar line and drops what
    /// came after**, and a repeat of the meter in force is no change.
    #[test]
    fn a_meter_overlay_truncates_and_a_repeat_is_nothing() {
        let project = MeterMap::new([
            MeterChange::new(Beat(0.0), TimeSignature::of(4, 4)),
            MeterChange::new(Beat(16.0), TimeSignature::of(5, 4)),
        ]);
        let new = meter_at(&project, Beat(8.0), TimeSignature::of(3, 4)).unwrap();
        assert_eq!(
            new.changes(),
            [
                MeterChange::new(Beat(0.0), TimeSignature::of(4, 4)),
                MeterChange::new(Beat(8.0), TimeSignature::of(3, 4)),
            ]
        );
        assert!(meter_at(&four_four(), Beat(8.0), TimeSignature::of(4, 4)).is_none());
        assert!(
            meter_at(&project, Beat(8.0), TimeSignature::of(4, 4)).is_some(),
            "the project's change after it is dropped"
        );
    }
}
