//! `Bank` keeps the range-checked shape (SF2 2.04 section 7.2: a `wBank` is a
//! MIDI bank 0 to 127, or 128 for percussion).

use tutti_fixtures::{RangeContract, check_range_contract, check_range_serde};
use tutti_sf2::Bank;

/// A bank is 0 to 127 or 128, and nothing else.
/// Spec: sf2.pdta.phdr.bank-program.range
#[test]
fn a_bank_keeps_the_range_contract() {
    let c = RangeContract {
        new: Bank::new,
        get: Bank::get,
        of: None,
        valid: &[0, 1, 127, 128],
        invalid: &[129, 255, 256, u16::MAX],
    };
    check_range_contract(&c);
    check_range_serde(&c);
}
