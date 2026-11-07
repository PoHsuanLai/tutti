//! Regression gates: what the audio thread runs of each primitive neither
//! allocates nor frees. Everything is built outside the gate, then driven
//! inside `assert_no_alloc`; a primitive that runs on several threads runs
//! every participant inside it, helpers included.

use assert_no_alloc::AllocDisabler;
use tutti_rt::{AudioThreadCell, RtVec};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// `RtVec` reached through `&self`, as `tutti-midi-hardware`'s drain holds it:
/// behind an `AudioThreadCell`.
///
/// Mutation (run): an uncapped `Vec` in place of the `ArrayVec` -> the first
/// block pushes past the warm-up's capacity, grows the `Vec` inside the gate,
/// and aborts.
#[test]
fn rt_vec_steady_state_is_allocation_free() {
    let buf: AudioThreadCell<RtVec<(u32, u32), 64>> = AudioThreadCell::new(RtVec::new());
    // Warm up well within the inline capacity.
    buf.borrow_mut().extend((0..32).map(|i| (32 - i, i)));

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            let mut v = buf.borrow_mut();
            v.clear();
            for i in 0..96u32 {
                let _ = v.push((96 - i, i));
            }
            v.as_mut_slice().sort_by_key(|&(k, _)| k);
            let sum: u32 = v.iter().map(|&(_, x)| x).sum();
            core::hint::black_box(sum);
        }
    });
    assert_eq!(buf.borrow().len(), 64);
    assert!(buf.borrow().is_overflowed());
}

/// Every `AudioThreadCell` borrow path — `borrow_mut`, `borrow`, granted
/// and refused `try_borrow_mut` and `try_borrow`, two shared borrows at once,
/// and each guard's drop — neither allocates nor frees: the always-on check
/// is a compare-exchange and a store or decrement.
///
/// The panic path is left out: it formats a message, and it is a contract
/// violation, not a path the audio thread runs.
///
/// Mutations (run): box the result in `try_acquire_writer` (a
/// `Box::new(self.state.compare_exchange(..))`, unboxed for the test) → the
/// first borrow allocates inside the gate, and `assert_no_alloc` aborts; the
/// same in `try_acquire_reader` (`Ok(_) => return *Box::new(true)`) →
/// aborts at the first `try_borrow`.
#[test]
fn audio_thread_cell_borrows_neither_allocate_nor_free() {
    let cell = AudioThreadCell::new(0u64);
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..10_000u64 {
            *cell.borrow_mut() += i;
            let held = cell.try_borrow_mut().expect("nothing else borrows");
            assert!(cell.try_borrow_mut().is_none());
            assert!(cell.try_borrow().is_none());
            drop(held);
            let a = cell.try_borrow().expect("no writer");
            let b = cell.borrow();
            assert!(cell.try_borrow_mut().is_none());
            core::hint::black_box(*a + *b);
        }
    });
    assert_eq!(*cell.borrow(), (0..10_000u64).sum::<u64>());
}

/// `RequestWord::raise` and `take`, and an `edit_ring` push and pop (full and
/// empty answers included), neither allocate nor free: the whole of what a
/// host callback and the block-head drain do.
///
/// Mutations (run): box the value in `EditWriter::push` (a `Box::new(value)`
/// kept alive across the store) → the gate aborts on the first push; a
/// `vec!` in `RequestBits::iter` → it aborts.
#[test]
fn request_word_and_edit_ring_are_allocation_free() {
    use tutti_rt::{RequestBit, RequestWord, edit_ring};

    let word = RequestWord::new();
    let (mut writer, mut reader) = edit_ring::<[u32; 3]>(4);
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..1_000u32 {
            word.raise(RequestBit::of((round % 32) as u8));
            let taken = word.take();
            assert_eq!(taken.iter().count(), 1);
            for n in 0..6 {
                let _ = writer.push([round, n, 0]);
            }
            while let Some(edit) = reader.pop() {
                core::hint::black_box(edit);
            }
            assert!(reader.pop().is_none());
        }
    });
    assert!(writer.refused() > 0);
}

/// A reader's whole block on a `PosRing`: `claim`, the window, `holds`,
/// `frame`, `sample`, `get` and `idle`, neither allocating nor freeing. The
/// writer's `push` is the disk thread's and stays outside the claim.
///
/// Mutations (run): a `Box::new` at the top of `PosReader::claim` -> the first
/// claim aborts; the same in `PosReader::idle` -> aborts at the first idle.
#[test]
fn pos_ring_reader_is_allocation_free() {
    use tutti_rt::PosRing;

    let (mut writer, mut reader) = PosRing::new(64, 2, 8);
    let data: Vec<f32> = (0..2 * 32).map(|i| i as f32).collect();
    assert_eq!(writer.push(&data), 32);
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..1_000u64 {
            let play = round % 8;
            let claim = reader.claim(play, [(play, play + 8), (0, 0)]);
            assert!(claim.holds(play));
            assert!(claim.window().holds(play));
            let mut sum = 0.0;
            for s in play..play + 8 {
                sum += claim.sample(s, 0) + claim.frame(s).get(1);
            }
            let expect: f32 = (play..play + 8).map(|s| (4 * s + 1) as f32).sum();
            assert_eq!(sum, expect);
            if round % 4 == 0 {
                reader.idle();
            }
        }
    });
}

/// `SeqWriter::write` and `last`, and `SeqReader::read` (a value and the
/// never-written answer), neither allocate nor free.
///
/// Mutation (run): a `Box::new` at the top of `SeqWriter::write` -> aborts at
/// the first write.
#[test]
fn seq_cell_write_and_read_are_allocation_free() {
    use tutti_rt::{SeqReader, SeqWriter};

    let mut writer: SeqWriter<[u32; 3]> = SeqWriter::new();
    let reader: SeqReader<[u32; 3]> = writer.reader();
    assert_no_alloc::assert_no_alloc(|| {
        assert_eq!(reader.read(4), Ok(None));
        for round in 0..1_000u32 {
            writer.write([round, round + 1, round + 2]);
            assert_eq!(writer.last(), Some([round, round + 1, round + 2]));
            assert_eq!(reader.read(4), Ok(Some([round, round + 1, round + 2])));
        }
    });
}

/// `PackedAtomic::store` and `load` neither allocate nor free.
///
/// Mutation (run): a `Box::new` at the top of `PackedAtomic::store` -> aborts.
#[test]
fn packed_atomic_store_and_load_are_allocation_free() {
    use tutti_rt::PackedAtomic;

    let cell = PackedAtomic::new([0.0f32, 1.0]);
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..1_000u32 {
            let pair = [round as f32, -(round as f32)];
            cell.store(pair);
            assert_eq!(cell.load(), pair);
        }
    });
}

/// A block's claims on a `SplitMut` and a `SplitRw`: a claim, a refused
/// one, a run, a view's reads, writes and `split`, and their releases,
/// neither allocate nor free. The table and the held list are sized outside;
/// each block builds its split inside, as the executor does.
///
/// Mutations (run): a `Box::new` at the top of `SplitMut::try_claim` -> aborts
/// at the first claim; the same at the top of `RwView::split` -> aborts at the
/// first split.
#[test]
fn split_claims_and_views_are_allocation_free() {
    use tutti_rt::{ClaimTable, Held, SplitMut, SplitRw};

    let mut items = vec![0u32; 16];
    let mut chunks = vec![0u32; 32];
    let mut table = ClaimTable::new(16);
    let held = Held::new(8);
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..1_000u32 {
            let split = SplitMut::new(&mut items, &mut table);
            {
                let mut first = split.claim(0);
                *first += 1;
                assert!(split.try_claim(0).is_err(), "claimed twice");
                let mut run = split.claim_run(1..5);
                run[0] += 1;
            }
            assert_eq!(*split.claim(0), round + 1);

            let rw = SplitRw::new(&mut chunks, 4, &mut table);
            let mut view = rw.view(&held);
            view.get_mut(1)[0] = round;
            assert_eq!(view.get(1)[0], round);
            assert!(view.try_get(2).is_ok());
            let mut seen = 0;
            view.split(
                &[(2, false), (3, true)],
                |write| write,
                |_, read| seen += read.len(),
                |_, write| write[0] = round,
            );
            assert_eq!(seen, 4);
            assert_eq!(view.held(), 3);
        }
    });
}

/// `TaskGraph::begin` and every participant's `work`, the caller's and two
/// helpers', neither allocate nor free; each thread runs its share inside the
/// gate.
///
/// Mutation (run): a `Box::new` at the top of `TaskGraph::push` -> the first
/// participant that readies a second task aborts.
#[test]
fn task_graph_blocks_are_allocation_free() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use tutti_rt::{BlockWaits, Participant, TaskGraph};

    // 0 -> {1, 2, 3, 4} -> 5.
    let mut graph = TaskGraph::new(
        &[0, 1, 1, 1, 1, 4],
        &[0, 4, 5, 6, 7, 8, 8],
        &[1, 2, 3, 4, 5, 5, 5, 5],
    );
    let ran: Vec<AtomicU32> = (0..6).map(|_| AtomicU32::new(0)).collect();
    for _ in 0..300 {
        assert_no_alloc::assert_no_alloc(|| graph.begin());
        std::thread::scope(|s| {
            let graph = &graph;
            let ran = &ran;
            for helper in 1..3 {
                s.spawn(move || {
                    assert_no_alloc::assert_no_alloc(|| {
                        graph.work(Participant::Helper(helper), BlockWaits::DEFAULT, |t| {
                            ran[t as usize].fetch_add(1, Ordering::Relaxed);
                        });
                    });
                });
            }
            assert_no_alloc::assert_no_alloc(|| {
                graph.work(Participant::Caller, BlockWaits::DEFAULT, |t| {
                    ran[t as usize].fetch_add(1, Ordering::Relaxed);
                });
            });
        });
        assert!(graph.done());
    }
    for (task, count) in ran.iter().enumerate() {
        assert_eq!(count.load(Ordering::Relaxed), 300, "task {task}");
    }
}

/// `JobGate::run` and two helpers' `help` neither allocate nor free, every
/// thread inside the gate. The job holds participant 0 until both helpers
/// have taken a share of the round, so the helper path is what runs.
///
/// Mutation (run): a `Box::new` in `JobGate::help` before the job runs -> the
/// first helper that joins aborts.
#[test]
fn job_gate_run_and_help_are_allocation_free() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use tutti_rt::JobGate;

    let gate = JobGate::new(3);
    let stop = AtomicBool::new(false);
    let joined = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(|| {
                assert_no_alloc::assert_no_alloc(|| {
                    let mut served = 0;
                    while !stop.load(Ordering::Acquire) {
                        gate.help(&mut served);
                        std::hint::spin_loop();
                    }
                });
            });
        }
        assert_no_alloc::assert_no_alloc(|| {
            for round in 1..=200u32 {
                let joined_all = gate.run(
                    &|i| {
                        joined[i].store(round, Ordering::Release);
                        if i == 0 {
                            while joined[1].load(Ordering::Acquire) != round
                                || joined[2].load(Ordering::Acquire) != round
                            {
                                std::hint::spin_loop();
                            }
                        }
                    },
                    || {},
                );
                assert!(joined_all);
            }
        });
        stop.store(true, Ordering::Release);
    });
}

/// `RtScope::enter`, `is_inside`, `check_outside` and the guard's drop
/// neither allocate nor free.
///
/// Mutation (run): a `Box::new` in `RtScope::enter` -> aborts.
#[test]
fn rt_scope_is_allocation_free() {
    use tutti_rt::RtScope;

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            RtScope::check_outside("a value");
            let guard = RtScope::enter();
            assert!(RtScope::is_inside());
            drop(guard);
            assert!(!RtScope::is_inside());
        }
    });
}

/// `with_no_denormals` neither allocates nor frees, and returns its
/// closure's value.
///
/// Mutation (run): a `Box::new` in `with_no_denormals` -> aborts.
#[test]
fn with_no_denormals_is_allocation_free() {
    use tutti_rt::with_no_denormals;

    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..1_000u32 {
            assert_eq!(with_no_denormals(|| round + 1), round + 1);
        }
    });
}
