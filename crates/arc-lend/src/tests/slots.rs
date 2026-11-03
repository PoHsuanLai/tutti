//! Slot reuse, and what a slot left announcing pins.

use super::*;

/// The list doubles when every slot is owned, starting from one: a cell
/// with one reader pays for one slot, and `n` readers registered at once
/// hold `n.next_power_of_two()`.
///
/// Mutations: make `register` grow by one slot (`let grow = 1;`); the count
/// after three readers is 3, not 4. Make it grow by sixteen
/// (`let grow = 16;`); the count after one reader is 16. In `Slots::slot`,
/// map every slot of a block to its first (`Some(k) => (k as usize + 1, 0)`);
/// the slots that alias an owned one look owned, so registration skips them
/// and grows the list early: the count after four readers is 8. (A bad
/// mapping cannot make two readers share a slot: `register` takes only a
/// slot whose `owned` it read as false. That readers lend through distinct
/// slots across blocks is `parked_loans_pin_one_value_per_reader`'s.)
#[test]
fn the_list_doubles_from_one_slot() {
    let cell = Arc::new(ArcLend::new(0u32));
    let mut readers = Vec::new();
    for n in 1..=40usize {
        readers.push(cell.reader());
        assert_eq!(cell.slot_count(), n.next_power_of_two(), "{n} readers");
    }
}

/// Slots of dropped readers are reused, so the slot list stays at the
/// high-water mark of readers registered at once, and doubles past it. A
/// clone is a registration of its own.
///
/// Mutation: make `Reader::drop` do nothing. The re-registrations then take
/// new slots and the count after them is 32, not 16.
#[test]
fn slots_are_reused_and_the_list_keeps_its_high_water_mark() {
    let cell = Arc::new(ArcLend::new(0u32));
    let first: Vec<_> = (0..16).map(|_| cell.reader()).collect();
    assert_eq!(cell.slot_count(), 16);
    drop(first);
    for _ in 0..3 {
        let again: Vec<_> = (0..16).map(|_| cell.reader()).collect();
        assert_eq!(cell.slot_count(), 16, "the dropped readers' slots");
        drop(again);
    }

    // Past it, the list doubles, and every reader lends its own slot.
    let one = cell.reader();
    let mut many: Vec<_> = (0..16).map(|_| one.clone()).collect();
    assert_eq!(cell.slot_count(), 32);
    let loans: Vec<_> = many.iter_mut().map(Reader::lend).collect();
    cell.publish(Arc::new(1));
    assert!(loans.iter().all(|l| **l == 0));
    assert_eq!(cell.retired_len(), 1);
    drop(loans);
    drop((one, many));
    assert_eq!(cell.reclaim(), 0);
    assert_eq!(cell.slot_count(), 32, "the high-water mark stays");
}

/// A loan passed to `mem::forget` leaves its announcement in the slot, which
/// pins that one value until the reader lends again or its slot is handed to
/// a new reader. It never pins more.
///
/// Mutation: delete `slot.word.store(FREE, ..)` in `Slots::take`. The
/// forgotten announcement then survives the slot's re-registration and the
/// last `reclaim` reports 1.
#[test]
fn a_forgotten_loan_pins_one_value_until_its_slot_is_reused() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));

    let mut reader = cell.reader();
    std::mem::forget(reader.lend());
    for id in 1..=10 {
        cell.publish(make(id));
    }
    assert_eq!(cell.retired_len(), 1, "only the forgotten value");
    assert_eq!(freed(&sites), (1..10).collect::<Vec<_>>());

    // The reader lends again: its slot moves on, and the value is released.
    assert_eq!(reader.lend().id, 10);
    assert_eq!(cell.reclaim(), 0);

    // Forgotten again, then the reader is dropped and its slot reused.
    std::mem::forget(reader.lend());
    cell.publish(make(11));
    drop(reader);
    assert_eq!(cell.reclaim(), 1, "the slot still announces 10");
    // Registering as many readers as there are slots hands every slot on,
    // that one included.
    let slots = cell.slot_count();
    let _next: Vec<_> = (0..slots).map(|_| cell.reader()).collect();
    assert_eq!(cell.slot_count(), slots);
    assert_eq!(cell.reclaim(), 0, "a new reader starts from a free slot");
}
