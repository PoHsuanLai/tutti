use super::*;

/// Nothing before the first write; afterwards, the last write's value.
#[test]
fn a_read_returns_the_last_write() {
    let mut writer = SeqWriter::<[u64; 3]>::new();
    let cell = writer.reader();
    assert_eq!(cell.read(0), Ok(None));
    assert_eq!(writer.last(), None);
    writer.write([1, 2, 3]);
    assert_eq!(cell.read(0), Ok(Some([1, 2, 3])));
    writer.write([4, 5, 6]);
    assert_eq!(cell.read(0), Ok(Some([4, 5, 6])));
    assert_eq!(cell.read(0), Ok(Some([4, 5, 6])), "a read takes nothing");
    assert_eq!(writer.last(), Some([4, 5, 6]));
}

/// A `T` whose size is not a whole number of words round-trips every
/// byte, through a last word it fills only partly, and a one-byte and a
/// zero-sized `T` work too.
#[test]
fn a_value_short_of_a_whole_word_round_trips() {
    let value: [u8; 13] = std::array::from_fn(|i| 0xA0 | i as u8);
    let mut writer = SeqWriter::<[u8; 13]>::new();
    let reader = writer.reader();
    assert_eq!(reader.cell.len(), 1 + 2, "the sequence and two words");
    writer.write(value);
    assert_eq!(reader.read(0), Ok(Some(value)));
    // The padding is zero: the last word holds bytes 8..13 and nothing
    // else, so a stale tail never leaks between writes.
    let tail = reader.cell[2].load(Ordering::Relaxed).to_ne_bytes();
    assert_eq!(tail[..5], value[8..]);
    assert_eq!(tail[5..], [0; 3]);

    let mut byte = SeqWriter::<u8>::new();
    byte.write(0xFE);
    assert_eq!(byte.reader().read(0), Ok(Some(0xFE)));

    let mut unit = SeqWriter::<()>::new();
    assert_eq!(unit.reader().read(0), Ok(None));
    unit.write(());
    assert_eq!(unit.reader().read(0), Ok(Some(())));
}

/// A cell whose write never completes (a writer preempted mid-write)
/// is `Contended` after the retries, never "never written"; one never
/// written is `Ok(None)` however few retries the caller allows.
#[test]
fn a_read_tells_never_written_from_contended() {
    let mut writer = SeqWriter::<u64>::new();
    let reader = writer.reader();
    assert_eq!(reader.read(0), Ok(None));
    assert_eq!(reader.read(5), Ok(None));
    writer.write(7);
    // What a write leaves between its odd mark and its closing store.
    reader.cell[0].store(3, Ordering::Relaxed);
    assert_eq!(reader.read(0), Err(Contended));
    assert_eq!(reader.read(5), Err(Contended));
    reader.cell[0].store(2, Ordering::Relaxed);
    assert_eq!(reader.read(0), Ok(Some(7)), "settled again");
}

/// A concurrent reader never accepts a copy mixing two writes. Every
/// write stores one value in all elements, so a mix shows as unequal
/// elements. A stress run, not a proof: the proof is the loom model.
#[test]
#[cfg_attr(miri, ignore = "a threaded stress run; the loom model covers it")]
fn a_concurrent_reader_never_sees_a_mixed_copy() {
    use std::sync::atomic::AtomicBool;

    let mut writer = SeqWriter::<[u64; 8]>::new();
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let cell = writer.reader();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut mixed = 0usize;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(Some(w)) = cell.read(8)
                    && w.iter().any(|&x| x != w[0])
                {
                    mixed += 1;
                }
            }
            mixed
        })
    };
    for i in 1..=200_000u64 {
        writer.write([i; 8]);
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(reader.join().unwrap(), 0, "a reader accepted a mixed copy");
}
