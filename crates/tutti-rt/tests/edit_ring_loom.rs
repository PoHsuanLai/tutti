//! A `loom` model of [`edit_ring`]: the shipped code, whose atomics and slot
//! cells are loom's under the flag (see `src/edits.rs`).
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p tutti-rt --release --test edit_ring_loom
//! ```
//!
//! The ring holds `[u32; 3]` (twelve bytes: two words, the second half used);
//! each value's elements are its number, so a torn slot shows as unequal
//! elements. The producer pushes 1, 2, 3, retrying a refused push once after a
//! yield, and records the values whose push returned `Ok`. The consumer pops,
//! and after the producer joins drains the rest. Over every interleaving:
//!
//! - **no accepted value is lost, none repeated, order kept**: the values
//!   popped equal the values accepted, in order;
//! - **no torn slot**: every popped value has equal elements;
//! - **refusals are counted**: the writer's count equals the pushes that
//!   returned `RingFull`;
//! - **slot hand-over is ordered**: the slots are loom `UnsafeCell`s, so a
//!   write the consumer's read is not ordered after, or a write that races the
//!   consumer still reading, is a causality violation.
//!
//! Two rings: capacity 2, and capacity 1, where every value reuses the one
//! slot.

#![cfg(loom)]

use loom::thread;
use tutti_rt::edit_ring;

fn run(capacity: usize) {
    loom::model(move || {
        let (mut writer, mut reader) = edit_ring::<[u32; 3]>(capacity);

        let producer = thread::spawn(move || {
            let mut accepted = Vec::new();
            let mut refused = 0;
            for n in 1..=3u32 {
                let mut tries = 0;
                while tries < 2 {
                    match writer.push([n; 3]) {
                        Ok(()) => {
                            accepted.push(n);
                            break;
                        }
                        Err(_) => {
                            refused += 1;
                            tries += 1;
                            thread::yield_now();
                        }
                    }
                }
            }
            (accepted, refused, writer.refused())
        });

        let mut popped = Vec::new();
        let take = |reader: &mut tutti_rt::EditReader<[u32; 3]>, popped: &mut Vec<u32>| {
            if let Some([a, b, c]) = reader.pop() {
                assert!(a == b && b == c, "a torn slot: {:?}", [a, b, c]);
                popped.push(a);
                true
            } else {
                false
            }
        };
        for _ in 0..6 {
            if !take(&mut reader, &mut popped) {
                thread::yield_now();
            }
        }

        let (accepted, refused, counted) = producer.join().unwrap();
        // Bounded by what was accepted (plus one pop that must find nothing), so
        // a repeated value fails the equality below rather than loom's branch
        // limit.
        for _ in 0..=accepted.len() {
            if !take(&mut reader, &mut popped) {
                break;
            }
        }
        assert_eq!(popped, accepted, "popped values differ from accepted");
        assert_eq!(refused, counted, "refusals miscounted");
    });
}

#[test]
fn every_accepted_edit_is_applied_once_in_order() {
    run(2);
}

#[test]
fn a_single_slot_is_reused_in_order() {
    run(1);
}
