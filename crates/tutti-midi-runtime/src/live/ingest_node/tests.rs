use super::*;
use std::num::NonZeroUsize;
use tutti_fixtures::midi;
use tutti_midi_protocol::widen_velocity;
use tutti_ump::{U7, Utility};

/// The node adopts the mode last asked for on its next block.
#[test]
fn the_node_adopts_the_mode_last_asked_for() {
    let parts = MidiIngestNode::new(NonZeroUsize::MIN).into_parts();
    let controls = parts.controls;
    let lower = tutti_fixtures::midi::lower_zone(15);
    let upper = tutti_fixtures::midi::upper_zone(7);
    controls.set_mpe_mode(lower);
    controls.clone().set_mpe_mode(upper);

    // A node over the same request, reading it through a reader of its
    // own, as the inserted one is.
    let mut node = MidiIngestNode {
        mpe_request: controls.request.reader(),
        ..MidiIngestNode::new(NonZeroUsize::MIN)
    };
    node.adopt_mpe_request();
    assert_eq!(node.mpe_mode, upper, "the node adopts it");
}

/// **A request the node held across the next `set_mpe_mode` is freed by
/// the next `reclaim`**, with no further publish.
#[test]
fn reclaim_frees_a_request_a_block_held_across_the_next() {
    let mut node = MidiIngestNode::new(NonZeroUsize::MIN);
    let controls = MidiIngestControls {
        request: Arc::clone(node.mpe_request.cell()),
        dropped: Arc::clone(&node.dropped),
    };
    {
        // A block in progress across the request.
        let _held = node.mpe_request.lend();
        controls.set_mpe_mode(tutti_fixtures::midi::lower_zone(15));
    }
    assert_eq!(controls.request.retired_len(), 1, "pinned until a scan");
    assert_eq!(tutti_rt::Reclaim::reclaim(&controls), 0);
    assert_eq!(controls.request.retired_len(), 0, "freed by the reclaim");
}

/// A listener on one channel of a Group takes that channel's port and the
/// Group's channelless one; an omni listener of a Group takes its seventeen.
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
    let on = midi::note_on(MidiChannel::of(9), 60, widen_velocity(U7::of(100)).get());
    assert_eq!(first.port_of(&on), Some(9));
    let clock = Ump::from(Message::System(
        tutti_ump::MidiGroup::FIRST,
        System::TimingClock,
    ));
    assert_eq!(first.port_of(&clock), Some(CHANNELLESS_PORT));
}

/// **A Group is a channel space of its own** (M2-104 section 2.1.2: a Group
/// is 16 Channels; Appendix B.1: what a message does in one Group does not
/// apply to Channels in any other Group): channel 1 of Group 2 leaves from a
/// port of its own, not channel 1 of Group 1's; a Group's System message
/// leaves from that Group's channelless port; a message with no Group
/// (Utility) from the first Group's; a Group the node was not built to hear
/// has no port.
/// Spec: m2-104/2.1.2#2
#[test]
fn a_group_has_ports_of_its_own() {
    let two = MidiInputPorts::through(MidiGroup::of(1));
    assert_eq!(two.count(), 2 * MIDI_INPUT_PORTS);
    let note = |group: u8, channel: u8| {
        midi::note_on(
            tutti_ump::Address::new(MidiGroup::of(group), MidiChannel::of(channel)),
            60,
            widen_velocity(U7::of(100)).get(),
        )
    };
    assert_eq!(two.port_of(&note(0, 1)), Some(1));
    assert_eq!(two.port_of(&note(1, 1)), Some(MIDI_INPUT_PORTS + 1));
    assert_eq!(
        two.port_of(&Ump::from(Message::System(
            MidiGroup::of(1),
            System::TimingClock
        ))),
        Some(MIDI_INPUT_PORTS + CHANNELLESS_PORT)
    );
    // A Utility NOOP has no Group.
    assert_eq!(
        two.port_of(&Ump::from(Message::Utility(Utility::Noop))),
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
