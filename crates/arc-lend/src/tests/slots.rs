//! Slot reuse, and what a slot left announcing pins.

use super::*;

use crate::slots::{Padded, Slot};

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

/// A slot is one padded unit, a cache line or two, so a cell with one reader
/// holds one such unit for its slots and no reader's slot shares a line with
/// another's.
///
/// Mutation: remove `repr(align)` from `Padded`; its alignment falls to a
/// word's and the first assertion fails.
#[test]
fn a_slot_has_its_own_cache_line() {
    assert!(std::mem::align_of::<Padded<Slot>>() >= 64);
    assert_eq!(
        std::mem::size_of::<Padded<Slot>>(),
        std::mem::align_of::<Padded<Slot>>(),
        "a slot fits in one aligned unit"
    );

    let cell = Arc::new(ArcLend::new(0u32));
    let _reader = cell.reader();
    assert_eq!(cell.slot_count(), 1);
}

/// The bytes of `value`, for a type without padding.
fn bytes_of<T>(value: &T) -> Vec<u8> {
    // SAFETY: `value` is a live reference, so its `size_of::<T>()` bytes are
    // readable; the caller checks the type has no padding to be uninitialized.
    unsafe {
        std::slice::from_raw_parts((value as *const T).cast::<u8>(), std::mem::size_of::<T>())
            .to_vec()
    }
}

/// A slot's words, as another thread would read them.
fn slot_state<T>(reader: &Reader<T>) -> (usize, bool, usize) {
    let slot = reader.slot();
    (
        slot.word.load(Ordering::Relaxed),
        slot.owned.load(Ordering::Relaxed),
        slot.generation.load(StdOrdering::Relaxed),
    )
}

/// A lend writes only its own slot: its lend counter is on the slot (which
/// has a line to itself), so the reader's own bytes and every other slot are
/// as they were. A counter in the `Reader` would make readers kept side by
/// side in a `Vec` share a line that every lend writes; one on the cell would
/// share it among all readers.
///
/// Mutations: give `Reader` a `generation: Cell<usize>` field that
/// `next_claim` bumps instead of the slot's; the reader grows by a word, and
/// its slot's counter would stay put. Make `next_claim` bump a static counter
/// instead; the slot's counter stays put.
#[test]
fn a_lend_writes_only_its_own_slot() {
    let cell = Arc::new(ArcLend::new(0u32));
    let mut readers: Vec<Reader<u32>> = (0..4).map(|_| cell.reader()).collect();
    // No padding, so reading the reader's bytes is sound.
    assert_eq!(
        std::mem::size_of::<Reader<u32>>(),
        2 * std::mem::size_of::<usize>()
    );
    let bytes: Vec<_> = readers.iter().map(bytes_of).collect();

    for round in 1..=3 {
        for i in 0..readers.len() {
            let before: Vec<_> = readers.iter().map(slot_state).collect();
            drop(readers[i].lend());
            let after: Vec<_> = readers.iter().map(slot_state).collect();
            for j in 0..readers.len() {
                if j == i {
                    assert_eq!(after[j], (FREE, true, round), "reader {i}'s own slot");
                } else {
                    assert_eq!(after[j], before[j], "reader {j}'s slot, lent by {i}");
                }
            }
        }
    }
    let after: Vec<_> = readers.iter().map(bytes_of).collect();
    assert_eq!(after, bytes, "a lend wrote into a `Reader`");
}
