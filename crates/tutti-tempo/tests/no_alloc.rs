//! The audio thread's calls do not allocate: every lookup, the hinted ones and
//! the cell queries, on a map with step and both ramp shapes.
//!
//! Mutation (run): `seconds_at_with` boxes a value (`black_box(Box::new(b))`); the gate aborts
//! with "memory allocation of 8 bytes failed".

mod common;

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use common::{R, pt};
use tutti_tempo::{CellGrid, Cursor, Shape, TempoMap, chord_cell, chord_cell_with};
use tutti_types::{Beat, Frame};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

#[test]
fn queries_do_not_allocate() {
    let map = TempoMap::new((0..64).map(|i| {
        let shape = [Shape::Step, Shape::RampTime, Shape::RampBeats][i % 3];
        pt(
            i as f64 * 2.0,
            80.0 + (i % 7) as f64 * 10.0,
            if i == 63 { Shape::Step } else { shape },
        )
    }))
    .unwrap();
    let grid = CellGrid::of(128);
    let mut hint = Cursor::default();
    let mut acc = 0.0;
    assert_no_alloc(|| {
        for i in 0..4000u64 {
            let beat = Beat((i % 130) as f64 * 0.97);
            let frame = Frame(i * 977);
            acc += map.seconds_at(beat) + map.seconds_at_with(beat, &mut hint);
            acc += map.beat_at_seconds(i as f64 * 0.01).get();
            acc += map.beat_at_seconds_with(i as f64 * 0.01, &mut hint).get();
            acc += map.tempo_at(beat).get() + map.tempo_at_seconds(i as f64 * 0.01).get();
            acc += map.beat_at_frame(frame, R).get();
            acc += map.frame_at_or_after(beat, R).get() as f64;
            acc += map.frame_at_or_after_with(beat, R, &mut hint).get() as f64;
            acc += map.tempo_at_with(beat, &mut hint).get();
            acc += map.tempo_at_seconds_with(i as f64 * 0.01, &mut hint).get();
            acc += map.beat_at_frame_with(frame, R, &mut hint).get();
            acc += map.next_change(frame, R).map_or(0.0, |f| f.get() as f64);
            acc += map
                .next_change_with(frame, R, &mut hint)
                .map_or(0.0, |f| f.get() as f64);
            let cell = chord_cell(&map, frame, grid, R);
            acc += cell.beat_at(frame, R).get() + cell.tempo.get();
            let cell = chord_cell_with(&map, frame, grid, R, &mut hint);
            acc += cell.beat_at(frame, R).get() + cell.tempo.get();
            acc += map.points().len() as f64;
        }
    });
    assert!(acc.is_finite() && acc > 0.0, "the calls ran");
}
