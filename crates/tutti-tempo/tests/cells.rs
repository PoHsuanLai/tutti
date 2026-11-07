//! A ramp as constant-tempo cells on one fixed frame grid.
//!
//! "Mutation (run)" means the change was made, the test seen to fail, and the
//! change reverted.

mod common;

use common::{Lcg, R, pt};
use std::collections::BTreeMap;
use tutti_tempo::{Cell, CellGrid, Shape, TempoMap, chord_cell};
use tutti_types::{Beat, Frame};

fn mixed() -> TempoMap {
    TempoMap::new([
        pt(0.0, 60.0, Shape::RampTime),
        pt(8.0, 240.0, Shape::Step),
        pt(10.0, 100.0, Shape::RampBeats),
        pt(16.0, 180.0, Shape::Step),
    ])
    .unwrap()
}

/// Walks `blocks` (frame counts, cycled) over `0..end` the way a render would,
/// asking for every cell each block touches. Returns the cells by start frame.
fn cells_seen(map: &TempoMap, grid: CellGrid, end: u64, blocks: &[u64]) -> BTreeMap<u64, Cell> {
    let mut seen = BTreeMap::new();
    let (mut at, mut i) = (0, 0);
    while at < end {
        let block_end = (at + blocks[i % blocks.len()]).min(end);
        i += 1;
        let mut f = at;
        while f < block_end {
            let c = chord_cell(map, Frame(f), grid, R);
            if let Some(earlier) = seen.insert(c.start.get(), c) {
                assert_eq!(earlier, c, "the cell at {} changed between blocks", c.start);
            }
            f = c.end.get();
        }
        at = block_end;
    }
    seen
}

/// The same map through several block layouts, including an irregular one, gives
/// identical cells at identical absolute frames, and they are the cells a walk
/// by the grid alone gives.
///
/// Mutation (run): `chord_cell` starts the cell at `frame` instead of the grid multiple
/// (`let start = frame`); a cell then depends on where a block began.
#[test]
fn cells_do_not_depend_on_the_block_layout() {
    let map = mixed();
    let grid = CellGrid::of(128);
    let end = map.frame_at_or_after(Beat(16.0), R).get();
    let reference = cells_seen(&map, grid, end, &[grid.get()]);
    assert!(
        reference.len() > 1000,
        "a long ramp: {} cells",
        reference.len()
    );
    let mut rng = Lcg(5);
    let irregular: Vec<u64> = (0..64).map(|_| 1 + (rng.next() * 3000.0) as u64).collect();
    for layout in [
        vec![32],
        vec![100],
        vec![480],
        vec![512],
        vec![1000],
        vec![4096],
        vec![65_536],
        irregular,
    ] {
        let seen = cells_seen(&map, grid, end, &layout);
        assert_eq!(seen, reference, "blocks {layout:?}");
    }
    // Every frame in a cell asks for that cell.
    for f in (0..end).step_by(997) {
        let c = chord_cell(&map, Frame(f), grid, R);
        assert_eq!(c, reference[&c.start.get()]);
        assert!(c.start.get() <= f && f < c.end.get());
        assert_eq!(c.start.get() % grid.get(), 0);
    }
}

/// Cells tile the line, start on the map's exact beat, and the chord lands on the
/// map's beat at the cell's end (to float rounding).
///
/// Mutation (run): a cell ends `grid - 1` frames on; the cells no longer tile.
#[test]
fn cells_tile_and_seat_on_the_map() {
    let map = mixed();
    for grid in [
        CellGrid::of(1),
        CellGrid::of(37),
        CellGrid::of(128),
        CellGrid::of(512),
    ] {
        let mut f = 0;
        for _ in 0..500 {
            let c = chord_cell(&map, Frame(f), grid, R);
            assert_eq!(c.start, Frame(f));
            assert_eq!(c.end.get() - c.start.get(), grid.get());
            assert_eq!(
                c.beat,
                map.beat_at_frame(c.start, R),
                "exact beat at the start"
            );
            let at_end = c.beat_at(c.end, R).get();
            let want = map.beat_at_frame(c.end, R).get();
            assert!(
                (at_end - want).abs() < 1e-9,
                "grid {grid:?} cell {f}: {at_end} vs {want}"
            );
            f = c.end.get();
        }
    }
}

/// The chord's worst error against the curve, in frames, by grid (every 7th
/// frame of the ramp): the ceilings are the measured worst figures with a
/// quarter of headroom (0.0399 frames for 60 -> 240 over 8 beats at 128).
///
/// Mutation (run): the chord tempo divides by `grid + 1`; the error leaves every ceiling.
#[test]
fn chord_error_stays_under_a_fraction_of_a_frame() {
    // (shape, from, to, beats, [(grid, ceiling in frames)])
    let cases: [(Shape, f64, f64, f64, [(u64, f64); 3]); 3] = [
        (
            Shape::RampTime,
            60.0,
            240.0,
            8.0,
            [(32, 0.0032), (128, 0.05), (512, 0.8)],
        ),
        (
            Shape::RampBeats,
            60.0,
            240.0,
            8.0,
            [(32, 0.0013), (128, 0.02), (512, 0.33)],
        ),
        (
            Shape::RampTime,
            120.0,
            140.0,
            16.0,
            [(32, 0.0002), (128, 0.0013), (512, 0.02)],
        ),
    ];
    for (shape, a, b, db, grids) in cases {
        let map = TempoMap::new([pt(0.0, a, shape), pt(db, b, Shape::Step)]).unwrap();
        let end = map.frame_at_or_after(Beat(db), R).get();
        for (g, ceiling) in grids {
            let grid = CellGrid::of(g);
            let mut worst = 0.0f64;
            for f in (0..end).step_by(7) {
                let c = chord_cell(&map, Frame(f), grid, R);
                let want = map.beat_at_frame(Frame(f), R).get();
                let got = c.beat_at(Frame(f), R).get();
                // Beat error to frames, at the local rate.
                let rate = map.tempo_at(Beat(want)).get() / 60.0;
                worst = worst.max((got - want).abs() / rate * R.get());
            }
            assert!(
                worst < ceiling,
                "{shape:?} {a}->{b} grid {g}: {worst} frames"
            );
        }
    }
}

/// On a constant tempo every cell has that tempo, to rounding.
///
/// Mutation (run): the chord tempo multiplies by 59 instead of 60.
#[test]
fn a_constant_tempo_gives_that_tempo() {
    let map = TempoMap::constant(tutti_types::Bpm(133.3)).unwrap();
    for f in [0, 1, 127, 128, 99_999] {
        let c = chord_cell(&map, Frame(f), CellGrid::of(128), R);
        assert!((c.tempo.get() - 133.3).abs() < 1e-9, "{}", c.tempo.get());
    }
}
