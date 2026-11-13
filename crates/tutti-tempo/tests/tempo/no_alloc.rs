//! The audio thread's calls do not allocate: every lookup, the view's hinted
//! ones and the cell queries, on a map with step and both ramp shapes.

use crate::common::{R, pt};
use assert_no_alloc::assert_no_alloc;
use tutti_tempo::{CellGrid, TempoMap, Transition};
use tutti_types::{Beat, Frame, Seconds};

#[test]
fn queries_do_not_allocate() {
    let map = TempoMap::new((0..64).map(|i| {
        let shape = [
            Transition::Step,
            Transition::RampTime,
            Transition::RampBeats,
        ][i % 3];
        pt(
            i as f64 * 2.0,
            80.0 + (i % 7) as f64 * 10.0,
            if i == 63 { Transition::Step } else { shape },
        )
    }))
    .unwrap();
    let grid = CellGrid::of(128);
    let mut acc = 0.0;
    assert_no_alloc(|| {
        let mut view = map.view(R);
        for i in 0..4000u64 {
            let beat = Beat((i % 130) as f64 * 0.97);
            let frame = Frame::new(i * 977);
            let secs = Seconds(i as f64 * 0.01);
            acc += map.seconds_at(beat).get() + view.seconds_at(beat).get();
            acc += map.beat_at_seconds(secs).get() + view.beat_at_seconds(secs).get();
            acc += map.tempo_at(beat).get() + view.tempo_at(beat).get();
            acc += map.tempo_at_seconds(secs).get() + view.tempo_at_seconds(secs).get();
            acc += view.tempo_slope(secs, Seconds(secs.get() + 0.1));
            acc += view.tempo_at_frame(frame).get();
            acc += view.beat_at_frame(frame).get();
            acc += view.frame_at_or_after(beat).get() as f64;
            acc += view.next_change(frame).map_or(0.0, |f| f.get() as f64);
            let cell = view.chord_cell(frame, grid);
            acc += cell.beat_at(frame, R).get() + cell.tempo.get();
            acc += map.points().len() as f64;
        }
    });
    assert!(acc.is_finite() && acc > 0.0, "the calls ran");
}
