use std::time::{Duration, Instant};

use tutti_ump::MidiChannel;

use super::super::testing::{OTHER, OURS, PEER, header_from, nak, sent, wait};
use super::super::*;

fn start() -> (ProcessInitiator, Vec<ProcessAction>, Instant) {
    let now = Instant::now();
    let (initiator, actions) = ProcessInitiator::new(Muid::of(OURS), Muid::of(PEER), now);
    (initiator, actions, now)
}

fn features_reply(features: ProcessFeatures) -> Decoded {
    Decoded::Message(CiMessage::ProcessCapabilitiesReply {
        header: header_from(PEER, CiDeviceId::FunctionBlock),
        to: Muid::of(OURS),
        features,
    })
}

fn begin(scope: ReportScope) -> Decoded {
    Decoded::Message(CiMessage::MessageReportReply {
        header: header_from(PEER, scope.address()),
        to: Muid::of(OURS),
        scope,
    })
}

fn end(address: CiDeviceId) -> Decoded {
    Decoded::Message(CiMessage::MessageReportEnd {
        header: header_from(PEER, address),
        to: Muid::of(OURS),
    })
}

/// Every kind of message the address may be asked for.
fn everything(address: CiDeviceId) -> ReportScope {
    let system = if address == CiDeviceId::FunctionBlock {
        SystemMessages::all()
    } else {
        SystemMessages::empty()
    };
    ReportScope::from_wire(
        address,
        system.bits(),
        ChannelControllers::all().bits(),
        NoteData::all().bits(),
    )
    .unwrap()
}

fn function_block() -> ReportRequest {
    ReportRequest::new(DataControl::Full, everything(CiDeviceId::FunctionBlock))
}

/// An Initiator whose capabilities are known, and so nothing waits.
fn known(features: ProcessFeatures) -> (ProcessInitiator, Instant) {
    let (mut initiator, _, now) = start();
    initiator.on_message(&features_reply(features), now);
    assert_eq!(initiator.deadline(), None);
    (initiator, now)
}

#[test]
fn it_starts_with_a_capabilities_inquiry_to_the_peer_on_the_function_block() {
    // §9.2: the Destination is the Function Block, never a Channel.
    let (initiator, actions, now) = start();
    let [CiMessage::ProcessCapabilitiesInquiry { header, to }] = sent(&actions)[..] else {
        panic!("one inquiry, got {actions:?}");
    };
    assert_eq!(header.device_id, CiDeviceId::FunctionBlock);
    assert_eq!(header.source, Muid::of(OURS));
    assert_eq!(*to, Muid::of(PEER));
    assert_eq!(initiator.peer(), Muid::of(PEER));
    assert_eq!(initiator.features(), None);
    assert_eq!(initiator.deadline(), Some(now + PROCESS_TIMEOUT));
}

#[test]
fn the_reply_gives_the_features_and_ends_the_wait() {
    let (mut initiator, _, now) = start();
    let actions = initiator.on_message(&features_reply(ProcessFeatures::MIDI_MESSAGE_REPORT), now);
    assert_eq!(
        actions,
        [ProcessAction::Capabilities(
            ProcessFeatures::MIDI_MESSAGE_REPORT
        )]
    );
    assert_eq!(
        initiator.features(),
        Some(ProcessFeatures::MIDI_MESSAGE_REPORT)
    );
    assert_eq!(initiator.deadline(), None);
    // A second reply, for no inquiry, changes nothing.
    assert!(
        initiator
            .on_message(&features_reply(ProcessFeatures::empty()), now)
            .is_empty()
    );
    assert_eq!(
        initiator.features(),
        Some(ProcessFeatures::MIDI_MESSAGE_REPORT)
    );
}

#[test]
fn a_silent_peer_times_out_the_capabilities_after_the_process_timeout() {
    let (mut initiator, _, now) = start();
    assert_eq!(PROCESS_TIMEOUT, Duration::from_secs(3));
    let early = now + PROCESS_TIMEOUT - Duration::from_millis(1);
    assert!(initiator.on_timeout(early).is_empty());
    assert_eq!(initiator.deadline(), Some(now + PROCESS_TIMEOUT));
    assert_eq!(
        initiator.on_timeout(now + PROCESS_TIMEOUT),
        [ProcessAction::CapabilitiesTimedOut]
    );
    assert_eq!(initiator.deadline(), None);
}

#[test]
fn a_nak_of_the_capabilities_inquiry_ends_it_as_refused() {
    let (mut initiator, _, now) = start();
    let refusal = nak(
        CiDeviceId::FunctionBlock,
        SUB_ID2_PROCESS_CAPABILITIES_INQUIRY,
        Nak::STATUS_MESSAGE_NOT_SUPPORTED,
    );
    let actions = initiator.on_message(&refusal, now);
    let [ProcessAction::CapabilitiesRefused(nak)] = &actions[..] else {
        panic!("a refusal, got {actions:?}");
    };
    assert_eq!(nak.status_code, Nak::STATUS_MESSAGE_NOT_SUPPORTED);
    assert_eq!(initiator.deadline(), None);
}

#[test]
fn a_report_is_one_inquiry_a_reply_and_an_end_on_the_inquirys_device_id() {
    // §9.4 stages 1, 2 and 4.
    for address in [
        CiDeviceId::FunctionBlock,
        CiDeviceId::Group,
        CiDeviceId::Channel(MidiChannel::of(6)),
    ] {
        let (mut initiator, now) = known(ProcessFeatures::MIDI_MESSAGE_REPORT);
        let request = ReportRequest::new(DataControl::NonDefault, everything(address));
        let actions = initiator.report(request, now).unwrap();
        let [
            CiMessage::MessageReportInquiry {
                header,
                to,
                control,
                scope,
            },
        ] = sent(&actions)[..]
        else {
            panic!("one inquiry, got {actions:?}");
        };
        assert_eq!(header.device_id, address);
        assert_eq!(*to, Muid::of(PEER));
        assert_eq!(*control, DataControl::NonDefault);
        assert_eq!(*scope, request.scope);
        assert!(initiator.is_reporting());

        let declared = ReportScope::none(address);
        assert_eq!(
            initiator.on_message(&begin(declared), now),
            [ProcessAction::ReportBegan { declared }]
        );
        assert!(initiator.is_reporting());
        assert_eq!(
            initiator.on_message(&end(address), now),
            [ProcessAction::ReportEnded]
        );
        assert!(!initiator.is_reporting());
        assert_eq!(initiator.deadline(), None);
    }
}

#[test]
fn a_reply_declares_only_the_kinds_that_were_asked() {
    // §9.6: a Responder sets no bit that was not requested.
    let (mut initiator, now) = known(ProcessFeatures::MIDI_MESSAGE_REPORT);
    let asked = ReportScope::Group {
        controllers: ChannelControllers::PITCH_BEND,
        notes: NoteData::empty(),
    };
    initiator
        .report(ReportRequest::new(DataControl::Full, asked), now)
        .unwrap();
    let overreach = ReportScope::Group {
        controllers: ChannelControllers::all(),
        notes: NoteData::NOTES,
    };
    assert_eq!(
        initiator.on_message(&begin(overreach), now),
        [ProcessAction::ReportBegan { declared: asked }]
    );
}

#[test]
fn a_reply_or_end_on_another_device_id_is_not_the_reports() {
    let (mut initiator, now) = known(ProcessFeatures::MIDI_MESSAGE_REPORT);
    initiator.report(function_block(), now).unwrap();
    let other = CiDeviceId::Group;
    assert!(
        initiator
            .on_message(&begin(everything(other)), now)
            .is_empty()
    );
    assert!(initiator.on_message(&end(other), now).is_empty());
    assert!(initiator.is_reporting());
}

#[test]
fn an_end_before_the_reply_does_not_end_the_report() {
    let (mut initiator, now) = known(ProcessFeatures::MIDI_MESSAGE_REPORT);
    initiator.report(function_block(), now).unwrap();
    assert!(
        initiator
            .on_message(&end(CiDeviceId::FunctionBlock), now)
            .is_empty()
    );
    assert!(initiator.is_reporting());
}

#[test]
fn a_nak_ends_the_report_without_an_end() {
    // §9.8.1.
    for status in [Nak::STATUS_TERMINATE, Nak::STATUS_RETRY] {
        let (mut initiator, now) = known(ProcessFeatures::MIDI_MESSAGE_REPORT);
        initiator.report(function_block(), now).unwrap();
        initiator.on_message(&begin(everything(CiDeviceId::FunctionBlock)), now);
        let actions = initiator.on_message(
            &nak(
                CiDeviceId::FunctionBlock,
                SUB_ID2_MESSAGE_REPORT_INQUIRY,
                status,
            ),
            now,
        );
        let [ProcessAction::ReportRefused(nak)] = &actions[..] else {
            panic!("a refusal, got {actions:?}");
        };
        assert_eq!(nak.status_code, status);
        assert!(!initiator.is_reporting());
        assert_eq!(initiator.deadline(), None);
    }
}

#[test]
fn a_nak_of_the_report_before_the_reply_ends_it_too() {
    let (mut initiator, now) = known(ProcessFeatures::MIDI_MESSAGE_REPORT);
    let channel = CiDeviceId::Channel(MidiChannel::of(2));
    let request = ReportRequest::new(DataControl::Full, everything(channel));
    initiator.report(request, now).unwrap();
    let actions = initiator.on_message(
        &nak(
            channel,
            SUB_ID2_MESSAGE_REPORT_INQUIRY,
            Nak::STATUS_NOT_IN_USE,
        ),
        now,
    );
    assert!(matches!(&actions[..], [ProcessAction::ReportRefused(_)]));
    assert!(!initiator.is_reporting());
}

#[test]
fn a_nak_of_the_capabilities_does_not_end_a_report() {
    let (mut initiator, now) = known(ProcessFeatures::MIDI_MESSAGE_REPORT);
    initiator.report(function_block(), now).unwrap();
    let refusal = nak(
        CiDeviceId::FunctionBlock,
        SUB_ID2_PROCESS_CAPABILITIES_INQUIRY,
        Nak::STATUS_NAK,
    );
    assert!(initiator.on_message(&refusal, now).is_empty());
    assert!(initiator.is_reporting());
}

#[test]
fn a_report_waits_for_the_reply_and_the_reply_starts_the_wait_for_the_end() {
    let (mut initiator, now) = known(ProcessFeatures::MIDI_MESSAGE_REPORT);
    initiator.report(function_block(), now).unwrap();
    assert_eq!(initiator.deadline(), Some(now + PROCESS_TIMEOUT));
    // The Reply arrives late in its wait; the End gets a wait of its own.
    let later = now + PROCESS_TIMEOUT - Duration::from_millis(1);
    initiator.on_message(&begin(everything(CiDeviceId::FunctionBlock)), later);
    assert_eq!(initiator.deadline(), Some(later + PROCESS_TIMEOUT));
    assert_eq!(
        initiator.on_timeout(later + PROCESS_TIMEOUT),
        [ProcessAction::ReportTimedOut]
    );
    assert!(!initiator.is_reporting());
    // A report with no Reply times out in the first wait.
    initiator.report(function_block(), later).unwrap();
    assert_eq!(
        initiator.on_timeout(later + PROCESS_TIMEOUT),
        [ProcessAction::ReportTimedOut]
    );
}

#[test]
fn a_timeout_wait_ack_extends_the_named_transaction_and_never_shortens_it() {
    // §5.10, Table 14: Status Data is the time to wait in 100 ms.
    let (mut initiator, _, now) = start();
    initiator.on_message(
        &wait(
            CiDeviceId::FunctionBlock,
            SUB_ID2_PROCESS_CAPABILITIES_INQUIRY,
            100,
        ),
        now,
    );
    assert_eq!(initiator.deadline(), Some(now + Duration::from_secs(10)));
    initiator.on_message(
        &wait(
            CiDeviceId::FunctionBlock,
            SUB_ID2_PROCESS_CAPABILITIES_INQUIRY,
            0,
        ),
        now,
    );
    assert_eq!(initiator.deadline(), Some(now + Duration::from_secs(10)));

    let (mut initiator, now) = known(ProcessFeatures::MIDI_MESSAGE_REPORT);
    initiator.report(function_block(), now).unwrap();
    initiator.on_message(
        &wait(
            CiDeviceId::FunctionBlock,
            SUB_ID2_MESSAGE_REPORT_INQUIRY,
            50,
        ),
        now,
    );
    assert_eq!(initiator.deadline(), Some(now + Duration::from_secs(5)));
    // On another Device ID, or naming another message, it is not this report's.
    initiator.on_message(
        &wait(CiDeviceId::Group, SUB_ID2_MESSAGE_REPORT_INQUIRY, 120),
        now,
    );
    initiator.on_message(
        &wait(
            CiDeviceId::FunctionBlock,
            SUB_ID2_PROCESS_CAPABILITIES_INQUIRY,
            120,
        ),
        now,
    );
    assert_eq!(initiator.deadline(), Some(now + Duration::from_secs(5)));
}

#[test]
fn a_peer_without_midi_message_report_cannot_be_asked_for_one() {
    let (mut initiator, now) = known(ProcessFeatures::empty());
    assert_eq!(
        initiator.report(function_block(), now),
        Err(ProcessError::NotSupported)
    );
    assert!(!initiator.is_reporting());
}

#[test]
fn a_report_is_sent_before_the_capabilities_are_known() {
    // §9 does not make the capabilities inquiry a precondition.
    let (mut initiator, _, now) = start();
    let actions = initiator.report(function_block(), now).unwrap();
    assert_eq!(sent(&actions).len(), 1);
}

#[test]
fn a_second_report_is_refused_while_one_is_in_flight() {
    let (mut initiator, now) = known(ProcessFeatures::MIDI_MESSAGE_REPORT);
    initiator.report(function_block(), now).unwrap();
    assert_eq!(
        initiator.report(function_block(), now),
        Err(ProcessError::InFlight)
    );
    initiator.on_message(&begin(everything(CiDeviceId::FunctionBlock)), now);
    initiator.on_message(&end(CiDeviceId::FunctionBlock), now);
    assert!(initiator.report(function_block(), now).is_ok());
}

#[test]
fn a_message_from_another_source_or_to_another_muid_is_not_this_peers() {
    let (mut initiator, now) = known(ProcessFeatures::MIDI_MESSAGE_REPORT);
    initiator.report(function_block(), now).unwrap();
    let scope = everything(CiDeviceId::FunctionBlock);
    let from_other = Decoded::Message(CiMessage::MessageReportReply {
        header: header_from(OTHER, CiDeviceId::FunctionBlock),
        to: Muid::of(OURS),
        scope,
    });
    let to_other = Decoded::Message(CiMessage::MessageReportReply {
        header: header_from(PEER, CiDeviceId::FunctionBlock),
        to: Muid::of(OTHER),
        scope,
    });
    assert!(initiator.on_message(&from_other, now).is_empty());
    assert!(initiator.on_message(&to_other, now).is_empty());
    assert!(initiator.on_message(&Decoded::NotCi, now).is_empty());
}

#[test]
fn asking_again_for_the_capabilities_starts_the_wait_over_and_keeps_the_features() {
    let (mut initiator, now) = known(ProcessFeatures::MIDI_MESSAGE_REPORT);
    let later = now + Duration::from_secs(60);
    let actions = initiator.inquire(later);
    assert_eq!(sent(&actions).len(), 1);
    assert_eq!(initiator.deadline(), Some(later + PROCESS_TIMEOUT));
    assert_eq!(
        initiator.features(),
        Some(ProcessFeatures::MIDI_MESSAGE_REPORT)
    );
}
