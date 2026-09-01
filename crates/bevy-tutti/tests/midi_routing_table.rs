//! The routing table a host holds must be the one the audio thread reads.
//!
//! `MidiRoutingRes` wraps the *writer* half of a `MidiRoutingTable`; the RT
//! `MidiPreBlock` holds the `Arc<RtPublish<..>>` its `snapshot_arc()` handed
//! out. Build a second table and its `commit()` publishes into a cell nothing
//! reads — every hardware MIDI event is dropped, silently, with nothing in the
//! log.
//!
//! That failure is now unreachable through this type: the field is
//! `pub(crate)` and the constructor is crate-internal, so a host cannot build
//! an orphan at all. The compile-time half needs no test — you cannot assert
//! the absence of a compile error — and the behavioural half moved down to
//! `tutti_midi_types::routing`'s own tests
//! (`two_tables_do_not_share_a_snapshot`), where the type it describes lives.
//!
//! What remains here is the positive claim: a publish through the resource is
//! visible on the snapshot the RT would read.

#![cfg(feature = "midi")]

use bevy_tutti::midi::test_support::routing_table_for_test;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_midi_types::{MidiRoute, MidiUnitId};

/// A publish through the resource reaches the snapshot the RT reads.
#[test]
fn the_routing_table_publishes_where_the_rt_reads() {
    // Both halves of the one shared cell: the resource a host writes through,
    // and the arc `build_into` hands the pre-block.
    let (mut res, rt_view) = routing_table_for_test();

    let unit = MidiUnitId::new(7);
    res.publish(
        vec![MidiRoute::for_channel(MidiChannel::new(3)).with_target(unit)],
        None,
    );

    let snapshot = rt_view.read();
    let targets: Vec<MidiUnitId> = snapshot
        .route(&tutti_midi_types::ump::MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(3),
            60,
            0x8000,
        ))
        .collect();
    assert!(
        targets.contains(&unit),
        "a route published through the resource must reach the RT snapshot"
    );
}

/// `publish` commits — staging without publishing is not a reachable state.
///
/// The engine's `set_routes` only marks the table dirty; nothing reaches the
/// audio thread until `commit()`. Pairing them in one method is what keeps a
/// caller from staging an edit that silently never arrives.
#[test]
fn publishing_leaves_nothing_staged() {
    let (mut res, rt_view) = routing_table_for_test();
    res.publish(
        vec![MidiRoute::for_channel(MidiChannel::new(0)).with_target(MidiUnitId::new(1))],
        None,
    );

    assert_eq!(res.route_count(), 1, "the rule is staged");
    assert!(
        rt_view.read().has_routes(),
        "and already published — a caller cannot be left holding an uncommitted edit"
    );
}
