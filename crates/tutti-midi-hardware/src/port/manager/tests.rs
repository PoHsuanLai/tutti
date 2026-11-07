use super::*;
use tutti_ump::{MidiChannel, MidiGroup};

/// Test helper: collect one cycle's visits into a `Vec`, so the assertions
/// below can index and count without threading a closure through each.
fn drain_cycle(manager: &Arc<HardwareMidiInputs>, nframes: usize) -> Vec<(PortId, MidiEvent)> {
    let mut out = Vec::new();
    HardwareMidiDrain::new(Arc::clone(manager))
        .cycle_start_read_all_inputs(nframes, |port, event| out.push((port, event)));
    out
}

/// A drain of `manager`, as a node would hold.
fn drain_of(manager: &Arc<HardwareMidiInputs>) -> HardwareMidiDrain {
    HardwareMidiDrain::new(Arc::clone(manager))
}

fn now() -> Instant {
    Instant::now()
}

#[test]
fn test_create_ports() {
    let manager = Arc::new(HardwareMidiInputs::new(256, 44_100.0));

    let input_id = manager.open_input_port("Test Input").port_id();
    assert_eq!(input_id.index(), 0);

    let input_ports = manager.list_input_ports();
    assert_eq!(input_ports.len(), 1);
    let input_info = &input_ports[0];
    assert_eq!(input_info.name, "Test Input");
    assert!(input_info.active);
}

/// A multi-port sweep must not push either scratch buffer past its reserved
/// capacity — that push is a `realloc` inside the audio callback.
///
/// Four ports of 256 events each offer 1024, four times the cap.
#[test]
fn cycle_read_is_capped_across_ports() {
    let manager = Arc::new(HardwareMidiInputs::new(256, 44_100.0));
    for port in 0..4 {
        let mut handle = manager.open_input_port(format!("Input {port}"));
        for _ in 0..256 {
            assert!(handle.push(MidiEvent::noop(), now()));
        }
    }

    let events = drain_cycle(&manager, 512);
    assert_eq!(
        events.len(),
        CYCLE_SCRATCH_CAP,
        "one block drains at most the cap, however many ports are flooded"
    );

    // The remainder is deferred, not dropped: successive blocks drain it.
    let total: usize = (0..3).map(|_| drain_cycle(&manager, 512).len()).sum();
    assert_eq!(
        total + CYCLE_SCRATCH_CAP,
        1024,
        "every queued event is eventually delivered"
    );
}

#[test]
fn test_list_ports() {
    let manager = Arc::new(HardwareMidiInputs::new(256, 44_100.0));

    let id1 = manager.open_input_port("Input 1").port_id();
    let id2 = manager.open_input_port("Input 2").port_id();

    let inputs = manager.list_input_ports();
    assert_eq!(inputs.len(), 2);
    assert!(inputs.iter().any(|p| p.id == id1));
    assert!(inputs.iter().any(|p| p.id == id2));
}

#[test]
fn test_port_active_state() {
    let manager = Arc::new(HardwareMidiInputs::new(256, 44_100.0));

    let port_id = manager.open_input_port("Test").port_id();
    assert!(manager.is_port_active(port_id));

    manager.set_port_active(port_id, false);
    assert!(!manager.is_port_active(port_id));

    manager.set_port_active(port_id, true);
    assert!(manager.is_port_active(port_id));
}

/// A drained event is tagged with the port it came from.
///
/// Named for the seam rather than "input flow": `async_port` has a test of
/// the same shape one layer down, and two tests sharing a name make
/// `-E 'test(name)'` ambiguous. What is unique here is `events[0].0` — the
/// port attribution the single-port drain cannot assert.
#[test]
fn manager_tags_drained_events_with_their_port_id() {
    let manager = Arc::new(HardwareMidiInputs::new(256, 44_100.0));

    let mut producer_handle = manager.open_input_port("Input");
    let input_id = producer_handle.port_id();
    // Upconvert 7-bit 127 so the downconverted velocity_u7 round-trips.
    let event = MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        0x3C,
        tutti_midi_types::convert::midi1_velocity_to_midi2(0x7F),
    );
    assert!(producer_handle.push(event, now()));

    let events = drain_cycle(&manager, 512);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, input_id);
    assert!(events[0].1.is_note_on());
    assert_eq!(events[0].1.note(), Some(0x3C));
    assert_eq!(events[0].1.velocity_u7(), Some(0x7F));
}

#[test]
fn test_inactive_ports_ignored() {
    let manager = Arc::new(HardwareMidiInputs::new(256, 44_100.0));

    let mut producer_handle = manager.open_input_port("Input");
    let input_id = producer_handle.port_id();

    let event = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 0x3C, 0x7F);
    assert!(producer_handle.push(event, now()));

    manager.set_port_active(input_id, false);

    let events = drain_cycle(&manager, 512);
    assert_eq!(events.len(), 0);
}

#[test]
fn test_multiple_input_ports() {
    let manager = Arc::new(HardwareMidiInputs::new(256, 44_100.0));

    let mut handle1 = manager.open_input_port("Input 1");
    let mut handle2 = manager.open_input_port("Input 2");
    let (id1, id2) = (handle1.port_id(), handle2.port_id());

    handle1.push(
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100),
        now(),
    );
    handle2.push(
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 100),
        now(),
    );

    let events = drain_cycle(&manager, 512);
    assert_eq!(events.len(), 2);

    let port_ids: Vec<_> = events.iter().map(|(id, _)| *id).collect();
    assert!(port_ids.contains(&id1));
    assert!(port_ids.contains(&id2));
}

/// Concurrent creators each land their port: none is lost to another's
/// publish, and the indices handed back are exactly `0..n`, each once.
///
/// Hotplug can create ports from more than one control thread (a backend's
/// notification thread, a session's rescan). Reading the set, appending
/// and publishing is not one atomic step, so two creators that interleave
/// would each publish a copy missing the other's port.
///
/// Mutation: releasing `republish`'s `published` lock between the copy
/// and the publish (the read, append and publish then run unserialized)
/// fails this with fewer than 8 x 200 ports listed and repeated indices.
#[test]
#[cfg_attr(miri, ignore = "threaded stress test; too slow under miri")]
fn concurrent_creators_all_land() {
    use std::collections::BTreeSet;
    use std::sync::Barrier;

    const THREADS: usize = 8;
    const PER_THREAD: usize = 200;
    let manager = Arc::new(HardwareMidiInputs::new(4, 44_100.0));
    let start = Arc::new(Barrier::new(THREADS));

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let manager = Arc::clone(&manager);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                (0..PER_THREAD)
                    .map(|i| {
                        (
                            manager
                                .open_input_port(format!("{t}/{i}"))
                                .port_id()
                                .index(),
                            t,
                            i,
                        )
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let created: Vec<(usize, usize, usize)> = handles
        .into_iter()
        .flat_map(|h| h.join().expect("creator thread"))
        .collect();

    let ports = manager.list_input_ports();
    assert_eq!(
        ports.len(),
        THREADS * PER_THREAD,
        "a creator's port was lost"
    );
    let indices: BTreeSet<usize> = created.iter().map(|&(index, _, _)| index).collect();
    assert_eq!(
        indices,
        (0..THREADS * PER_THREAD).collect(),
        "every creator was handed its own index"
    );
    for (index, t, i) in created {
        assert_eq!(
            ports[index].name,
            format!("{t}/{i}"),
            "the index a creator was handed names its own port"
        );
    }
}

/// Push one event that arrived `age` before the block start, and read back
/// the `frame_offset` the conversion gives it.
///
/// Both instants are chosen here, so the answer is arithmetic rather than a
/// measurement: `nframes - age * sample_rate`, clamped into the block. That
/// is the whole reason `cycle_start_read_all_inputs_at` exists — with
/// `Instant::now()` inside the drain, the age is whatever the scheduler made
/// it, and the only assertion available is a band.
fn offset_for(sample_rate: f64, nframes: usize, age: core::time::Duration) -> u32 {
    let manager = Arc::new(HardwareMidiInputs::new(256, 44_100.0));
    manager.set_sample_rate(sample_rate);
    let mut handle = manager.open_input_port("Input");

    // A fixed block start, and an arrival a known age before it. Neither is
    // read from the clock during the conversion.
    let block_start = Instant::now();
    assert!(handle.push(
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100),
        block_start - age,
    ));

    let mut seen = Vec::new();
    drain_of(&manager).cycle_start_read_all_inputs_at(nframes, block_start, |_, e| seen.push(e));
    assert_eq!(seen.len(), 1, "exactly the event pushed");
    seen[0].frame_offset
}

/// A known age converts to exactly one frame offset.
///
/// 1 ms at 48 kHz is 48 samples, so an event that old at the start of a
/// 1024-frame block belongs at 1024 - 48 = 976. Not a band: with the block
/// start supplied there is no scheduling slack left in the answer, and the
/// previous version of this test could assert only `offset <= nframes` —
/// which every possible value of a `u32` clamped into the block satisfies.
///
/// Three rates, because the rate is a *multiplier* on the age: a conversion
/// that ignored it, or that used a hardcoded 44100, agrees with the truth at
/// one rate and disagrees at the others.
#[test]
fn a_known_age_converts_to_an_exact_frame_offset() {
    use core::time::Duration;

    for (rate, want) in [
        (48_000.0, 1024 - 48),
        (44_100.0, 1024 - 44), // 44.1 samples, truncated
        (96_000.0, 1024 - 96),
    ] {
        let got = offset_for(rate, 1024, Duration::from_millis(1));
        assert_eq!(
            got, want,
            "1 ms before a 1024-frame block at {rate} Hz is frame {want}, got {got}"
        );
    }
}

/// An event older than the whole block clamps to frame 0, not to a wrap.
///
/// This is the branch `Instant::now()` inside the drain made unreachable
/// from a test. `samples_ago` exceeding `nframes` makes the subtraction
/// saturate at 0; without the saturation it would wrap to ~4 billion and the
/// clamp below it would then pull it to `nframes - 1`, so an event from the
/// distant past would be scheduled at the *end* of the block instead of the
/// start — a stale note landing after everything that came later.
#[test]
fn an_event_older_than_the_block_clamps_to_its_start() {
    use core::time::Duration;

    // 100 ms at 48 kHz is 4800 samples, far past a 1024-frame block.
    let got = offset_for(48_000.0, 1024, Duration::from_millis(100));
    assert_eq!(
        got, 0,
        "an event 4800 samples old in a 1024-frame block belongs at its start, got {got}"
    );
}

/// A zero-age event lands at the last frame of the block, not past its end.
///
/// The other boundary. `nframes - 0` is `nframes`, which is one past the
/// last valid index, so the conversion pulls it back to `nframes - 1`. An
/// off-by-one here writes an event at an index no renderer will read, and it
/// is invisible to any assertion of the form `offset <= nframes`.
#[test]
fn an_event_arriving_at_the_block_start_lands_on_its_last_frame() {
    use core::time::Duration;

    let got = offset_for(48_000.0, 1024, Duration::ZERO);
    assert_eq!(
        got, 1023,
        "a zero-age event must sit at the last frame of a 1024-frame block, not at 1024"
    );
}

/// The rate the manager is *told* is the rate it converts against.
///
/// `HardwareMidiInputs` is constructed before any device exists, so it
/// starts at a placeholder 44100 and depends on `set_sample_rate` being
/// called once the device is open. This pins both halves: the placeholder's
/// exact wrong answer, and the corrected one.
///
/// Naming the wrong value matters. Without it, someone changing
/// `CycleScratch::new`'s default would silently move what the corrected test
/// is being compared against rather than failing anything.
#[test]
fn the_default_rate_mistimes_an_event_the_set_rate_times_correctly() {
    use core::time::Duration;

    let manager = Arc::new(HardwareMidiInputs::new(256, 44_100.0));
    let mut handle = manager.open_input_port("Input");
    let block_start = Instant::now();
    let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100);

    // Untouched: the 44100 placeholder. 1 ms is 44 samples.
    assert!(handle.push(note, block_start - Duration::from_millis(1)));
    let mut seen = Vec::new();
    drain_of(&manager).cycle_start_read_all_inputs_at(1024, block_start, |_, e| seen.push(e));
    assert_eq!(
        seen[0].frame_offset, 980,
        "the placeholder rate puts a 1 ms-old event at 1024 - 44"
    );

    // Told the truth: 1 ms is 48 samples, and the event moves 4 frames.
    manager.set_sample_rate(48_000.0);
    assert!(handle.push(note, block_start - Duration::from_millis(1)));
    let mut seen = Vec::new();
    drain_of(&manager).cycle_start_read_all_inputs_at(1024, block_start, |_, e| seen.push(e));
    assert_eq!(
        seen[0].frame_offset, 976,
        "at 48 kHz the same event belongs 4 frames earlier -- the 8.8% the \
         placeholder was off by"
    );
}

/// Hot-plug cycles reuse the slot a removed port left, so the set the
/// drain walks stays as large as what is connected.
///
/// An anchor port at index 1 stays plugged while a port below it (index 0)
/// and one above it (index 2) are each cycled 100 times, so both shapes of
/// reuse run: an interior slot, and the trailing one (trimmed, then
/// re-appended).
///
/// Mutations: `remove_input_port` only deactivating the port → the list
/// grows by one per cycle and fails the length check. `open_input_port`
/// always appending → the interior cycle lands at 2, 3, 4 … and fails the
/// index check and the slot bound.
#[test]
fn hot_plug_cycles_reuse_freed_slots() {
    let manager = Arc::new(HardwareMidiInputs::new(16, 44_100.0));
    let mut interior = manager.open_input_port("Interior").port_id();
    let anchor = manager.open_input_port("Anchor").port_id();
    assert_eq!((interior.index(), anchor.index()), (0, 1));

    let mut trailing_gone = None;
    for cycle in 0..100 {
        // An interior slot: index 0 is below the live anchor at 1.
        assert!(
            manager.remove_input_port(interior),
            "cycle {cycle}: the interior port is there"
        );
        let reused = manager.open_input_port("Interior").port_id();
        assert_eq!(reused.index(), 0, "cycle {cycle}: the freed slot is reused");
        assert_ne!(reused, interior, "cycle {cycle}: by a new port");
        interior = reused;

        // A trailing slot: index 2 is past every live port.
        let trailing = manager.open_input_port("Trailing").port_id();
        assert_eq!(trailing.index(), 2, "cycle {cycle}");
        assert!(manager.remove_input_port(trailing));
        trailing_gone = Some(trailing);

        assert_eq!(manager.list_input_ports().len(), 2, "cycle {cycle}");
        assert_eq!(
            manager.slot_count(),
            2,
            "cycle {cycle}: the set does not grow"
        );
    }
    let trailing_gone = trailing_gone.expect("the loop ran");
    assert!(
        !manager.remove_input_port(trailing_gone),
        "a removed port is gone"
    );
    assert!(manager.port_info(trailing_gone).is_none());
}

/// A removed port, and the ring its producer no longer holds, are freed by
/// the control side's publish: nothing is left for the audio thread.
///
/// Mutation: `remove_input_port` deactivating the port instead of taking
/// it out of the set → the published set keeps it alive and the weak
/// reference still upgrades.
#[test]
fn a_removed_port_is_freed_by_the_publish_that_removes_it() {
    let manager = Arc::new(HardwareMidiInputs::new(16, 44_100.0));
    let mut handle = manager.open_input_port("Gone");
    let id = handle.port_id();
    assert!(handle.push(MidiEvent::noop(), now()));
    let port = {
        let slots = manager.published_slots();
        Arc::downgrade(slots[0].as_ref().expect("the port is live"))
    };
    drop(handle);

    assert!(manager.remove_input_port(id));
    assert!(
        port.upgrade().is_none(),
        "no reader held the set, so the publish freed the port"
    );
    assert_eq!(drain_cycle(&manager, 512).len(), 0);
}

/// A producer that outlives its port, and the whole manager, still pushes
/// into memory it owns.
///
/// This is the shape that was a use-after-free: the handle was a raw
/// pointer into a ring the manager owned, so a driver thread pushing while
/// its session tore down wrote into freed memory. Now the handle owns its
/// half of the ring, so the ring lives exactly as long as someone holds it.
/// There is no mutation to record: the old shape cannot be rebuilt without
/// `unsafe`, and without it the compiler refuses a handle that borrows the
/// manager. Run it under miri to check the claim directly.
#[test]
fn a_producer_outliving_its_manager_pushes_into_its_own_ring() {
    let manager = Arc::new(HardwareMidiInputs::new(4, 44_100.0));
    let mut handle = manager.open_input_port("Orphan");
    assert!(manager.remove_input_port(handle.port_id()));
    drop(manager);

    for i in 0..4 {
        assert!(
            handle.push(MidiEvent::noop(), now()),
            "slot {i} of 4 is free"
        );
    }
    assert!(
        !handle.push(MidiEvent::noop(), now()),
        "nothing drains it, so it fills at its capacity"
    );
}

/// A second drain of the manager started from a drain's visitor hands out
/// nothing, and the outer drain still hands out every event, in order.
/// The visitor may also touch the rest of the manager (list, toggle, set
/// the rate). (The *same* drain cannot be re-entered at all: it is polled
/// through `&mut self`, which `HardwareMidiDrain`'s `compile_fail`
/// doctest pins.)
///
/// Ported from `RtEventBuf`'s `drain_each_callback_may_reborrow_the_buffer`.
///
/// Mutation (run): dropping the `draining` guard → the nested drains take
/// the events pushed during the visits, so `nested` is 4 and the next
/// block finds none → fails.
#[test]
fn a_drain_re_entered_from_its_visitor_hands_out_every_event() {
    let manager = Arc::new(HardwareMidiInputs::new(16, 44_100.0));
    let mut a = manager.open_input_port("A");
    let mut b = manager.open_input_port("B");
    for i in 0..2 {
        assert!(a.push(MidiEvent::noop(), now()));
        assert!(b.push(MidiEvent::noop(), now()), "event {i}");
    }

    let mut outer = Vec::new();
    let mut nested = 0;
    let mut nested_visits = 0;
    let visited = drain_of(&manager).cycle_start_read_all_inputs(512, |port, _| {
        outer.push(port);
        // Push one more so a nested drain would have something to take.
        assert!(a.push(MidiEvent::noop(), now()));
        nested += drain_of(&manager).cycle_start_read_all_inputs(512, |_, _| nested_visits += 1);
        assert_eq!(manager.list_input_ports().len(), 2);
        assert!(manager.set_port_active(b.port_id(), true));
        manager.set_sample_rate(48_000.0);
    });

    assert_eq!(visited, 4, "every event of the outer drain was visited");
    assert_eq!(
        outer,
        vec![a.port_id(), a.port_id(), b.port_id(), b.port_id()]
    );
    assert_eq!(nested, 0, "each nested drain returned 0");
    assert_eq!(nested_visits, 0, "and visited nothing");
    assert_eq!(
        drain_cycle(&manager, 512).len(),
        4,
        "what arrived during the visits waits for the next block"
    );
}

/// A stale `PortId` cannot reach the port that took its slot.
///
/// Port A is removed and port B created in the same slot. A's id must not
/// remove, toggle or describe B, and B's events carry B's id, not A's.
///
/// Mutation (run): `PortId`'s generation ignored (the lookup and the
/// removal compare `index` only, as keys did before) → A's id removes B →
/// fails.
#[test]
fn a_stale_port_id_cannot_reach_the_port_that_reused_its_slot() {
    let manager = Arc::new(HardwareMidiInputs::new(16, 44_100.0));
    let a = manager.open_input_port("A").port_id();
    assert!(manager.remove_input_port(a));
    let mut b = manager.open_input_port("B");
    let b_id = b.port_id();
    assert_eq!(b_id.index(), a.index(), "B reuses A's slot");
    assert_ne!(b_id, a);

    assert!(!manager.set_port_active(a, false));
    assert!(!manager.is_port_active(a));
    assert!(manager.port_info(a).is_none());
    assert!(!manager.remove_input_port(a), "a stale id removes nothing");

    let info = manager.port_info(b_id).expect("B is still there");
    assert_eq!((info.name.as_str(), info.active), ("B", true));
    assert!(b.push(MidiEvent::noop(), now()));
    let events = drain_cycle(&manager, 512);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, b_id, "B's event carries B's id");
}

/// The sample rate may be set from a control thread while the audio thread
/// drains: no shared borrow, no torn value, and the rate a drain uses is
/// one of those set.
///
/// Mutation (run): the rate back in an `AudioThreadCell` written with
/// `borrow_mut` (the old shape) → the two threads trip the cell's in-use
/// flag and the test panics (in a debug build when this was run; the flag
/// is checked in release builds too now).
#[test]
#[cfg_attr(miri, ignore = "threaded stress test; too slow under miri")]
fn the_sample_rate_can_be_set_while_a_drain_runs() {
    use core::time::Duration;
    use std::sync::atomic::AtomicBool;

    let manager = Arc::new(HardwareMidiInputs::new(64, 44_100.0));
    manager.set_sample_rate(48_000.0);
    let mut handle = manager.open_input_port("Input");
    let stop = Arc::new(AtomicBool::new(false));
    let setter = {
        let manager = Arc::clone(&manager);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut flip = false;
            while !stop.load(Ordering::Relaxed) {
                manager.set_sample_rate(if flip { 48_000.0 } else { 96_000.0 });
                flip = !flip;
            }
        })
    };

    let block_start = Instant::now();
    let mut drain = drain_of(&manager);
    for _ in 0..20_000 {
        assert!(handle.push(MidiEvent::noop(), block_start - Duration::from_millis(1)));
        drain.cycle_start_read_all_inputs_at(1024, block_start, |_, e| {
            // 1 ms at 48 kHz or at 96 kHz, never anything else.
            assert!(
                e.frame_offset == 1024 - 48 || e.frame_offset == 1024 - 96,
                "offset {} is from neither rate",
                e.frame_offset
            );
        });
    }
    stop.store(true, Ordering::Relaxed);
    setter.join().expect("setter thread");
}

/// A port removed while a drain holds the set is not freed by the drain;
/// `collect` frees it on the control side once the drain is done, with no
/// further change needed.
///
/// Mutation (run): `collect` not reclaiming (returning `retired_len` only)
/// → the port stays alive → fails.
#[test]
fn collect_frees_a_port_removed_during_a_drain() {
    let manager = Arc::new(HardwareMidiInputs::new(16, 44_100.0));
    let mut handle = manager.open_input_port("Gone");
    let id = handle.port_id();
    assert!(handle.push(MidiEvent::noop(), now()));
    drop(handle);
    let port = Arc::downgrade(
        manager.published_slots()[0]
            .as_ref()
            .expect("the port is live"),
    );

    drain_of(&manager).cycle_start_read_all_inputs(512, |_, _| {
        assert!(manager.remove_input_port(id));
    });
    assert!(
        port.upgrade().is_some(),
        "the drain held the set when the port was removed"
    );

    assert_eq!(manager.collect(), 0, "nothing is still held");
    assert!(port.upgrade().is_none(), "collect freed the port");
    assert_eq!(
        manager.collect(),
        0,
        "and a second collect has nothing to do"
    );
}

/// A drain sized for its block takes that many events in one cycle: four
/// ports of 256 events offer 1,024, and a drain reserved for 1,024 takes all
/// of them where the default takes 256.
///
/// Mutation: ignore `reserve`'s `events` (`want = CYCLE_SCRATCH_CAP`) → the
/// cycle takes 256 → fails.
#[test]
fn a_reserved_drain_takes_a_longer_blocks_events() {
    let manager = Arc::new(HardwareMidiInputs::new(256, 44_100.0));
    for port in 0..4 {
        let mut handle = manager.open_input_port(format!("Input {port}"));
        for _ in 0..256 {
            assert!(handle.push(MidiEvent::noop(), now()));
        }
    }
    let mut drain = drain_of(&manager);
    assert_eq!(drain.capacity(), CYCLE_SCRATCH_CAP);
    drain.reserve(1024);
    assert_eq!(drain.capacity(), 1024);
    let mut n = 0;
    drain.cycle_start_read_all_inputs(512, |_, _| n += 1);
    assert_eq!(n, 1024);
}

/// A `MidiIn` poll into a buffer smaller than what waits takes what the
/// buffer holds and leaves the rest in the rings: 30 events and a buffer of
/// 10 are three polls, and none is lost.
///
/// Before, the drain popped up to its own cap and dropped what the buffer had
/// no room for, uncounted.
///
/// Mutation: poll with `self.limit` instead of `buffer.len()` in
/// `poll_block` → the first poll takes 30, the buffer indexes past its end →
/// fails.
#[test]
fn a_poll_into_a_small_buffer_defers_and_drops_nothing() {
    use tutti_midi_types::MidiIn;
    let manager = Arc::new(HardwareMidiInputs::new(64, 44_100.0));
    let mut handle = manager.open_input_port("In");
    for i in 0..30u32 {
        assert!(handle.push(MidiEvent::noop().with_frame_offset(i), now()));
    }
    let mut drain = drain_of(&manager);
    let mut buffer = [MidiEvent::noop(); 10];
    let taken: Vec<usize> = (0..4).map(|_| drain.poll_block(512, &mut buffer)).collect();
    assert_eq!(taken, vec![10, 10, 10, 0]);
    assert_eq!(manager.dropped_events(), 0);
}

/// A driver pushing into a full ring has its events counted: per port, and
/// in the manager's total, which keeps a removed port's count.
///
/// Ring of 4: 10 pushes are 4 taken and 6 dropped.
///
/// Mutation: drop the `fetch_add` in `InputProducerHandle::push` → the
/// counts read 0 → fails. Mutation: leave `retired_dropped` alone in
/// `remove_input_port` → the total falls to 0 on removal → fails.
#[test]
fn a_full_ring_counts_what_it_drops() {
    let manager = Arc::new(HardwareMidiInputs::new(4, 44_100.0));
    let mut handle = manager.open_input_port("Busy");
    let id = handle.port_id();
    let taken = (0..10)
        .filter(|_| handle.push(MidiEvent::noop(), now()))
        .count();
    assert_eq!(taken, 4);
    assert_eq!(manager.port_info(id).map(|p| p.dropped), Some(6));
    assert_eq!(manager.dropped_events(), 6);
    assert!(manager.remove_input_port(id));
    assert_eq!(manager.dropped_events(), 6, "a removed port's drops stay");
}
