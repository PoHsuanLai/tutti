use super::*;

use super::super::MIN_POINT_ROOM;

/// **A queued write that has no room is counted, not grown into**, and
/// reported with the other input-list drops
/// ([`points_dropped`](ParameterChangesImpl::points_dropped), which
/// the host processor's dropped-point total adds in): the queue of its
/// parameter is full, or the list has no queue left for a parameter the
/// block's automation does not name. One with room lands ahead of the
/// automation.
///
/// Mutation (run): `prepend_point` not counting on a failed landing → the
/// two counts stay 0 → fails; always counting → the landed write is
/// counted too → fails.
#[test]
fn a_prepended_point_without_room_is_counted_not_grown_into() {
    // Two queues of two points: one is full of automation for 7, one is free.
    let list = unpooled(2, 2);
    let mut changes = ParameterChanges::reserved(1, 2);
    changes.add_change(7u32.into(), 0, 0.1);
    changes.add_change(7u32.into(), 4, 0.2);
    let mut staging = list.begin();
    staging.refill(&changes).unwrap();

    staging.prepend(7, Normalized::of(0.9)).unwrap();
    assert_eq!(list.points_dropped(), 1, "the queue of 7 is full");
    staging.prepend(9, Normalized::of(0.4)).unwrap();
    assert_eq!(list.points_dropped(), 1, "the free queue took 9");
    staging.prepend(11, Normalized::of(0.5)).unwrap();
    assert_eq!(list.points_dropped(), 2, "no queue left for 11");
    drop(staging);

    let mut out = ParameterChanges::reserved(4, 4);
    assert!(list.try_fill_changes(&mut out));
    let points = |id: u32| -> Vec<(i32, f64)> {
        out.queues
            .iter()
            .find(|q| q.param_id == id.into())
            .map(|q| {
                q.points
                    .iter()
                    .map(|p| (p.sample_offset, p.value.get()))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(
        points(7),
        [(0, 0.1), (4, 0.2)],
        "the automation is untouched"
    );
    assert_eq!(points(9), [(0, 0.4)]);
    assert!(points(11).is_empty());
}

/// A list of `queues` queues of `points` each, with no pool: the
/// output list's layout.
fn unpooled(queues: usize, points: usize) -> ComWrapper<ParameterChangesImpl> {
    ParameterChangesImpl::with_queues(queues, points, PointPool::default())
}

/// The host's input list for `queues` queues at a chunk room of `room`,
/// as activation builds it (`instance.rs`).
fn input_list(queues: usize, room: usize) -> ComWrapper<ParameterChangesImpl> {
    let (floor, blocks) = ParameterChanges::point_layout(queues, room);
    ParameterChangesImpl::with_queues(queues, floor, PointPool::new(blocks, room + 1, queues))
}

/// Plugin threads reading the list at once all get it.
///
/// Mutation (run): `AudioThreadCell::try_acquire_reader` exclusive → the
/// COM reads answer 0 / null → fails.
#[test]
fn concurrent_reads_of_the_list_both_succeed() {
    use std::sync::Barrier;
    let list = unpooled(2, 4);
    list.begin().refill(&changes_with(2, 1)).unwrap();
    let (taken, done) = (Barrier::new(2), Barrier::new(2));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let read = list.inner.borrow();
            taken.wait();
            done.wait();
            assert_eq!((list.queues.len(), read.live), (2, 2));
        });
        taken.wait();
        let ptr = list.to_com_ptr::<IParameterChanges>().unwrap();
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        let count = unsafe { ptr.getParameterCount() };
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        let queue = unsafe { ptr.getParameterData(1) };
        done.wait();
        assert_eq!(count, 2, "a second reader must see the list");
        assert!(!queue.is_null());
    });
}

/// A read while the list is being added to answers with the method's
/// failure value instead of panicking into the plugin.
///
/// Mutation (run): `getParameterData` back on a panicking `borrow` → the panic
/// cannot unwind out of the COM thunk, and the test process aborts
/// (SIGABRT): the plugin-server crash this guards against.
#[test]
fn a_read_during_a_write_fails_at_the_com_level() {
    let list = unpooled(2, 4);
    list.begin().refill(&changes_with(1, 1)).unwrap();
    let ptr = list.to_com_ptr::<IParameterChanges>().unwrap();
    {
        let _adding = list.inner.borrow_mut();
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.getParameterCount() }, 0);
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert!(unsafe { ptr.getParameterData(0) }.is_null());
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert!(unsafe { ptr.addParameterData(&7, &mut 0) }.is_null());
    }
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert_eq!(unsafe { ptr.getParameterCount() }, 1);
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    assert!(!unsafe { ptr.getParameterData(0) }.is_null());
    assert_eq!(list.points_dropped(), 1, "the refused add is counted");
}

fn changes_with(num_queues: usize, points_per: usize) -> ParameterChanges {
    let mut changes = ParameterChanges::reserved(num_queues, points_per);
    for q in 0..num_queues {
        for p in 0..points_per {
            changes.add_change((q as u32).into(), p as i32, p as f64 * 0.01);
        }
    }
    changes
}

/// RT regression: once `refill_from_changes` has seen N queues,
/// subsequent refills at ≤ N queues must not allocate. This is the
/// steady-state automation path.
#[test]
fn refill_from_changes_steady_state_is_allocation_free() {
    let wrapper = unpooled(8, MIN_POINT_ROOM);
    let changes = changes_with(8, 4);

    // Grow the pool once (off the hot path).
    wrapper.begin().refill(&changes).unwrap();

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            wrapper.begin().refill(&changes).unwrap();
        }
    });
    assert_eq!(wrapper.len(), 8);
}

/// **A block with fewer queues than the last frees nothing, and the next
/// with more allocates nothing**: the host sends changes only, so the
/// queue count varies block to block, and a list sized for every
/// parameter must ride that out without touching the allocator. The
/// plugin sees exactly this block's queues.
///
/// Mutation (run): trimming the list to each block's count
/// (`queues.truncate(changes.queues.len())` in the refill) → the block that
/// has more queues finds them gone and has to grow them, inside the gate →
/// fails.
#[test]
fn a_varying_queue_count_never_touches_the_allocator() {
    use vst3::Steinberg::Vst::{IParameterChanges, IParameterChangesTrait};

    let wrapper = unpooled(200, MIN_POINT_ROOM);
    let ptr = wrapper.to_com_ptr::<IParameterChanges>().unwrap();
    let sizes = [200, 3, 0, 150, 1, 200, 17];
    let blocks: Vec<ParameterChanges> = sizes.iter().map(|&n| changes_with(n, 1)).collect();

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100 {
            for (changes, &n) in blocks.iter().zip(&sizes) {
                if changes.is_empty() {
                    drop(wrapper.begin());
                } else {
                    wrapper.begin().refill(changes).unwrap();
                }
                // SAFETY: the test calls a COM method on a live object it owns, with arguments
                // valid for the call.
                assert_eq!(unsafe { ptr.getParameterCount() }, n as i32);
            }
        }
    });
    assert_eq!(wrapper.pooled(), 200);
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    // A queue past the live count is not reachable by the plugin.
    assert!(unsafe { ptr.getParameterData(17) }.is_null());
}

/// **A list presized for the chunk's points refills without allocating
/// on first use**: every queue already has the room, so the first block
/// that carries 140 points on each of 50 parameters reserves nothing
/// (growing each queue past its 16 points would happen on the audio thread).
///
/// Mutation (run): `with_queues` ignoring `floor` (a room of
/// `MIN_POINT_ROOM`) → each queue keeps its latest value past that
/// room, not its 140 points → fails. (Reworded with `PointBuf`: the
/// first refill grew each queue inside the gate before.)
#[test]
fn a_presized_list_takes_its_first_dense_block_without_allocating() {
    let wrapper = unpooled(50, 140);
    let block = changes_with(50, 140);
    assert_no_alloc::assert_no_alloc(|| {
        wrapper.begin().refill(&block).unwrap();
    });
    assert_eq!(wrapper.len(), 50);
    assert!(wrapper.queues.iter().all(|q| q.stage().len() == 140));
}

/// The plugin's own `addParameterData` on the output list reuses the
/// pooled queues block after block: each block's queues start empty and
/// under the id the plugin asked for, and none is allocated.
///
/// Mutation (run): `addParameterData` without `reset` (a reused queue
/// keeps last block's points and id) → the point count and id fail.
#[test]
fn add_parameter_data_reuses_pooled_queues() {
    use vst3::Steinberg::Vst::{IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait};

    let changes = unpooled(4, MIN_POINT_ROOM);
    let ptr = changes.to_com_ptr::<IParameterChanges>().unwrap();
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let block = |id: u32| unsafe {
        drop(changes.begin());
        let mut index = -1;
        let q = ptr.addParameterData(&id, &mut index);
        let q = vst3::ComRef::from_raw(q).unwrap();
        let mut at = -1;
        assert_eq!(q.getPointCount(), 0);
        q.addPoint(0, 0.5, &mut at);
        assert_eq!(index, 0);
        assert_eq!(q.getParameterId(), id);
    };
    block(1);
    assert_no_alloc::assert_no_alloc(|| {
        for id in 2..100 {
            block(id);
        }
    });
    // Reserved: `fill_changes` fills within the set's room.
    let mut out = ParameterChanges::reserved(4, 1);
    assert!(changes.try_fill_changes(&mut out));
    assert_eq!(out.queues.len(), 1);
    assert_eq!(out.queues[0].points.len(), 1);
    assert_eq!(changes.pooled(), 4);
}

/// `clear_in_place` on an already-empty wrapper is allocation-free
/// (the hot-path case once the plugin isn't emitting output param
/// events). Exercised rather than the "had items, now empty" case
/// because the latter drops `ComWrapper` refcounts which allocate.
#[test]
fn clear_in_place_on_empty_is_allocation_free() {
    let wrapper = unpooled(0, 0);

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            drop(wrapper.begin());
        }
    });
    assert!(wrapper.is_empty());
}

/// A plugin that wants to write automation back calls `addParameterData`
/// on the host's list and then writes into the queue it is handed. The
/// vtable must therefore hand back a live queue *and* the index it was
/// filed under — a null pointer or a stale index is a plugin crash, not a
/// missed value.
#[test]
fn add_parameter_data_hands_the_plugin_a_live_queue_and_its_index() {
    use vst3::Steinberg::Vst::{IParameterChanges, IParameterChangesTrait};

    let changes = unpooled(1, MIN_POINT_ROOM);
    let ptr = changes.to_com_ptr::<IParameterChanges>().unwrap();

    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    unsafe {
        assert_eq!(ptr.getParameterCount(), 0);

        let param_id: u32 = 42;
        let mut index: i32 = -1;
        let queue_ptr = ptr.addParameterData(&param_id, &mut index);
        assert!(!queue_ptr.is_null(), "plugin must get a writable queue");
        assert_eq!(index, 0);
        assert_eq!(ptr.getParameterCount(), 1);

        assert!(
            !ptr.getParameterData(0).is_null(),
            "the queue must be retrievable at the index we reported"
        );
    }
}

/// **The plugin's emitted changes drain into the reserved output set
/// without allocating, past its pool too.** 1 000 parameters, the output
/// set reserved as activation reserves it (`MIN_POINT_ROOM` points a
/// queue, so pooled). Warmed with one pattern (parameters `0..256`
/// emitting a full room, every other one two points), then measured
/// with another inside the no-alloc gate (parameters `256..512` at a
/// full room, so no warm-up left them storage, and one more parameter
/// emitting a third point with the pool spent): nothing grows, that
/// parameter's third point takes its last point's place (it ends on its
/// latest value), and one point is counted.
///
/// Mutation (run): `ParameterChanges::push_point` dropping the point
/// past the room (`try_push` only, no `replace_latest`) → the point past
/// the floor is lost and the parameter ends on an earlier one → fails.
/// (Reworded with `PointBuf`: the unbounded path, which grew a floor
/// inside the gate, is gone.)
#[test]
fn emitted_changes_drain_into_the_reserved_set_without_allocating() {
    use vst3::Steinberg::Vst::{IParamValueQueueTrait, IParameterChanges};

    const PARAMS: u32 = 1_000;
    let room = MIN_POINT_ROOM;
    let pooled = ParameterChanges::POOLED_QUEUES as u32;
    let (floor, _) = ParameterChanges::point_layout(PARAMS as usize, room);
    let list = unpooled(PARAMS as usize, room);
    let ptr = list.to_com_ptr::<IParameterChanges>().unwrap();
    let mut out = ParameterChanges::reserved(PARAMS as usize, room);
    assert!(
        ParameterChanges::point_layout(PARAMS as usize, room).1 > 0,
        "pooled"
    );
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let emit = |moving: core::ops::Range<u32>, third: Option<u32>| unsafe {
        drop(list.begin());
        for id in 0..PARAMS {
            let n = if moving.contains(&id) {
                room
            } else if third == Some(id) {
                floor + 1
            } else {
                floor
            };
            let mut index = -1;
            let q = vst3::ComRef::from_raw(ptr.addParameterData(&id, &mut index)).unwrap();
            for k in 0..n {
                let mut at = -1;
                q.addPoint(k as i32, (k as f64) / 32.0, &mut at);
            }
        }
    };
    emit(0..pooled, None);
    assert!(list.try_fill_changes(&mut out));
    let storage = out.point_storage();
    assert_no_alloc::assert_no_alloc(|| {
        emit(pooled..2 * pooled, Some(2 * pooled));
        assert!(list.try_fill_changes(&mut out));
    });
    assert_eq!(out.point_storage(), storage);
    assert_eq!(out.points_overflowed(), 1);
    for q in &out.queues {
        let id = q.param_id.get();
        let want = if (pooled..2 * pooled).contains(&id) {
            room
        } else {
            floor
        };
        assert_eq!(q.points.len(), want, "parameter {id}");
    }
    let third = out.queue((2 * pooled).into()).expect("queued");
    let last = floor as i32;
    assert_eq!(third.points.last().map(|p| p.sample_offset), Some(last));
}

/// The host's chunk at the pool budget: `params` parameters with a room
/// of `room`, the first `POOLED_QUEUES` at full room and the rest at
/// their floor, the points arriving interleaved (frame by frame, every
/// parameter) into a pooled set built as the host's node builds it
/// (`reserved_bridged`), as the host's node and the wire's deserialize
/// hand it on. The parameters moving at full room are
/// `first..first + POOLED_QUEUES`. Point `k` of parameter `id` is on
/// frame `2k` with value `(id + k) % 5 / 8`.
fn pooled_chunk(params: u32, room: usize, first: u32) -> ParameterChanges {
    let mut changes = ParameterChanges::reserved_bridged(params as usize, room);
    let moving = first..first + ParameterChanges::POOLED_QUEUES as u32;
    for k in 0..room {
        for id in 0..params {
            if k >= ParameterChanges::FLOOR_POINTS && !moving.contains(&id) {
                continue;
            }
            changes.add_change(
                id.into(),
                2 * k as i32,
                ((id as usize + k) % 5) as f64 / 8.0,
            );
        }
    }
    changes
}

/// Each live queue's `(id, [(offset, value)])` as the plugin reads it.
fn plugin_view(list: &ComWrapper<ParameterChangesImpl>) -> Vec<(u32, Vec<(i32, f64)>)> {
    use vst3::Steinberg::Vst::{IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait};
    let ptr = list.to_com_ptr::<IParameterChanges>().unwrap();
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let n = unsafe { ptr.getParameterCount() };
    (0..n)
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        .map(|i| unsafe {
            let q = vst3::ComRef::from_raw(ptr.getParameterData(i)).unwrap();
            let points = (0..q.getPointCount())
                .map(|p| {
                    let (mut offset, mut value) = (0, 0.0);
                    assert_eq!(q.getPoint(p, &mut offset, &mut value), 0);
                    (offset, value)
                })
                .collect();
            (q.getParameterId(), points)
        })
        .collect()
}

/// **The host's input list holds the pool budget, not parameters ×
/// room**: for 2 000 and 16 384 parameters at 512- and 4096-frame blocks
/// (a chunk room of `chunk_room`, as activation builds it), a floor of
/// `FLOOR_POINTS + 1` per queue and `POOLED_QUEUES` blocks of
/// `room + 1`, under a third of a full room per queue.
///
/// Mutation (run): `with_queues` building every queue with the block's
/// room (`room + 1` for `floor`) → `queues × (room + 1)` and the pool
/// on top → fails.
#[test]
fn the_input_list_holds_the_pool_budget_not_params_times_room() {
    for params in [2_000usize, 16_384] {
        for frames in [512usize, 4096] {
            let room = tutti_plugin_types::ParameterQueue::chunk_room(frames);
            let list = input_list(params, room);
            let budget = ParameterChanges::point_budget(params, room);
            assert_eq!(
                budget,
                params * (ParameterChanges::FLOOR_POINTS + 1)
                    + ParameterChanges::POOLED_QUEUES * (room + 1)
            );
            assert_eq!(list.point_storage(), budget, "{params} at {frames}");
            assert!(3 * budget < params * (room + 1));
        }
    }
}

/// **A chunk the host's pool bounded reaches the plugin whole, carried
/// values and all.** 2 000 parameters at a 512-frame block, the input
/// list built as activation builds it. The host's chunk at its budget,
/// the parameters moving at full room the *last* `POOLED_QUEUES`, then a
/// skipped block's carried value prepended to every queue, as the bridge
/// does: every floor queue now carries three points and every moving one
/// its room and one more. The refill hands every point on, none counted:
/// the floor queues fit their own room, so the blocks go to the moving
/// ones however late in the list they are.
///
/// Mutation (run): the input list's floor without the carried value's
/// point (`floor - 1`, as it first was) → the first floor queues borrow
/// the blocks for their third point, and the moving parameters at the
/// end find the pool spent → points counted → fails.
#[test]
fn a_chunk_with_carried_values_refills_whole() {
    const PARAMS: u32 = 2_000;
    let room = tutti_plugin_types::ParameterQueue::chunk_room(512);
    let list = input_list(PARAMS as usize, room);
    let mut chunk = pooled_chunk(
        PARAMS,
        room,
        PARAMS - ParameterChanges::POOLED_QUEUES as u32,
    );
    for i in 0..PARAMS as usize {
        // As the bridge's `merge_carried` does: a real slot, nothing
        // grows.
        assert!(chunk.prepend_carried(
            i,
            tutti_plugin_types::ParameterPoint {
                sample_offset: 0,
                value: tutti_plugin_types::Normalized::of(0.25),
            },
        ));
    }
    assert_no_alloc::assert_no_alloc(|| list.begin().refill(&chunk).unwrap());
    assert_eq!(list.points_dropped(), 0);
    let view = plugin_view(&list);
    for (q, (id, points)) in chunk.queues.iter().zip(&view) {
        let want: Vec<(i32, f64)> = q
            .points
            .iter()
            .map(|p| (p.sample_offset, p.value.get()))
            .collect();
        assert_eq!(points, &want, "parameter {id}");
    }
}

/// **A chunk at the pool budget reaches the plugin per parameter and in
/// time order, without allocating, block after block; past it each
/// surplus point gives way to its parameter's latest, counted.** 2 000
/// parameters at a 512-frame block: the host's pooled chunk (its points
/// arrived interleaved) refills the pooled input list inside the no-alloc
/// gate, block after block, with a different 256 parameters moving in
/// alternate blocks and an empty block between (so every block's moving
/// parameters need the blocks the last one lent), and the plugin reads
/// every parameter's points, in order, through `getPoint`. Then one
/// more parameter carries two points past its floor of
/// `FLOOR_POINTS + 1` with the list's pool spent: it keeps that floor,
/// ending on its latest value, and one point is counted.
///
/// Mutations (run), each fails: the refill not borrowing a block (no
/// lend) → the moving parameters keep their floor, points counted;
/// `return_blocks` skipped in the refill (or in the refill and
/// `clear_in_place`) → the other 256 parameters find the pool spent,
/// points counted; `refill_from_queue` dropping past the room
/// (`try_push`) rather than keeping the latest value
/// (`push_or_replace_last`) → the over-budget parameter ends on its
/// earlier value. Not covered: `return_blocks` skipped in
/// `clear_in_place` alone, since every refill gives the blocks back
/// first, so a cleared list is never refilled without it.
#[test]
fn a_chunk_at_the_pool_budget_refills_per_parameter_without_allocating() {
    const PARAMS: u32 = 2_000;
    let room = tutti_plugin_types::ParameterQueue::chunk_room(512);
    let list = input_list(PARAMS as usize, room);
    let chunk = pooled_chunk(PARAMS, room, 0);
    let other = pooled_chunk(PARAMS, room, ParameterChanges::POOLED_QUEUES as u32);
    let empty = ParameterChanges::new();
    list.begin().refill(&chunk).unwrap();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..3 {
            list.begin().refill(&other).unwrap();
            list.begin().refill(&chunk).unwrap();
            drop(list.begin());
            list.begin().refill(&empty).unwrap();
            list.begin().refill(&other).unwrap();
            list.begin().refill(&empty).unwrap();
            list.begin().refill(&chunk).unwrap();
        }
    });
    assert_eq!(list.points_dropped(), 0);
    let view = plugin_view(&list);
    assert_eq!(view.len(), PARAMS as usize);
    for (q, (id, points)) in chunk.queues.iter().zip(&view) {
        assert_eq!(*id, q.param_id.get());
        let want: Vec<(i32, f64)> = q
            .points
            .iter()
            .map(|p| (p.sample_offset, p.value.get()))
            .collect();
        assert_eq!(points, &want, "parameter {id}");
        assert!(points.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    // A set the host's pool did not bound (built by hand): the last
    // parameter with two points past the list's floor.
    let mut over = ParameterChanges::new();
    for q in &chunk.queues {
        over.add_queue(q.clone());
    }
    let last = over.queues.last_mut().expect("a queue");
    let mut grown =
        tutti_plugin_types::ParameterQueue::with_capacity(last.param_id, last.len() + 2);
    for p in &last.points {
        assert!(grown.points.try_push(*p).is_ok());
    }
    assert!(grown.add_point(200, 0.25));
    assert!(grown.add_point(300, 0.875));
    *last = grown;
    assert_no_alloc::assert_no_alloc(|| list.begin().refill(&over).unwrap());
    assert_eq!(list.points_dropped(), 1);
    let (_, last) = plugin_view(&list).pop().expect("a queue");
    assert_eq!(last.len(), ParameterChanges::FLOOR_POINTS + 1);
    assert_eq!(last.last(), Some(&(300, 0.875)), "ends on its latest");
}

/// **A refill with more queues than the list's drops the queues past,
/// counted, and never grows the list.** A list of four queues refilled
/// from six of three points each, block after block inside the no-alloc
/// gate: the plugin sees the first four, whole, and each refill counts
/// the other two queues' six points.
///
/// Mutations (run), each fails: the refill growing the list past its
/// queues (pushing a queue, as it did) → the gate trips; the dropped
/// queues not counted → `points_dropped` is 0.
#[test]
fn a_refill_past_the_lists_queues_drops_them_counted() {
    let list = unpooled(4, MIN_POINT_ROOM);
    let changes = changes_with(6, 3);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..3 {
            list.begin().refill(&changes).unwrap();
        }
    });
    assert_eq!(list.len(), 4);
    assert_eq!(list.pooled(), 4);
    assert_eq!(list.points_dropped(), 3 * 6);
    let view = plugin_view(&list);
    assert_eq!(
        view.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    assert!(view.iter().all(|(_, points)| points.len() == 3));
}

/// **`addParameterData` answers as the SDK's host does, and past the
/// list's queues returns null, counted, never growing the list.** On a
/// two-queue list inside the no-alloc gate: two ids take the two queues
/// (indices 0 and 1), the first id asked again gets its own queue back
/// (index 0, as the SDK's `ParameterChanges` dedupes), and a third id
/// gets null with `index` untouched, counted once.
///
/// Mutations (run), each fails: past the queues pushing a new one (as
/// it did) → the gate trips; no lookup of a live queue for the id → the
/// repeated id takes the null; the refusal not counted →
/// `points_dropped` is 0.
#[test]
fn add_parameter_data_past_the_lists_queues_is_null_counted() {
    use vst3::Steinberg::Vst::{IParameterChanges, IParameterChangesTrait};
    let list = unpooled(2, MIN_POINT_ROOM);
    let ptr = list.to_com_ptr::<IParameterChanges>().unwrap();
    // SAFETY: the test calls a COM method on a live object it owns, with arguments valid for
    // the call.
    let add = |id: u32| unsafe {
        let mut index = -1;
        let q = ptr.addParameterData(&id, &mut index);
        (!q.is_null(), index)
    };
    let got = assert_no_alloc::assert_no_alloc(|| [add(7), add(9), add(7), add(11)]);
    assert_eq!(got, [(true, 0), (true, 1), (true, 0), (false, -1)]);
    assert_eq!(list.len(), 2);
    assert_eq!(list.pooled(), 2);
    assert_eq!(list.points_dropped(), 1);
}

/// **A plugin thread still inside a parameter queue or the list when the host
/// stages the next block is refused and counted, never a panic**: the host
/// reached the queues with panicking borrows, so a plugin worker still reading
/// a queue panicked the audio thread. Every staging call and the read of the
/// plugin's output answer `Busy` / `false` / `None` and add to
/// [`busy`](ParameterChangesImpl::busy).
///
/// Checked against the old code (the panicking borrows): the first refill
/// while the queue is held panics with "already borrowed" instead of
/// returning `Busy`.
#[test]
fn staging_while_a_plugin_holds_a_queue_or_the_list_is_refused_and_counted() {
    let list = unpooled(2, 4);
    list.begin().refill(&changes_with(2, 1)).unwrap();
    assert_eq!(list.busy(), 0);

    {
        let _plugin_in_getpoint = list.hold_queue_as_plugin(0);
        let mut staging = list.begin();
        assert_eq!(staging.refill(&changes_with(2, 1)), Err(Busy));
        assert_eq!(list.busy(), 1, "the refill is counted");
        assert_eq!(staging.prepend(99, Normalized::of(0.5)), Err(Busy));
        assert_eq!(list.busy(), 2, "so is the prepend");
    }
    // The plugin's output: a live queue the host reads while a plugin thread holds it.
    list.begin().refill(&changes_with(2, 1)).unwrap();
    {
        let _plugin_in_getpoint = list.hold_queue_as_plugin(0);
        let mut out = ParameterChanges::reserved(2, 4);
        assert!(!list.try_fill_changes(&mut out), "the read is refused");
        assert!(out.queues.is_empty(), "and leaves nothing half read");
    }
    assert_eq!(list.busy(), 3, "the refused read is counted");
    // Released: the next block stages whole.
    let mut staging = list.begin();
    assert_eq!(staging.refill(&changes_with(2, 1)), Ok(()));
    drop(staging);
    assert_eq!(list.busy(), 3, "and counts nothing");

    let _plugin_in_getparameterdata = list.inner.borrow();
    assert!(list.try_begin().is_none(), "a held list is not staged");
    assert_eq!(list.busy(), 4);
}

/// A queue that lent a block and is held by a plugin thread keeps the block
/// lent: the list is not begun, and once released the block comes back.
#[test]
fn a_held_queue_keeps_its_lent_block_until_released() {
    const PARAMS: u32 = 2_000;
    let room = tutti_plugin_types::ParameterQueue::chunk_room(512);
    let list = input_list(PARAMS as usize, room);
    let storage = list.point_storage();
    let chunk = pooled_chunk(PARAMS, room, 0);
    list.begin().refill(&chunk).unwrap();
    {
        let _held = list.hold_queue_as_plugin(0);
        assert!(list.try_begin().is_none());
        assert_eq!(list.busy(), 1);
    }
    drop(list.begin());
    assert_eq!(list.point_storage(), storage, "no block was lost");
    list.begin().refill(&chunk).unwrap();
    assert_eq!(list.points_dropped(), 0, "the block lends again");
}
