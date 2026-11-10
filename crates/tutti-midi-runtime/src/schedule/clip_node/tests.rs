use super::*;
use tutti_fixtures::midi::note_on;
use tutti_types::Beat;
use tutti_ump::{MidiChannel, MidiGroup};

/// **A clip a block still held when `set_events` replaced it is freed by
/// the next `reclaim`**, with no further publish.
///
/// Mutation (run): `MidiClipControls::reclaim` returning 0 without
/// reclaiming → the first clip is still alive, fails.
#[test]
fn reclaim_frees_a_clip_a_block_held_across_its_replacement() {
    let on = note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF);
    let mut node = MidiClipNode::new([TimedMidiEvent::new(Beat(1.0), on)]);
    let controls = MidiClipControls {
        shared: Arc::clone(&node.shared),
    };
    let gone = Arc::downgrade(&node.shared.current());
    {
        // A block in progress across the replacement.
        let _held = node.clip.lend();
        controls.clear();
    }
    assert!(gone.upgrade().is_some(), "retired, pinned until a scan");
    assert_eq!(controls.reclaim(), 0);
    assert!(gone.upgrade().is_none(), "freed by the reclaim");
}

/// **A clip node's held-note table is 24 KiB**, allocated with the node.
///
/// Mutation (run): `LEDGER_CAPACITY` of 16,384 → 196 KB, fails.
#[test]
fn a_clip_nodes_table_is_24_kib() {
    let node = MidiClipNode::new([]);
    assert_eq!(node.play.held.heap_bytes(), 2_048 * 12);
}
