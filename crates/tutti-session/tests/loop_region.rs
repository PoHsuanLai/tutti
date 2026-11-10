//! The loop region a host edits, and the form it reads back.
//!
//! The vocabulary half of `bevy-tutti`'s `prelude_surface`
//! `the_loop_region_round_trips_through_the_prelude` (the prelude half, that
//! every type is nameable from it, stayed there). An edit is a scheduled
//! `LoopEdit`; the region is the engine's, so a read shows an edit only once
//! a block has applied it. `LoopRange::new` is where an inverted brace is
//! refused.
//!
//! Mutation (run): `LoopRegion::with` keeping the old `armed` whatever the
//! edit says → the disarm leaves `bounds()` right but the armed assertions
//! fail.

use tutti_transport::{LoopEdit, LoopRegion};
use tutti_types::Beat;
use tutti_types::LoopRange;

#[test]
fn the_loop_region_round_trips() {
    let range: LoopRange =
        LoopRange::new(Beat::new(4.0), Beat::new(8.0)).expect("4..8 is a usable region");
    assert_eq!(range.start().get(), 4.0);
    assert_eq!(range.end().get(), 8.0);
    assert!(
        LoopRange::new(Beat::new(8.0), Beat::new(4.0)).is_none(),
        "an inverted brace is not a loop"
    );

    let region = LoopRegion::default().with(LoopEdit::over(range));
    assert_eq!(region.active(), Some(range));
    let disarmed = region.with(LoopEdit::armed(false));
    assert_eq!(disarmed.bounds(), range, "the bounds are kept");
    assert_eq!(disarmed.active(), None, "but nothing is looping");
    assert!(!disarmed.is_armed());
}
