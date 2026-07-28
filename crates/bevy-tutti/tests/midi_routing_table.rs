//! The routing table a host holds must be the one the audio thread reads.
//!
//! `MidiRoutingRes` wraps the *writer* half of a `MidiRoutingTable`; the RT
//! `MidiPreBlock` holds the `Arc<RtPublish<..>>` its `snapshot_arc()` handed
//! out. Build a second table and its `commit()` publishes into a cell nothing
//! reads — every hardware MIDI event is dropped, silently, with nothing in the
//! log. That is the highest-consequence invariant in this layer and it had no
//! test; it was enforced only by the type having no `Default`.
//!
//! This asserts the property directly rather than trusting the type: publish
//! through the resource, read through the arc the pre-block would hold.

#![cfg(feature = "midi")]

use bevy_tutti::midi::MidiRoutingRes;
use tutti_midi_types::{MidiRoute, MidiRoutingTable, MidiUnitId};

/// A commit through the resource is visible on the snapshot the RT reads.
#[test]
fn the_routing_table_publishes_where_the_rt_reads() {
    let table = MidiRoutingTable::new();
    // Exactly what `build_into` hands the pre-block, before the table itself
    // moves into the resource.
    let rt_view = table.snapshot_arc();
    let mut res = MidiRoutingRes(table);

    let unit = MidiUnitId::new(7);
    res.0
        .set_routes(vec![MidiRoute::for_channel(3).with_target(unit)], None);
    res.0.commit();

    let snapshot = rt_view.read();
    let targets: Vec<MidiUnitId> = snapshot
        .route(&tutti_midi_types::ump::MidiEvent::note_on(0, 3, 60, 0x8000))
        .collect();
    assert!(
        targets.contains(&unit),
        "a route published through the resource must reach the RT snapshot"
    );
}

/// The failure this guards: a table built separately publishes nowhere.
///
/// Not a test of our code so much as of the claim in `MidiRoutingRes`'s doc —
/// if this ever stops holding, the `no Default` guard is protecting nothing and
/// the doc is wrong.
#[test]
fn a_separately_built_table_does_not_reach_that_snapshot() {
    let rt_table = MidiRoutingTable::new();
    let rt_view = rt_table.snapshot_arc();

    // The mistake: a fresh table instead of the one the pre-block shares.
    let mut orphan = MidiRoutingRes(MidiRoutingTable::new());
    let unit = MidiUnitId::new(9);
    orphan
        .0
        .set_routes(vec![MidiRoute::for_channel(3).with_target(unit)], None);
    orphan.0.commit();

    let snapshot = rt_view.read();
    let targets: Vec<MidiUnitId> = snapshot
        .route(&tutti_midi_types::ump::MidiEvent::note_on(0, 3, 60, 0x8000))
        .collect();
    assert!(
        !targets.contains(&unit),
        "an orphaned table must not appear to work — this is the silent failure"
    );
}
