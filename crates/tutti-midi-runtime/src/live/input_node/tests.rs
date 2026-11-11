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
    let parts = MidiInputNode::new(None).into_parts();
    let controls = parts.controls;
    assert_eq!(controls.pending_mpe_mode(), None);
    let lower = tutti_fixtures::midi::lower_zone(15);
    let upper = tutti_fixtures::midi::upper_zone(7);
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
    let mut node = MidiInputNode::new(None);
    let controls = MidiInputControls {
        request: Arc::clone(node.mpe_request.cell()),
        dropped: Arc::clone(&node.dropped),
    };
    {
        // A block in progress across the request.
        let _held = node.mpe_request.lend();
        controls.set_mpe_mode(tutti_fixtures::midi::lower_zone(15));
    }
    assert_eq!(controls.request.retired_len(), 1, "pinned until a scan");
    assert_eq!(controls.reclaim(), 0);
    assert_eq!(controls.request.retired_len(), 0, "freed by the reclaim");
}

/// A listener on one channel of a Group takes that channel's port and the
/// Group's channelless one; an omni listener of a Group takes its seventeen.
///
/// Mutation: leave out the channelless port for a channel → fails.
#[test]
fn a_listener_takes_its_channel_and_the_channelless_port() {
    let first = MidiInputPorts::FIRST_GROUP;
    let one: Vec<usize> = first
        .listening(Some(MidiGroup::FIRST), Some(MidiChannel::of(3)))
        .collect();
    assert_eq!(one, vec![3, CHANNELLESS_PORT]);
    let all: Vec<usize> = first.listening(None, None).collect();
    assert_eq!(all, (0..MIDI_INPUT_PORTS).collect::<Vec<_>>());
}

/// A voice message leaves on its channel's port; a system message on
/// the channelless one.
#[test]
fn events_leave_on_their_channels_port() {
    let first = MidiInputPorts::FIRST_GROUP;
    // 7-bit velocity 100, widened (M2-115).
    let on = tutti_fixtures::midi::note_on(
        tutti_ump::MidiGroup::FIRST,
        MidiChannel::of(9),
        60,
        tutti_fixtures::midi::wide_velocity(100),
    );
    assert_eq!(first.port_of(&on), Some(9));
    let clock = tutti_fixtures::midi::timing_clock(tutti_ump::MidiGroup::FIRST);
    assert_eq!(first.port_of(&clock), Some(CHANNELLESS_PORT));
}

/// **A Group is a channel space of its own** (M2-104 section 2.1.2: a Group
/// is 16 Channels; Appendix B.1: what a message does in one Group "does not
/// apply to Channels in any other Group"): channel 1 of Group 2 leaves from a
/// port of its own, not channel 1 of Group 1's; a Group's System message
/// leaves from that Group's channelless port; a message with no Group
/// (Utility) from the first Group's; a Group the node was not built to hear
/// has no port.
///
/// Mutation: `port_of` ignoring the Group (the channel alone) → the first
/// assertion fails.
#[test]
fn a_group_has_ports_of_its_own() {
    let two = MidiInputPorts::through(MidiGroup::of(1));
    assert_eq!(two.count(), 2 * MIDI_INPUT_PORTS);
    let note = |group: u8, channel: u8| {
        tutti_fixtures::midi::note_on(
            MidiGroup::of(group),
            MidiChannel::of(channel),
            60,
            tutti_fixtures::midi::wide_velocity(100),
        )
    };
    assert_eq!(two.port_of(&note(0, 1)), Some(1));
    assert_eq!(two.port_of(&note(1, 1)), Some(MIDI_INPUT_PORTS + 1));
    assert_eq!(
        two.port_of(&tutti_fixtures::midi::timing_clock(MidiGroup::of(1))),
        Some(MIDI_INPUT_PORTS + CHANNELLESS_PORT)
    );
    // A Utility NOOP has no Group.
    assert_eq!(
        two.port_of(&tutti_fixtures::midi::noop()),
        Some(CHANNELLESS_PORT)
    );
    assert_eq!(two.port_of(&note(2, 1)), None, "not heard");
    // A listener on Group 2's channel 1 takes that Group's ports only.
    let ports: Vec<usize> = two
        .listening(Some(MidiGroup::of(1)), Some(MidiChannel::of(1)))
        .collect();
    assert_eq!(
        ports,
        vec![MIDI_INPUT_PORTS + 1, MIDI_INPUT_PORTS + CHANNELLESS_PORT]
    );
    // One that names a Group not heard takes nothing.
    assert_eq!(two.listening(Some(MidiGroup::of(2)), None).count(), 0);
}
