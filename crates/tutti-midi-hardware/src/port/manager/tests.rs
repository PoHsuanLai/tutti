use super::*;
use crate::testing::{RATE, Rig, ports};
use std::time::{Duration, Instant};
use tutti_fixtures::midi::{noop, note_on};
use tutti_midi_runtime::DeviceInlets;
use tutti_ump::{MidiChannel, MidiGroup, Ump};

fn now() -> Instant {
    Instant::now()
}

fn note(n: u8) -> Ump {
    note_on(MidiGroup::FIRST, MidiChannel::FIRST, n, 0x8000)
}

/// A rig over `slots` inlets with `lead`, whose clock has read once (frame 0 at
/// `origin`), so an arrival can be placed.
fn rig(slots: usize, capacity: usize, lead: Duration) -> (Arc<HardwareMidiInputs>, Rig) {
    let inlets = Arc::new(DeviceInlets::new(slots, capacity, lead));
    let manager = Arc::new(HardwareMidiInputs::new(Arc::clone(&inlets)));
    let mut rig = Rig::over(&inlets, now());
    rig.run(1);
    (manager, rig)
}

#[test]
fn test_create_ports() {
    let manager = ports(4);

    let input_id = manager.open_input_port("Test Input").unwrap().port_id();
    assert_eq!(input_id.index(), 0);

    let input_ports = manager.list_input_ports();
    assert_eq!(input_ports.len(), 1);
    let input_info = &input_ports[0];
    assert_eq!(input_info.name, "Test Input");
    assert!(input_info.active);
}

#[test]
fn test_list_ports() {
    let manager = ports(4);

    let id1 = manager.open_input_port("Input 1").unwrap().port_id();
    let id2 = manager.open_input_port("Input 2").unwrap().port_id();

    let inputs = manager.list_input_ports();
    assert_eq!(inputs.len(), 2);
    assert!(inputs.iter().any(|p| p.id == id1));
    assert!(inputs.iter().any(|p| p.id == id2));
}

#[test]
fn test_port_active_state() {
    let manager = ports(4);

    let port_id = manager.open_input_port("Test").unwrap().port_id();
    assert!(manager.is_port_active(port_id));

    manager.set_port_active(port_id, false);
    assert!(!manager.is_port_active(port_id));

    manager.set_port_active(port_id, true);
    assert!(manager.is_port_active(port_id));
}

/// **A port is a slot of the inlets: when every slot is held, the next open
/// is refused and makes no port**, and a slot a removal frees is taken by the
/// next.
#[test]
fn a_port_beyond_the_slots_is_refused_and_a_freed_slot_is_reused() {
    let manager = ports(2);
    let first = manager.open_input_port("A").unwrap();
    let _second = manager.open_input_port("B").unwrap();
    assert_eq!(
        manager.open_input_port("C").unwrap_err(),
        NoFreeSlot { slots: 2 }
    );
    assert_eq!(
        manager.list_input_ports().len(),
        2,
        "the refused made no port"
    );

    assert!(manager.remove_input_port(first.port_id()));
    assert!(
        manager.open_input_port("C").is_ok(),
        "the freed slot is taken"
    );
}

/// **An event reaches the graph on its frame**: pushed for an arrival 130
/// frames after the clock's reading, it is the graph's at frame 130 of the
/// slot its port holds, and the second port's on the other.
#[test]
fn a_ports_events_reach_the_graph_on_their_frames_and_slots() {
    let (manager, mut rig) = rig(2, 64, Duration::ZERO);
    let mut a = manager.open_input_port("A").unwrap();
    let mut b = manager.open_input_port("B").unwrap();
    rig.apply();
    assert!(a.push(note(60), rig.time_of(130)));
    assert!(b.push(note(64), rig.time_of(200)));
    rig.run(4);
    assert_eq!(rig.take(), vec![(0, 130, note(60)), (1, 200, note(64))]);
}

/// **The spacing of a device's events is the spacing of their arrivals**
/// (doc 017 section 5, "Timing at the edge"), whatever the block boundaries:
/// arrivals at 130, 200 and 333 frames after the reading sound at those
/// frames plus the lead, the lead being the same for every event.
#[test]
fn arrivals_keep_their_spacing_and_the_lead_is_added_to_each() {
    let lead = Duration::from_secs_f64(100.0 / RATE);
    let (manager, mut rig) = rig(1, 64, lead);
    let mut port = manager.open_input_port("A").unwrap();
    rig.apply();
    for (n, frame) in [(60u8, 130u64), (61, 200), (62, 333)] {
        assert!(port.push(note(n), rig.time_of(frame)));
    }
    rig.run(8);
    let frames: Vec<u64> = rig.take().iter().map(|e| e.1).collect();
    assert_eq!(frames, vec![230, 300, 433]);
}

/// **An event that arrives after its frame has begun is late, not lost**: it
/// sounds at the first frame of the next block and is counted
/// ([`InletCounts::late`](tutti_graph::InletCounts::late)).
#[test]
fn an_arrival_past_its_block_is_emitted_late_and_counted() {
    let (manager, mut rig) = rig(1, 64, Duration::ZERO);
    let mut port = manager.open_input_port("A").unwrap();
    rig.apply();
    rig.run(3); // with the block `rig` ran, frames 0..256 have been rendered
    assert!(port.push(note(60), rig.time_of(10)));
    rig.run(1);
    assert_eq!(
        rig.take(),
        vec![(0, 256, note(60))],
        "the next block's first frame"
    );
    let info = manager.port_info(port.port_id()).unwrap();
    assert_eq!((info.late, info.dropped), (1, 0), "late, not lost");
}

/// **An inactive port discards what its driver pushes**: nothing reaches the
/// graph while it is paused, nothing arrives in a burst when it is active again,
/// and a discard is not a loss.
#[test]
fn an_inactive_port_discards_its_events() {
    let (manager, mut rig) = rig(1, 64, Duration::ZERO);
    let mut port = manager.open_input_port("A").unwrap();
    let id = port.port_id();
    rig.apply();

    manager.set_port_active(id, false);
    assert!(port.push(note(60), rig.time_of(100)), "discarded, not lost");
    manager.set_port_active(id, true);
    assert!(port.push(note(61), rig.time_of(120)));
    rig.run(4);
    assert_eq!(rig.take(), vec![(0, 120, note(61))]);
    assert_eq!(manager.dropped_events(), 0);
}

/// Concurrent creators each land their port: none is lost to another's open,
/// and the indices handed back are exactly `0..n`, each once.
///
/// Hotplug can create ports from more than one control thread (a backend's
/// notification thread, a session's rescan); the list and the inlets' slots
/// are taken as one step.
#[test]
#[cfg_attr(miri, ignore = "threaded stress test; too slow under miri")]
fn concurrent_creators_all_land() {
    use std::collections::BTreeSet;
    use std::sync::Barrier;

    const THREADS: usize = 8;
    const PER_THREAD: usize = 50;
    let manager = ports(THREADS * PER_THREAD);
    let start = Arc::new(Barrier::new(THREADS));

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let manager = Arc::clone(&manager);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                (0..PER_THREAD)
                    .map(|i| {
                        let handle = manager.open_input_port(format!("{t}/{i}")).unwrap();
                        (handle.port_id().index(), t, i, handle)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let created: Vec<_> = handles
        .into_iter()
        .flat_map(|h| h.join().expect("creator thread"))
        .collect();

    let listed = manager.list_input_ports();
    assert_eq!(
        listed.len(),
        THREADS * PER_THREAD,
        "a creator's port was lost"
    );
    let indices: BTreeSet<usize> = created.iter().map(|c| c.0).collect();
    assert_eq!(
        indices,
        (0..THREADS * PER_THREAD).collect(),
        "every creator was handed its own index"
    );
    for (index, t, i, _) in &created {
        assert_eq!(
            listed[*index].name,
            format!("{t}/{i}"),
            "the index a creator was handed names its own port"
        );
    }
}

/// Hot-plugging reuses freed indices rather than growing the list: an
/// interior index (below a live port) and a trailing one (past every live
/// port) are both reused, and the list shrinks back when its last port goes.
#[test]
fn hot_plug_cycles_reuse_freed_slots() {
    let manager = ports(3);
    let mut interior = manager.open_input_port("Interior").unwrap().port_id();
    let anchor = manager.open_input_port("Anchor").unwrap().port_id();
    assert_eq!((interior.index(), anchor.index()), (0, 1));

    let mut trailing_gone = None;
    for cycle in 0..100 {
        assert!(
            manager.remove_input_port(interior),
            "cycle {cycle}: the interior port is there"
        );
        let reused = manager.open_input_port("Interior").unwrap().port_id();
        assert_eq!(
            reused.index(),
            0,
            "cycle {cycle}: the freed index is reused"
        );
        assert_ne!(reused, interior, "cycle {cycle}: by a new port");
        interior = reused;

        let trailing = manager.open_input_port("Trailing").unwrap().port_id();
        assert_eq!(trailing.index(), 2, "cycle {cycle}");
        assert!(manager.remove_input_port(trailing));
        trailing_gone = Some(trailing);

        assert_eq!(manager.list_input_ports().len(), 2, "cycle {cycle}");
        assert_eq!(
            manager.slot_count(),
            2,
            "cycle {cycle}: the list does not grow"
        );
    }
    let trailing_gone = trailing_gone.expect("the loop ran");
    assert!(
        !manager.remove_input_port(trailing_gone),
        "a removed port is gone"
    );
    assert!(manager.port_info(trailing_gone).is_none());
}

/// **A removed port's inlet leaves the graph**: its slot's change is a
/// `Detach`, and the slot is another device's after it.
#[test]
fn a_removed_port_queues_its_inlets_removal() {
    use tutti_midi_runtime::InletChange;
    let manager = ports(2);
    let port = manager.open_input_port("A").unwrap();
    let slot = port.port_id();
    let first = manager.inlets().take_changes();
    assert!(matches!(
        first.as_slice(),
        [InletChange::Attach { slot: 0, .. }]
    ));
    assert!(manager.remove_input_port(slot));
    let after = manager.inlets().take_changes();
    assert!(matches!(
        after.as_slice(),
        [InletChange::Detach { slot: 0 }]
    ));
}

/// A stale `PortId` cannot reach the port that took its index.
///
/// Port A is removed and port B created at the same index. A's id must not
/// remove, toggle or describe B.
#[test]
fn a_stale_port_id_cannot_reach_the_port_that_reused_its_slot() {
    let manager = ports(2);
    let a = manager.open_input_port("A").unwrap().port_id();
    assert!(manager.remove_input_port(a));
    let b = manager.open_input_port("B").unwrap();
    let b_id = b.port_id();
    assert_eq!(b_id.index(), a.index(), "B reuses A's index");
    assert_ne!(b_id, a);

    assert!(!manager.set_port_active(a, false));
    assert!(!manager.is_port_active(a));
    assert!(manager.port_info(a).is_none());
    assert!(!manager.remove_input_port(a), "a stale id removes nothing");

    let info = manager.port_info(b_id).expect("B is still there");
    assert_eq!((info.name.as_str(), info.active), ("B", true));
}

/// **A full inlet counts what it drops**, per port and in total, and a removed
/// port's drops stay in the total.
#[test]
fn a_full_inlet_counts_what_it_drops() {
    let (manager, _rig) = rig(1, 4, Duration::ZERO);
    let mut handle = manager.open_input_port("Busy").unwrap();
    let id = handle.port_id();
    let taken = (0..10).filter(|_| handle.push(noop(), now())).count();
    assert_eq!(taken, 4);
    assert_eq!(manager.port_info(id).map(|p| p.dropped), Some(6));
    assert_eq!(manager.dropped_events(), 6);
    assert!(manager.remove_input_port(id));
    assert_eq!(manager.dropped_events(), 6, "a removed port's drops stay");
}
