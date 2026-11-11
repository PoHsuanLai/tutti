use super::*;

fn ranges(grid: Cadence, size: usize, unprimed: bool) -> Vec<Range<usize>> {
    grid.steps(0, size, unprimed).map(|s| s.frames).collect()
}

/// The steps tile the block with no gap and no overlap, each solved at its
/// last frame, and a block grid is one step.
#[test]
fn steps_tile_the_block_and_solve_at_their_last_frame() {
    let grid = Cadence::every(N16);
    assert_eq!(ranges(grid, 40, false), vec![0..16, 16..32, 32..40]);
    assert_eq!(ranges(grid, 1, false), vec![0..1]);
    assert_eq!(ranges(grid, 0, false), Vec::<Range<usize>>::new());
    assert_eq!(ranges(Cadence::BLOCK, 40, false), vec![0..40]);
    assert_eq!(ranges(Cadence::BLOCK, 0, false), Vec::<Range<usize>>::new());
    assert!(
        grid.steps(0, 40, false)
            .all(|s| s.solve_at == s.frames.end - 1 && s.grid_point)
    );
}

/// An unprimed block solves its first frame alone, then tiles the rest.
#[test]
fn an_unprimed_block_isolates_frame_zero() {
    let grid = Cadence::every(N16);
    assert_eq!(ranges(grid, 40, true), vec![0..1, 1..17, 17..33, 33..40]);
    assert_eq!(ranges(grid, 1, true), vec![0..1]);
    assert_eq!(ranges(grid, 0, true), Vec::<Range<usize>>::new());
    assert_eq!(ranges(Cadence::BLOCK, 40, true), vec![0..1, 1..40]);
}

/// A stream grid cuts the block at the stream's grid points, whatever the
/// block's own start, and only a step that begins on one is solved.
#[test]
fn a_stream_grid_is_cut_at_the_streams_grid_points() {
    let grid = Cadence::stream(N16);
    let cut = |position, size| {
        grid.steps(position, size, false)
            .map(|s| (s.frames, s.solve_at, s.grid_point))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        cut(0, 40),
        [(0..16, 0, true), (16..32, 16, true), (32..40, 32, true)]
    );
    assert_eq!(
        cut(5, 40),
        [(0..11, 0, false), (11..27, 11, true), (27..40, 27, true)]
    );
    assert_eq!(cut(32, 8), [(0..8, 0, true)]);
    assert_eq!(cut(40, 8), [(0..8, 0, false)]);
    assert_eq!(cut(47, 2), [(0..1, 0, false), (1..2, 1, true)]);
    assert!(cut(7, 0).is_empty());
    // Blocks of any length find the same grid points of the stream.
    for block in [1, 5, 17, 64] {
        let points: Vec<usize> = (0..96)
            .step_by(block)
            .flat_map(|p| {
                grid.steps(p as u64, block.min(96 - p), false)
                    .filter(|s| s.grid_point)
                    .map(move |s| p + s.frames.start)
            })
            .collect();
        assert_eq!(points, [0, 16, 32, 48, 64, 80], "blocks of {block}");
    }
}

const N16: NonZeroU32 = NonZeroU32::new(16).unwrap();

/// Only a fixed period on the stream clock is block independent.
#[test]
fn only_a_stream_grid_is_block_independent() {
    assert!(!Cadence::BLOCK.block_independent());
    assert!(!Cadence::every(N16).block_independent());
    assert!(Cadence::stream(N16).block_independent());
    let block_on_stream = Cadence {
        period: Period::Block,
        anchor: Anchor::Stream {
            offset: Samples::ZERO,
        },
    };
    assert!(!block_on_stream.block_independent());
}

/// The constructors state the period and the anchor the doc names.
#[test]
fn the_constructors_name_their_grid() {
    assert_eq!(Cadence::BLOCK.period(), Period::Block);
    assert_eq!(Cadence::every(N16).period(), Period::Frames(N16));
    assert_eq!(Cadence::every(N16).anchor(), Anchor::Block);
    assert_eq!(
        Cadence::stream(N16).anchor(),
        Anchor::Stream {
            offset: Samples::ZERO
        }
    );
}
