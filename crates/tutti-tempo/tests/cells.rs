//! A ramp as constant-tempo cells on one fixed frame grid.
//!
//! "Mutation (run)" means the change was made, the test seen to fail, and the
//! change reverted.

mod common;

use common::{Lcg, R, pt};
use std::collections::BTreeMap;
use tutti_tempo::{Cell, CellGrid, Cursor, Shape, TempoMap, chord_cell};
use tutti_types::{Beat, Bpm, Frame};

/// A ramp, a step that falls between two frames (its beat is not a frame
/// multiple of anything), a beat ramp, and a step 12.00003 beats in.
fn mixed() -> TempoMap {
    TempoMap::new([
        pt(0.0, 60.0, Shape::RampTime),
        pt(8.0, 240.0, Shape::Step),
        pt(10.000_01, 100.0, Shape::RampBeats),
        pt(12.000_03, 90.0, Shape::Step),
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
/// (`let on_grid = frame.get()`); a cell then depends on where a block began.
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
    // The cells tile the line, never cross a grid multiple or a point, and
    // include a step that falls inside a grid cell.
    let cuts: Vec<u64> = map.points()[1..]
        .iter()
        .map(|p| map.frame_at_or_after(p.beat, R).get())
        .collect();
    assert!(
        cuts.iter().any(|c| c % grid.get() != 0),
        "a step off the grid: {cuts:?}"
    );
    let all: Vec<&Cell> = reference.values().collect();
    for pair in all.windows(2) {
        assert_eq!(pair[0].end, pair[1].start, "cells tile");
    }
    for c in &all {
        assert!(
            c.start.get() / grid.get() == (c.end.get() - 1) / grid.get(),
            "{c:?} crosses a grid multiple"
        );
        assert!(
            !cuts.iter().any(|&p| c.start.get() < p && p < c.end.get()),
            "{c:?} crosses a point"
        );
    }
    for &p in cuts.iter().filter(|&&p| p < end) {
        assert!(
            reference.contains_key(&p),
            "a cell starts on the point at frame {p}"
        );
    }
    // Every frame in a cell asks for that cell.
    for f in (0..end).step_by(997) {
        let c = chord_cell(&map, Frame(f), grid, R);
        assert_eq!(c, reference[&c.start.get()]);
        assert!(c.start.get() <= f && f < c.end.get());
        assert_eq!(c.start.get() % grid.get(), 0);
    }
}

/// A cell that starts on a point starts on the map's exact beat there, and
/// every ramp cell ends on the map's beat (to float rounding). A cell that ends
/// at a point ends on its own segment's line, within one frame of the map's beat.
///
/// Mutation (run): a cell's beat is read one frame late (`beat_on(start + 1)`); the cell on a
/// point no longer starts on the map's beat.
#[test]
fn cells_seat_on_the_map() {
    let map = mixed();
    let end = map.frame_at_or_after(Beat(16.0), R).get();
    let one_frame = 240.0 / 60.0 / R.get();
    for grid in [CellGrid::of(37), CellGrid::of(128), CellGrid::of(512)] {
        let cells = cells_seen(&map, grid, end, &[grid.get()]);
        for c in cells.values() {
            let at_end = c.beat_at(c.end, R).get();
            let want = map.beat_at_frame(c.end, R).get();
            assert!(
                (at_end - want).abs() < one_frame,
                "grid {grid:?} {c:?}: {at_end} vs {want}"
            );
            if map.tempo_at(c.beat).get() != c.tempo.get() {
                // A ramp cell: the chord lands on the map's beat unless the cell
                // ends on a point, where the next segment's beat differs a little.
                let on_a_point = map
                    .points()
                    .iter()
                    .any(|p| map.frame_at_or_after(p.beat, R) == c.end);
                if !on_a_point {
                    assert!(
                        (at_end - want).abs() < 1e-9,
                        "grid {grid:?} {c:?}: {at_end} vs {want}"
                    );
                }
            }
        }
        for p in &map.points()[1..] {
            let cut = map.frame_at_or_after(p.beat, R);
            if cut.get() >= end {
                continue;
            }
            let c = cells[&cut.get()];
            assert_eq!(
                c.beat,
                map.beat_at_frame(cut, R),
                "the cell on the point at {cut} starts on the map's beat"
            );
        }
    }
}

/// A step inside a grid cell cuts it: the cell before holds the old tempo to
/// the bit, the cell on the step the new one, the beat at the cut is the map's
/// own, and the cells after it are back on the grid multiples.
///
/// Mutation (run): the cell's end ignores the next point (`.min(u64::MAX)`); the cell
/// holding the step spans it.
#[test]
fn a_step_inside_a_grid_cell_cuts_it() {
    // 60 BPM is 1 beat/s: beat 10 is frame 480 000 exactly, and 479 744 is
    // the 512-multiple under it; the second map puts the step between frames.
    let on_frame =
        TempoMap::new([pt(0.0, 60.0, Shape::Step), pt(10.0, 240.0, Shape::Step)]).unwrap();
    let grid = CellGrid::of(512);
    let before = chord_cell(&on_frame, Frame(479_744), grid, R);
    assert_eq!((before.start, before.end), (Frame(479_744), Frame(480_000)));
    assert_eq!(before.tempo.get().to_bits(), 60.0f64.to_bits());
    let after = chord_cell(&on_frame, Frame(480_000), grid, R);
    assert_eq!((after.start, after.end), (Frame(480_000), Frame(480_256)));
    assert_eq!(after.tempo.get().to_bits(), 240.0f64.to_bits());
    assert_eq!(after.beat, Beat(10.0), "the beat at the cut");
    assert_eq!(
        on_frame.next_change(Frame(479_000), R),
        Some(Frame(480_000))
    );

    // Between frames: beat 10.00001 at 60 BPM is 480 000.48 frames.
    let off = TempoMap::new([
        pt(0.0, 60.0, Shape::Step),
        pt(10.000_01, 240.0, Shape::Step),
    ])
    .unwrap();
    let cut = off.next_change(Frame(479_000), R).unwrap();
    assert_eq!(cut, Frame(480_001));
    let before = chord_cell(&off, Frame(479_900), grid, R);
    assert_eq!((before.start, before.end), (Frame(479_744), cut));
    assert_eq!(before.tempo.get().to_bits(), 60.0f64.to_bits());
    let after = chord_cell(&off, cut, grid, R);
    assert_eq!(
        (after.start, after.end),
        (cut, Frame(480_256)),
        "back on the grid after the cut"
    );
    assert_eq!(after.tempo.get().to_bits(), 240.0f64.to_bits());
    assert_eq!(
        after.beat,
        off.beat_at_frame(cut, R),
        "the map's beat at the cut"
    );
    // The old line at the cut is 0.52 frames of 60 BPM short of the map's beat,
    // which has spent them at 240: under one frame of the faster tempo.
    let old_line = before.beat_at(cut, R).get();
    assert!(
        (old_line - 10.000_01 - 0.52 / R.get()).abs() < 1e-9,
        "{old_line}"
    );
    assert!((after.beat.get() - old_line).abs() < 4.0 / R.get());
    // The cell on the grid multiple next to the cut is whole again.
    let next = chord_cell(&off, Frame(480_256), grid, R);
    assert_eq!((next.start, next.end), (Frame(480_256), Frame(480_768)));
}

/// `next_change` is the first frame at or after the one given on which a point
/// starts; the first point is not a change.
///
/// Mutation (run): `next_change_with` never returns the frame it is given; "at or after"
/// fails on a point's own frame.
#[test]
fn next_change_finds_the_next_point() {
    let map = mixed();
    let cuts: Vec<Frame> = map.points()[1..]
        .iter()
        .map(|p| map.frame_at_or_after(p.beat, R))
        .collect();
    assert_eq!(map.next_change(Frame(0), R), Some(cuts[0]));
    assert_eq!(map.next_change(Frame(cuts[0].get() - 1), R), Some(cuts[0]));
    assert_eq!(map.next_change(cuts[0], R), Some(cuts[0]), "at or after");
    assert_eq!(map.next_change(Frame(cuts[0].get() + 1), R), Some(cuts[1]));
    assert_eq!(map.next_change(cuts[3], R), Some(cuts[3]));
    assert_eq!(map.next_change(Frame(cuts[3].get() + 1), R), None);
    let constant = TempoMap::constant(tutti_types::Bpm(120.0)).unwrap();
    assert_eq!(
        constant.next_change(Frame(0), R),
        None,
        "the first point is not a change"
    );
    assert_eq!(constant.next_change(Frame(5), R), None);
    // A hint changes nothing.
    let mut hint = Cursor::default();
    for f in (0..cuts[3].get() + 10).step_by(997) {
        assert_eq!(
            map.next_change_with(Frame(f), R, &mut hint),
            map.next_change(Frame(f), R)
        );
    }
}

/// The chord's worst error against the curve, in frames, by grid (every 7th
/// frame of the ramp): the ceilings are the measured worst figures with a
/// quarter of headroom (0.0399 frames for 60 -> 240 over 8 beats at 128).
///
/// Mutation (run): the chord tempo divides by `end - start + 1` frames; the error leaves every
/// ceiling.
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

/// A cell wholly inside a step segment has that segment's tempo to the bit, on
/// a constant map and on both sides of a change; a clock that restarts when the
/// tempo changes then restarts only at a real change.
///
/// Mutation (run): a step cell's tempo is `bpm0 * (1 + 1e-15)`; the bit equality fails.
#[test]
fn a_step_cell_has_the_segments_exact_tempo() {
    for bpm in [133.3, 120.0, 97.7, 61.3] {
        let map = TempoMap::constant(Bpm(bpm)).unwrap();
        for i in 0..100_000u64 {
            let c = chord_cell(&map, Frame(i * 128 + i % 97), CellGrid::of(128), R);
            assert_eq!(c.tempo.get().to_bits(), bpm.to_bits(), "{bpm} cell {i}");
        }
    }
    let map = TempoMap::new([
        pt(0.0, 133.3, Shape::Step),
        pt(20.000_01, 97.7, Shape::Step),
    ])
    .unwrap();
    let cut = map.next_change(Frame(0), R).unwrap().get();
    for f in (0..cut + 20_000).step_by(61) {
        let c = chord_cell(&map, Frame(f), CellGrid::of(128), R);
        let want = if f < cut { 133.3f64 } else { 97.7 };
        assert_eq!(c.tempo.get().to_bits(), want.to_bits(), "frame {f}");
    }
}
