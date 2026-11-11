//! A `loom` model of [`level_cell`]: the shipped code, whose atomics, fence
//! and yield are loom's under the flag (see `src/level_cell.rs`).
//!
//! ```text
//! cargo test -p tutti-rt --release --test level_cell_loom   # with the loom cfg flag set
//! ```
//!
//! A writer thread folds three blocks (frames 1, 2 and 4, each with a peak
//! and an energy equal to its frames) while the main thread takes twice, then
//! once more after the join. Over every interleaving it asserts:
//!
//! - **a block lands whole in one take**: each take's peak, energy and frames
//!   agree (frames 0 has no peak and no energy; the peak is the largest block
//!   in the take, and the energy equals the frames);
//! - **none lost, none twice**: the takes' frames sum to 7 and their blocks
//!   are disjoint (each take's frames is a sum of distinct blocks, and the
//!   union is all three).

#![cfg(loom)]

use loom::thread;
use tutti_rt::{LevelSpan, level_cell};

fn check_whole(span: &LevelSpan) {
    // Frames are 1, 2, 4: a sum identifies its blocks.
    assert_eq!(span.sum_sq, span.frames as f64, "energy and frames agree");
    let largest = match span.frames {
        0 => 0.0,
        1 => 1.0,
        2 | 3 => 2.0,
        _ => 4.0,
    };
    assert_eq!(span.peak, largest, "peak and frames agree: {span:?}");
}

#[test]
fn a_block_lands_whole_in_one_take() {
    loom::model(|| {
        let (writer, mut reader) = level_cell();
        let w = thread::spawn(move || {
            for frames in [1u32, 2, 4] {
                writer.fold(frames as f32, f64::from(frames), frames);
            }
        });
        let mut spans = vec![reader.take(), reader.take()];
        w.join().unwrap();
        spans.push(reader.take());
        let mut total = 0;
        for span in &spans {
            check_whole(span);
            // Disjoint blocks: no frame count is taken twice.
            assert_eq!(total & span.frames, 0, "a block was taken twice");
            total |= span.frames;
        }
        assert_eq!(total, 7, "a block was lost: {spans:?}");
    });
}
