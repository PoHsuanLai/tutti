//! `Bank` keeps the range-checked shape (SF2 2.04 section 7.2: a `wBank` is a
//! MIDI bank 0 to 127, or 128 for percussion).

use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_sf2::Bank;

/// Mutation (run): `raw > 127` -> `raw > 128` in `Bank::new` accepts 128 as a
/// melodic bank and 129 fails to be refused (the invalid row); `New` accepting
/// everything fails every invalid row.
#[test]
fn a_bank_keeps_the_range_contract() {
    check_range_contract(&RangeContract {
        new: Bank::new,
        get: Bank::get,
        of: None,
        valid: &[0, 1, 127, 128],
        invalid: &[129, 255, 256, u16::MAX],
    });
}
