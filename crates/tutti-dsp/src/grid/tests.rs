use super::*;

#[test]
fn counts_enumerate_their_indices() {
    assert_eq!(
        FrameCount(3).indices().collect::<Vec<_>>(),
        vec![FrameIndex(0), FrameIndex(1), FrameIndex(2)]
    );
    assert_eq!(BinCount(0).indices().count(), 0);
}

/// The transposition these types exist to prevent. A frame index and a bin
/// index are both "some usize" today, so `at(bin, frame)` compiles.
#[test]
fn frame_and_bin_coordinates_are_distinct_types() {
    let frame = FrameIndex(2);
    let bin = BinIndex(2);
    // Same underlying number, different meaning — and no `PartialEq`
    // between them, so a swap cannot pass unnoticed.
    assert_eq!(frame.get(), bin.get());
}

fn grid() -> Grid<i32> {
    // 3 frames x 4 bins, values encode (frame, bin) as frame*10 + bin.
    let data = (0..3)
        .flat_map(|f| (0..4).map(move |b| f * 10 + b))
        .collect();
    Grid::new(data, FrameCount(3), BinCount(4)).unwrap()
}

/// The invariant private fields exist to hold.
#[test]
fn a_length_that_disagrees_with_the_shape_is_rejected() {
    assert_eq!(
        Grid::new(vec![0u8; 10], FrameCount(3), BinCount(4)),
        Err(Error::GridShapeMismatch {
            len: 10,
            rows: 3,
            cols: 4,
        })
    );
    assert!(Grid::new(vec![0u8; 12], FrameCount(3), BinCount(4)).is_ok());
    assert!(Grid::<u8>::new(Vec::new(), FrameCount(0), BinCount(4)).is_ok());
}

#[test]
fn indexing_is_row_major_by_frame_then_bin() {
    let g = grid();
    assert_eq!(*g.at(FrameIndex(0), BinIndex(0)), 0);
    assert_eq!(*g.at(FrameIndex(2), BinIndex(3)), 23);
    assert_eq!(g.row(FrameIndex(1)), &[10, 11, 12, 13]);
    // Flat layout is part of the contract: consumers index it directly.
    assert_eq!(g.as_slice()[4 + 2], 12);
}

#[test]
fn get_bounds_checks_both_axes_independently() {
    let g = grid();
    assert_eq!(g.get(FrameIndex(2), BinIndex(3)), Some(&23));
    assert_eq!(g.get(FrameIndex(3), BinIndex(0)), None, "frame past end");
    assert_eq!(g.get(FrameIndex(0), BinIndex(4)), None, "bin past end");
    // Without the per-axis check a flat bounds test would accept this:
    // frame 1, bin 4 flattens to offset 8, which is in range but wrong.
    assert_eq!(g.get(FrameIndex(1), BinIndex(4)), None);
}

/// `at` must panic on an out-of-axis bin rather than return a neighbour's
/// value. The offset `1 * 4 + 4 == 8` is inside the backing buffer, so a
/// flat bounds check accepts it and hands back frame 2's first value.
#[test]
#[should_panic(expected = "outside 3x4")]
fn at_panics_on_a_bin_past_the_end_of_its_frame() {
    let g = grid();
    let _ = g.at(FrameIndex(1), BinIndex(4));
}

#[test]
#[should_panic(expected = "outside 3x4")]
fn at_panics_on_a_frame_past_the_end() {
    let g = grid();
    let _ = g.at(FrameIndex(3), BinIndex(0));
}

/// A zero-bin grid still reports its frames, and `rows` agrees.
#[test]
fn rows_agrees_with_the_frame_count_even_with_no_bins() {
    let g: Grid<f32> = Grid::new(Vec::new(), FrameCount(5), BinCount(0)).unwrap();
    assert_eq!(g.frames(), FrameCount(5));
    assert_eq!(g.rows().count(), 5, "rows must not disagree with frames()");
    assert!(g.rows().all(|r| r.is_empty()));
}

#[test]
fn rows_enumerate_every_frame() {
    let g = grid();
    let rows: Vec<_> = g.rows().collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2], &[20, 21, 22, 23]);
}

#[test]
fn mutation_goes_through_the_same_coordinates() {
    let mut g = grid();
    *g.get_mut(FrameIndex(1), BinIndex(2)).unwrap() = 99;
    assert_eq!(*g.at(FrameIndex(1), BinIndex(2)), 99);

    g.row_mut(FrameIndex(0)).fill(7);
    assert_eq!(g.row(FrameIndex(0)), &[7, 7, 7, 7]);
}

#[test]
fn shape_agreement_is_checkable_before_elementwise_work() {
    let g = grid();
    let same = Grid::filled(0i32, FrameCount(3), BinCount(4));
    let different = Grid::filled(0i32, FrameCount(4), BinCount(3));

    assert!(g.same_shape_as(&same));
    assert!(!g.same_shape_as(&different), "same length, different shape");
}
