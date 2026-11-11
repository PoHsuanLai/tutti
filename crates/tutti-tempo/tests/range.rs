//! `CellGrid` keeps the range-checked contract (`CLAUDE.md`, "Range-checked
//! values").
#![cfg(feature = "serde")]

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_tempo::{CellGrid, CellGridError};

#[test]
fn a_grid_is_never_zero() {
    check_range_contract(&RangeContract {
        new: CellGrid::new,
        get: CellGrid::get,
        of: Some(CellGrid::of),
        valid: &[1, 64, 128, 512, u64::MAX],
        invalid: &[0],
    });
    assert_eq!(CellGrid::try_from(0), Err(CellGridError(0)));
    assert!(CellGridError(0).to_string().contains('0'));
}
