use super::*;

/// **The ring holds exactly `capacity` values**: the next push is refused and
/// counted, and nothing already queued is overwritten.
///
/// Mutation (run): the full check `head - tail >= capacity` as `>` -> the
/// third push of a two-slot ring is accepted over the first value, and the
/// test fails at the `Err(RingFull)` assertion.
#[test]
fn a_full_ring_refuses_and_keeps_what_it_holds() {
    let (mut writer, mut reader) = edit_ring::<[u32; 2]>(2);
    assert_eq!(writer.push([1, 2]), Ok(()));
    assert_eq!(writer.push([3, 4]), Ok(()));
    assert_eq!(writer.push([5, 6]), Err(RingFull));
    assert_eq!(writer.refused(), 1);
    assert_eq!(reader.refused(), 1);
    assert_eq!(reader.pending(), 2);
    assert_eq!(reader.pop(), Some([1, 2]));
    assert_eq!(reader.pop(), Some([3, 4]));
    assert_eq!(reader.pop(), None);
}

/// **Values come out in order and whole across many laps**, for a value of
/// two and a half words (the partial last word is padded on the way in and
/// trimmed on the way out) and for one smaller than a word.
///
/// Mutation (run): the slot index `count % capacity` as
/// `count % (capacity + 1)` -> a lap reads a slot the writer has not filled
/// for that count, and the test fails on an out-of-order value.
#[test]
fn values_wrap_around_in_order_and_whole() {
    let (mut writer, mut reader) = edit_ring::<[u32; 5]>(3);
    let mut next = 0u32;
    let mut expect = 0u32;
    for lap in 0..20 {
        for _ in 0..(lap % 3) + 1 {
            writer.push([next; 5]).unwrap();
            next += 1;
        }
        while let Some(value) = reader.pop() {
            assert_eq!(value, [expect; 5]);
            expect += 1;
        }
    }
    assert_eq!(expect, next);

    let (mut writer, mut reader) = edit_ring::<u8>(2);
    for byte in 0..=255u8 {
        writer.push(byte).unwrap();
        assert_eq!(reader.pop(), Some(byte));
    }
}

/// **A ring needs room for one value.**
///
/// Mutation (run): the `capacity > 0` assert removed -> the constructor
/// returns a ring that refuses everything, nothing panics, and the test
/// fails.
#[test]
#[should_panic(expected = "an edit ring holds at least one value")]
fn a_ring_of_no_room_is_refused() {
    let _ = edit_ring::<u32>(0);
}

/// **A producer thread and a consumer thread hand every value over once, in
/// order**, through the slot accesses that miri checks for aliasing and
/// data races.
///
/// Mutation: the writer's `Release` store of `head` as `Relaxed` should make
/// miri report a data race on a slot word; not run here (no nightly), CI's
/// miri job is the first run. The loom model in `tests/edit_ring_loom.rs`
/// holds the same mutation.
#[test]
fn two_threads_hand_over_every_value_in_order() {
    let count: u64 = if cfg!(miri) { 64 } else { 20_000 };
    let (mut writer, mut reader) = edit_ring::<[u64; 3]>(4);
    let producer = std::thread::spawn(move || {
        for n in 0..count {
            while writer.push([n, !n, n ^ 0x55]).is_err() {
                std::thread::yield_now();
            }
        }
    });
    let mut expect = 0;
    let mut take = |reader: &mut EditReader<[u64; 3]>| {
        let value = reader.pop()?;
        assert_eq!(value, [expect, !expect, expect ^ 0x55]);
        expect += 1;
        Some(())
    };
    // `is_finished` orders nothing, so an empty pop after it does not mean
    // the ring is drained: it only ends the wait (a producer that panicked
    // cannot leave this loop spinning). The producer is joined, which does
    // synchronise, and the rest is drained after.
    while !producer.is_finished() {
        if take(&mut reader).is_none() {
            std::thread::yield_now();
        }
    }
    producer.join().unwrap();
    while take(&mut reader).is_some() {}
    assert_eq!(expect, count, "every value arrived");
    assert_eq!(reader.pop(), None);
}
