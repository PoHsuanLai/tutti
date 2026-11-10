use super::*;

/// The controls report the mode last asked for, from the cell, and the
/// node adopts it on its next block.
///
/// Mutation (run): `set_mpe_mode` publishing nothing (drop the `publish`) →
/// `pending_mpe_mode` stays `None`, fails. The node built without a reader
/// on the controls' cell (its `mpe_request` left registered on a cell of
/// its own) → it never sees the request, fails (run).
#[test]
fn the_controls_report_the_mode_last_asked_for() {
    use tutti_midi_types::mpe::MpeZoneConfig;
    let parts = MidiInputNode::new(None).into_parts();
    let controls = parts.controls;
    assert_eq!(controls.pending_mpe_mode(), None);
    let lower = MpeMode::LowerZone(MpeZoneConfig::lower(15));
    let upper = MpeMode::UpperZone(MpeZoneConfig::upper(7));
    controls.set_mpe_mode(lower);
    controls.clone().set_mpe_mode(upper);
    assert_eq!(controls.pending_mpe_mode(), Some(upper), "the last asked");

    // A node over the same request, reading it through a reader of its
    // own, as the inserted one is.
    let mut node = MidiInputNode {
        mpe_request: controls.request.reader(),
        ..MidiInputNode::new(None)
    };
    node.adopt_mpe_request();
    assert_eq!(*node.mpe.mode(), upper, "the node adopts it");
}

/// **A request the node held across the next `set_mpe_mode` is freed by
/// the next `reclaim`**, with no further publish.
///
/// Mutation (run): `MidiInputControls::reclaim` returning 0 without
/// reclaiming → one request is still retired, fails.
#[test]
fn reclaim_frees_a_request_a_block_held_across_the_next() {
    use tutti_midi_types::mpe::MpeZoneConfig;
    let mut node = MidiInputNode::new(None);
    let controls = MidiInputControls {
        request: Arc::clone(node.mpe_request.cell()),
        dropped: Arc::clone(&node.dropped),
    };
    {
        // A block in progress across the request.
        let _held = node.mpe_request.lend();
        controls.set_mpe_mode(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    }
    assert_eq!(controls.request.retired_len(), 1, "pinned until a scan");
    assert_eq!(controls.reclaim(), 0);
    assert_eq!(controls.request.retired_len(), 0, "freed by the reclaim");
}

/// A listener on one channel takes that channel's port and the
/// channelless one; an omni listener takes all seventeen.
///
/// Mutation: leave out the channelless port for a channel → fails.
#[test]
fn a_listener_takes_its_channel_and_the_channelless_port() {
    let one: Vec<usize> = input_ports(Some(MidiChannel::of(3))).collect();
    assert_eq!(one, vec![3, CHANNELLESS_PORT]);
    let all: Vec<usize> = input_ports(None).collect();
    assert_eq!(all, (0..MIDI_INPUT_PORTS).collect::<Vec<_>>());
}

/// A voice message leaves on its channel's port; a system message on
/// the channelless one.
#[test]
fn events_leave_on_their_channels_port() {
    // 7-bit velocity 100, widened (M2-115).
    let on = tutti_fixtures::midi::note_on(
        tutti_ump::MidiGroup::FIRST,
        MidiChannel::of(9),
        60,
        tutti_midi_types::convert::midi1_velocity_to_midi2(100),
    );
    assert_eq!(input_port_of(&on), 9);
    let clock = tutti_fixtures::midi::timing_clock(tutti_ump::MidiGroup::FIRST);
    assert_eq!(input_port_of(&clock), CHANNELLESS_PORT);
}
