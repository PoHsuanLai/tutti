use std::time::{Duration, Instant};
use tutti_ump::DeviceIdentity;
use tutti_ump::{MidiChannel, U7};

use super::super::testing::{OTHER, OURS, PEER, channel, header_from, nak_with, wait, wait_with};
use super::super::*;

fn profile(n: u8) -> ProfileId {
    ProfileId::from_bytes([0x7E, n, 0, 1, 1]).unwrap()
}

fn start() -> (ProfileInitiator, Vec<ProfileAction>, Instant) {
    let now = Instant::now();
    let (initiator, actions) = ProfileInitiator::new(Muid::of(OURS), Muid::of(PEER), now);
    (initiator, actions, now)
}

/// An Initiator whose inquiry is over: nothing waits but what a test asks for.
fn known() -> (ProfileInitiator, Instant) {
    let (mut i, _, now) = start();
    i.on_message(&reply(CiDeviceId::FunctionBlock, &[], &[]));
    assert_eq!(i.deadline(), None);
    (i, now)
}

fn from_peer(address: CiDeviceId) -> CiHeader {
    header_from(PEER, address)
}

fn reply_from(
    source: u32,
    to: u32,
    address: CiDeviceId,
    enabled: &[ProfileId],
    disabled: &[ProfileId],
) -> Decoded {
    Decoded::Message(CiMessage::ProfileReply {
        header: header_from(source, address),
        to: Muid::of(to),
        profiles: ProfileLists::new(enabled.to_vec(), disabled.to_vec()).unwrap(),
    })
}

fn reply(address: CiDeviceId, enabled: &[ProfileId], disabled: &[ProfileId]) -> Decoded {
    reply_from(PEER, OURS, address, enabled, disabled)
}

fn enabled(address: CiDeviceId, p: ProfileId, channels: u16) -> Decoded {
    Decoded::Message(CiMessage::ProfileEnabled {
        header: header_from(PEER, address),
        profile: p,
        channels: ProfileChannels::of(channels),
    })
}

fn disabled(address: CiDeviceId, p: ProfileId, channels: u16) -> Decoded {
    Decoded::Message(CiMessage::ProfileDisabled {
        header: header_from(PEER, address),
        profile: p,
        channels: ProfileChannels::of(channels),
    })
}

fn state(i: &ProfileInitiator, address: CiDeviceId, p: ProfileId) -> Option<ProfileState> {
    i.table().get(address, p).map(ProfileEntry::state)
}

#[test]
fn it_starts_with_a_profile_inquiry_to_the_function_block_from_us_to_the_peer() {
    // M2-102 §2.4: an Inquiry to 0x7F asks for every Profile.
    let (initiator, actions, now) = start();
    let [ProfileAction::Send(CiMessage::ProfileInquiry { header, to })] = &actions[..] else {
        panic!("one Inquiry, got {actions:?}");
    };
    assert_eq!(header.device_id, CiDeviceId::FunctionBlock);
    assert_eq!(header.source, Muid::of(OURS));
    assert_eq!(*to, Muid::of(PEER));
    assert_eq!(initiator.deadline(), Some(now + PEER_REPLY_TIMEOUT));
    assert!(!initiator.is_known());
}

#[test]
fn the_set_is_complete_at_the_final_reply_on_7f_and_not_before() {
    // M2-102 §2.4: Channels first, then Groups, then a final reply on address 0x7F.
    let (mut i, _, _) = start();
    assert!(
        i.on_message(&reply(channel(0), &[profile(1)], &[]))
            .is_empty()
    );
    assert!(
        i.on_message(&reply(CiDeviceId::Group, &[], &[profile(2)]))
            .is_empty()
    );
    assert!(!i.is_known());
    let actions = i.on_message(&reply(CiDeviceId::FunctionBlock, &[profile(3)], &[]));
    assert_eq!(actions, [ProfileAction::ProfilesKnown]);
    assert!(i.is_known());
    assert_eq!(i.deadline(), None);
    assert_eq!(
        i.table().addresses().collect::<Vec<_>>(),
        [channel(0), CiDeviceId::Group, CiDeviceId::FunctionBlock]
    );
}

#[test]
fn the_enabled_list_is_enabled_and_the_disabled_list_disabled() {
    let (mut i, _, _) = start();
    i.on_message(&reply(channel(4), &[profile(1)], &[profile(2)]));
    assert_eq!(
        state(&i, channel(4), profile(1)),
        Some(ProfileState::Enabled)
    );
    assert_eq!(
        state(&i, channel(4), profile(2)),
        Some(ProfileState::Disabled)
    );
    assert_eq!(state(&i, channel(5), profile(1)), None);
    // A Reply carries no Channel count.
    assert_eq!(
        i.table().get(channel(4), profile(1)).unwrap().channels(),
        None
    );
}

#[test]
fn a_final_reply_with_no_profiles_ends_the_inquiry_and_prunes_the_silent_addresses() {
    // M2-102 §2.4: the empty 0x7F reply shows that no further replies will
    // follow, so an address that did not reply this round has no Profiles now.
    let (mut i, _, now) = start();
    i.on_message(&reply(channel(0), &[profile(1)], &[]));
    i.on_message(&reply(CiDeviceId::FunctionBlock, &[], &[]));
    assert!(i.is_known());
    assert_eq!(i.table().addresses().count(), 1);
    // A second inquiry: channel 0 is not heard again, so the final Reply drops it.
    let later = now + Duration::from_secs(60);
    i.inquire(later);
    assert!(
        i.table().get(channel(0), profile(1)).is_some(),
        "kept until the final Reply"
    );
    i.on_message(&reply(CiDeviceId::FunctionBlock, &[], &[]));
    assert!(i.table().is_empty());
}

#[test]
fn a_reply_replaces_its_address_whole() {
    let (mut i, _, now) = start();
    i.on_message(&reply(channel(0), &[profile(1)], &[profile(2)]));
    i.inquire(now);
    i.on_message(&reply(channel(0), &[], &[profile(3)]));
    assert_eq!(state(&i, channel(0), profile(1)), None);
    assert_eq!(state(&i, channel(0), profile(2)), None);
    assert_eq!(
        state(&i, channel(0), profile(3)),
        Some(ProfileState::Disabled)
    );
}

#[test]
fn a_reply_from_another_muid_or_to_another_muid_is_not_ours() {
    let (mut i, _, _) = start();
    assert!(
        i.on_message(&reply_from(
            OTHER,
            OURS,
            CiDeviceId::FunctionBlock,
            &[profile(1)],
            &[]
        ))
        .is_empty()
    );
    assert!(
        i.on_message(&reply_from(
            PEER,
            OTHER,
            CiDeviceId::FunctionBlock,
            &[profile(1)],
            &[]
        ))
        .is_empty()
    );
    assert!(!i.is_known());
    assert!(i.table().is_empty());
}

#[test]
fn a_reply_when_no_inquiry_is_waiting_changes_nothing() {
    let (mut i, _, _) = start();
    i.on_message(&reply(CiDeviceId::FunctionBlock, &[], &[]));
    let actions = i.on_message(&reply(channel(0), &[profile(1)], &[]));
    assert!(actions.is_empty());
    assert!(i.table().is_empty());
}

#[test]
fn an_inquiry_that_is_never_finished_times_out_and_keeps_what_it_heard() {
    let (mut i, _, now) = start();
    i.on_message(&reply(channel(0), &[profile(1)], &[]));
    assert!(
        i.on_timeout(now + PEER_REPLY_TIMEOUT - Duration::from_millis(1))
            .is_empty()
    );
    assert_eq!(
        i.on_timeout(now + PEER_REPLY_TIMEOUT),
        [ProfileAction::InquiryTimedOut]
    );
    assert!(!i.is_known());
    assert_eq!(
        state(&i, channel(0), profile(1)),
        Some(ProfileState::Enabled)
    );
    assert_eq!(i.deadline(), None);
}

#[test]
fn a_nak_of_the_inquiry_ends_it() {
    let (mut i, _, _) = start();
    let refusal = Decoded::Message(CiMessage::Nak {
        header: header_from(PEER, CiDeviceId::FunctionBlock),
        to: Muid::of(OURS),
        nak: Nak::new(U7::of(0x20), Nak::STATUS_MESSAGE_NOT_SUPPORTED),
    });
    let actions = i.on_message(&refusal);
    assert!(
        matches!(&actions[..], [ProfileAction::InquiryRefused(n)] if n.status_code == Nak::STATUS_MESSAGE_NOT_SUPPORTED)
    );
    assert_eq!(i.deadline(), None);
    assert!(!i.is_known());
}

#[test]
fn the_reports_update_the_table_and_say_what_changed() {
    let (mut i, _, _) = start();
    let p = profile(7);
    let changed = |address, change| ProfileAction::Changed {
        address,
        profile: p,
        change,
    };

    // Added (§7.4): newly supported, disabled.
    let added = Decoded::Message(CiMessage::ProfileAdded {
        header: header_from(PEER, channel(1)),
        profile: p,
    });
    assert_eq!(
        i.on_message(&added),
        [changed(channel(1), ProfileChange::Added)]
    );
    assert_eq!(state(&i, channel(1), p), Some(ProfileState::Disabled));
    // The same Report again changes nothing.
    assert!(i.on_message(&added).is_empty());

    // Enabled (§7.10), with its Channel count.
    assert_eq!(
        i.on_message(&enabled(channel(1), p, 3)),
        [changed(channel(1), ProfileChange::Enabled)]
    );
    let entry = *i.table().get(channel(1), p).unwrap();
    assert_eq!(
        (entry.state(), entry.channels()),
        (ProfileState::Enabled, Some(ProfileChannels::of(3)))
    );
    assert!(i.on_message(&enabled(channel(1), p, 3)).is_empty());
    // An Added Report of an enabled Profile does not disable it.
    assert!(i.on_message(&added).is_empty());
    assert_eq!(state(&i, channel(1), p), Some(ProfileState::Enabled));

    // Disabled (§7.11).
    assert_eq!(
        i.on_message(&disabled(channel(1), p, 3)),
        [changed(channel(1), ProfileChange::Disabled)]
    );
    assert_eq!(state(&i, channel(1), p), Some(ProfileState::Disabled));

    // Removed (§7.5).
    let removed = Decoded::Message(CiMessage::ProfileRemoved {
        header: header_from(PEER, channel(1)),
        profile: p,
    });
    assert_eq!(
        i.on_message(&removed),
        [changed(channel(1), ProfileChange::Removed)]
    );
    assert_eq!(state(&i, channel(1), p), None);
    assert!(i.table().is_empty());
    // A Profile that is not there cannot be removed again.
    assert!(i.on_message(&removed).is_empty());
}

#[test]
fn a_report_of_a_profile_the_table_lacks_adds_it() {
    let (mut i, _, _) = start();
    i.on_message(&enabled(CiDeviceId::Group, profile(1), 0));
    i.on_message(&disabled(CiDeviceId::Group, profile(2), 0));
    assert_eq!(
        state(&i, CiDeviceId::Group, profile(1)),
        Some(ProfileState::Enabled)
    );
    assert_eq!(
        state(&i, CiDeviceId::Group, profile(2)),
        Some(ProfileState::Disabled)
    );
}

#[test]
fn a_report_from_another_source_is_not_this_peers() {
    let (mut i, _, _) = start();
    let other = Decoded::Message(CiMessage::ProfileEnabled {
        header: header_from(OTHER, channel(0)),
        profile: profile(1),
        channels: ProfileChannels::of(1),
    });
    assert!(i.on_message(&other).is_empty());
    assert!(i.table().is_empty());
}

#[test]
fn a_report_at_one_address_leaves_the_same_profile_at_another_alone() {
    let (mut i, _, _) = start();
    i.on_message(&reply(channel(0), &[], &[profile(1)]));
    i.on_message(&reply(channel(1), &[], &[profile(1)]));
    i.on_message(&enabled(channel(0), profile(1), 1));
    assert_eq!(
        state(&i, channel(0), profile(1)),
        Some(ProfileState::Enabled)
    );
    assert_eq!(
        state(&i, channel(1), profile(1)),
        Some(ProfileState::Disabled)
    );
}

#[test]
fn a_version_1_peers_report_has_no_channel_count() {
    let (mut i, _, _) = start();
    let mut h = header_from(PEER, channel(0));
    h.ci_version = CiVersion::of(1);
    i.on_message(&Decoded::Message(CiMessage::ProfileEnabled {
        header: h,
        profile: profile(1),
        channels: ProfileChannels::BY_ADDRESS,
    }));
    assert_eq!(
        i.table().get(channel(0), profile(1)).unwrap().channels(),
        None
    );
}

#[test]
fn set_on_a_channel_sends_the_request_with_its_channel_count() {
    let (mut i, _, now) = start();
    let target = ProfileTarget::Channel {
        channel: MidiChannel::of(6),
        channels: ProfileChannels::of(7),
    };
    let actions = i.set_on(target, profile(1), now).unwrap();
    let [
        ProfileAction::Send(CiMessage::SetProfileOn {
            header,
            to,
            profile: p,
            channels,
        }),
    ] = &actions[..]
    else {
        panic!("a Set On, got {actions:?}");
    };
    assert_eq!(
        (header.device_id, header.source, *to, *p),
        (channel(6), Muid::of(OURS), Muid::of(PEER), profile(1))
    );
    assert_eq!(*channels, ProfileChannels::of(7));
}

#[test]
fn set_on_a_group_or_function_block_asks_for_zero_channels() {
    // §7.8: a Group or Function Block address sets Number Channels Requested to 0x0000.
    for (target, address) in [
        (ProfileTarget::Group, CiDeviceId::Group),
        (ProfileTarget::FunctionBlock, CiDeviceId::FunctionBlock),
    ] {
        let (mut i, _, now) = start();
        let actions = i.set_on(target, profile(1), now).unwrap();
        let [
            ProfileAction::Send(CiMessage::SetProfileOn {
                header, channels, ..
            }),
        ] = &actions[..]
        else {
            panic!("a Set On");
        };
        assert_eq!(header.device_id, address);
        assert!(channels.is_by_address());
    }
}

#[test]
fn set_on_resolves_on_the_enabled_report_of_that_profile_at_that_address() {
    let (mut i, now) = known();
    let target = ProfileTarget::Channel {
        channel: MidiChannel::of(0),
        channels: ProfileChannels::of(2),
    };
    i.set_on(target, profile(1), now).unwrap();
    let request = Request::SetOn {
        address: channel(0),
        profile: profile(1),
    };
    // Another Profile, and the same Profile at another address, are not the answer.
    let actions = i.on_message(&enabled(channel(0), profile(2), 2));
    assert!(
        actions
            .iter()
            .all(|a| !matches!(a, ProfileAction::Resolved { .. }))
    );
    let actions = i.on_message(&enabled(channel(1), profile(1), 2));
    assert!(
        actions
            .iter()
            .all(|a| !matches!(a, ProfileAction::Resolved { .. }))
    );
    // The Report is.
    let actions = i.on_message(&enabled(channel(0), profile(1), 2));
    assert!(actions.contains(&ProfileAction::Resolved {
        request,
        outcome: Outcome::Reported {
            state: ProfileState::Enabled,
            channels: ProfileChannels::of(2)
        },
    }));
    assert_eq!(i.deadline(), None);
}

#[test]
fn set_on_answered_by_a_disabled_report_is_the_peer_unable_to_comply() {
    // M2-102 §2.6: the device replies with a Profile Disabled Report.
    let (mut i, _, now) = start();
    i.set_on(ProfileTarget::Group, profile(1), now).unwrap();
    let actions = i.on_message(&disabled(CiDeviceId::Group, profile(1), 0));
    assert!(actions.contains(&ProfileAction::Resolved {
        request: Request::SetOn {
            address: CiDeviceId::Group,
            profile: profile(1)
        },
        outcome: Outcome::Reported {
            state: ProfileState::Disabled,
            channels: ProfileChannels::BY_ADDRESS
        },
    }));
}

#[test]
fn set_off_resolves_on_the_disabled_report_or_the_enabled_one_if_the_peer_could_not() {
    for (report, state) in [
        (disabled(channel(3), profile(1), 1), ProfileState::Disabled),
        (enabled(channel(3), profile(1), 1), ProfileState::Enabled),
    ] {
        let (mut i, _, now) = start();
        let actions = i.set_off(channel(3), profile(1), now).unwrap();
        let [
            ProfileAction::Send(CiMessage::SetProfileOff {
                header, profile: p, ..
            }),
        ] = &actions[..]
        else {
            panic!("a Set Off");
        };
        assert_eq!((header.device_id, *p), (channel(3), profile(1)));
        let actions = i.on_message(&report);
        assert!(actions.contains(&ProfileAction::Resolved {
            request: Request::SetOff {
                address: channel(3),
                profile: profile(1)
            },
            outcome: Outcome::Reported {
                state,
                channels: ProfileChannels::of(1)
            },
        }));
    }
}

#[test]
fn a_report_from_another_muid_does_not_resolve_a_request() {
    let (mut i, _, now) = start();
    i.set_off(channel(0), profile(1), now).unwrap();
    let other = Decoded::Message(CiMessage::ProfileDisabled {
        header: header_from(OTHER, channel(0)),
        profile: profile(1),
        channels: ProfileChannels::of(1),
    });
    assert!(i.on_message(&other).is_empty());
    assert!(i.deadline().is_some());
}

#[test]
fn a_nak_of_a_set_resolves_it_as_refused_whatever_its_status() {
    for status in [Nak::STATUS_PROFILE_NOT_SUPPORTED, Nak::STATUS_BUSY] {
        let (mut i, now) = known();
        i.set_on(ProfileTarget::FunctionBlock, profile(1), now)
            .unwrap();
        let actions = i.on_message(&nak_with(
            CiDeviceId::FunctionBlock,
            0x22,
            status,
            profile(1).to_bytes(),
        ));
        let [
            ProfileAction::Resolved {
                request,
                outcome: Outcome::Refused(n),
            },
        ] = &actions[..]
        else {
            panic!("refused, got {actions:?}");
        };
        assert_eq!(
            *request,
            Request::SetOn {
                address: CiDeviceId::FunctionBlock,
                profile: profile(1)
            }
        );
        assert_eq!(n.status_code, status);
        assert_eq!(i.deadline(), None);
    }
}

#[test]
fn a_nak_for_another_profile_or_another_message_resolves_nothing() {
    let (mut i, _, now) = start();
    i.set_on(ProfileTarget::Group, profile(1), now).unwrap();
    // The details name another Profile; the original message is a Set Off; another address.
    for refusal in [
        nak_with(
            CiDeviceId::Group,
            0x22,
            Nak::STATUS_PROFILE_NOT_SUPPORTED,
            profile(2).to_bytes(),
        ),
        nak_with(
            CiDeviceId::Group,
            0x23,
            Nak::STATUS_PROFILE_NOT_SUPPORTED,
            profile(1).to_bytes(),
        ),
        nak_with(
            channel(0),
            0x22,
            Nak::STATUS_PROFILE_NOT_SUPPORTED,
            profile(1).to_bytes(),
        ),
    ] {
        assert!(i.on_message(&refusal).is_empty());
    }
    assert!(i.deadline().is_some());
}

#[test]
fn a_nak_from_another_muid_resolves_nothing() {
    let (mut i, _, now) = start();
    i.set_on(ProfileTarget::Group, profile(1), now).unwrap();
    let foreign = Decoded::Message(CiMessage::Nak {
        header: header_from(OTHER, CiDeviceId::Group),
        to: Muid::of(OURS),
        nak: Nak::new(U7::of(0x22), Nak::STATUS_PROFILE_NOT_SUPPORTED)
            .with_details(profile(1).to_bytes().map(U7::of)),
    });
    assert!(i.on_message(&foreign).is_empty());
}

#[test]
fn a_request_with_no_answer_times_out() {
    let (mut i, _, now) = start();
    let later = now + Duration::from_secs(10);
    i.on_message(&reply(CiDeviceId::FunctionBlock, &[], &[]));
    i.set_on(ProfileTarget::Group, profile(1), later).unwrap();
    assert_eq!(i.deadline(), Some(later + PEER_REPLY_TIMEOUT));
    assert!(i.on_timeout(later).is_empty());
    assert_eq!(
        i.on_timeout(later + PEER_REPLY_TIMEOUT),
        [ProfileAction::Resolved {
            request: Request::SetOn {
                address: CiDeviceId::Group,
                profile: profile(1)
            },
            outcome: Outcome::TimedOut,
        }]
    );
}

#[test]
fn the_same_set_cannot_be_asked_twice_at_once() {
    let (mut i, _, now) = start();
    i.set_on(ProfileTarget::Group, profile(1), now).unwrap();
    assert_eq!(
        i.set_on(ProfileTarget::Group, profile(1), now),
        Err(RequestError::InFlight)
    );
    assert_eq!(
        i.set_off(CiDeviceId::Group, profile(1), now),
        Err(RequestError::InFlight)
    );
    // Another Profile or address is a request of its own.
    assert!(i.set_on(ProfileTarget::Group, profile(2), now).is_ok());
    assert!(i.set_off(channel(0), profile(1), now).is_ok());
    // And once it ends the same one can be asked again.
    i.on_message(&enabled(CiDeviceId::Group, profile(1), 0));
    assert!(i.set_on(ProfileTarget::Group, profile(1), now).is_ok());
}

#[test]
fn details_resolves_on_the_reply_that_echoes_the_profile_and_target() {
    let (mut i, now) = known();
    let target = InquiryTarget::NUMBER_OF_CHANNELS;
    let actions = i.details(channel(2), profile(1), target, now).unwrap();
    let [
        ProfileAction::Send(CiMessage::ProfileDetailsInquiry {
            header,
            to,
            profile: p,
            target: t,
        }),
    ] = &actions[..]
    else {
        panic!("a Details Inquiry, got {actions:?}");
    };
    assert_eq!(
        (header.device_id, *to, *p, *t),
        (channel(2), Muid::of(PEER), profile(1), target)
    );

    let data = ChannelsDetail {
        in_use: ProfileChannels::of(0),
        max: ProfileChannels::of(6),
    }
    .to_data();
    let answer = |address, p, target, to| {
        Decoded::Message(CiMessage::ProfileDetailsReply {
            header: from_peer(address),
            to: Muid::of(to),
            profile: p,
            target,
            data: data.clone(),
        })
    };
    // Another target, Profile, address or destination is not the answer.
    for wrong in [
        answer(
            channel(2),
            profile(1),
            InquiryTarget::new(U7::of(0x40)),
            OURS,
        ),
        answer(channel(2), profile(2), target, OURS),
        answer(channel(3), profile(1), target, OURS),
        answer(channel(2), profile(1), target, OTHER),
    ] {
        assert!(i.on_message(&wrong).is_empty());
    }
    let actions = i.on_message(&answer(channel(2), profile(1), target, OURS));
    assert_eq!(
        actions,
        [ProfileAction::Resolved {
            request: Request::Details {
                address: channel(2),
                profile: profile(1),
                target
            },
            outcome: Outcome::Details(data),
        }]
    );
    assert_eq!(i.deadline(), None);
}

#[test]
fn details_is_refused_by_a_nak_0x04_and_times_out() {
    let (mut i, now) = known();
    let target = InquiryTarget::NUMBER_OF_CHANNELS;
    i.details(channel(2), profile(1), target, now).unwrap();
    let actions = i.on_message(&nak_with(
        channel(2),
        0x28,
        Nak::STATUS_PROFILE_NOT_SUPPORTED,
        profile(1).to_bytes(),
    ));
    assert!(matches!(
        &actions[..],
        [ProfileAction::Resolved { outcome: Outcome::Refused(n), .. }] if n.status_code == Nak::STATUS_PROFILE_NOT_SUPPORTED
    ));
    i.details(channel(2), profile(1), target, now).unwrap();
    assert!(matches!(
        &i.on_timeout(now + PEER_REPLY_TIMEOUT)[..],
        [ProfileAction::Resolved {
            outcome: Outcome::TimedOut,
            request: Request::Details { .. }
        }]
    ));
}

#[test]
fn profile_specific_data_from_the_peer_is_handed_out_whole() {
    let (mut i, _, _) = start();
    let data = SpecificData::new(vec![1, 2, 3]).unwrap();
    for to in [Destination::Broadcast, Destination::Muid(Muid::of(OURS))] {
        let message = Decoded::Message(CiMessage::ProfileSpecificData {
            header: header_from(PEER, channel(9)),
            to,
            profile: profile(1),
            data: data.clone(),
        });
        assert_eq!(
            i.on_message(&message),
            [ProfileAction::SpecificData {
                address: channel(9),
                profile: profile(1),
                data: data.clone()
            }]
        );
    }
}

#[test]
fn profile_specific_data_for_another_muid_or_from_another_source_is_not_ours() {
    let (mut i, _, _) = start();
    for (source, to) in [(PEER, OTHER), (OTHER, OURS)] {
        let message = Decoded::Message(CiMessage::ProfileSpecificData {
            header: header_from(source, channel(0)),
            to: Destination::Muid(Muid::of(to)),
            profile: profile(1),
            data: SpecificData::default(),
        });
        assert!(i.on_message(&message).is_empty());
    }
}

#[test]
fn the_wire_of_what_it_sends_decodes_back() {
    // The messages it sends are messages a peer's decoder reads.
    let (mut i, first, now) = start();
    let mut sent: Vec<CiMessage> = first
        .into_iter()
        .map(|a| match a {
            ProfileAction::Send(m) => m,
            other => panic!("{other:?}"),
        })
        .collect();
    for actions in [
        i.set_on(
            ProfileTarget::Channel {
                channel: MidiChannel::of(1),
                channels: ProfileChannels::of(2),
            },
            profile(1),
            now,
        )
        .unwrap(),
        i.set_off(CiDeviceId::Group, profile(2), now).unwrap(),
        i.details(
            channel(0),
            profile(3),
            InquiryTarget::NUMBER_OF_CHANNELS,
            now,
        )
        .unwrap(),
    ] {
        sent.extend(actions.into_iter().filter_map(|a| match a {
            ProfileAction::Send(m) => Some(m),
            _ => None,
        }));
    }
    for message in sent {
        assert_eq!(
            CiMessage::decode(&message.encode()),
            Decoded::Message(message)
        );
    }
}

// §5.10, Table 14: Timeout Wait, Status Data in multiples of 100 ms, added to
// the time left (M2-103 §12.2.1).
#[test]
fn a_timeout_wait_adds_its_time_to_a_sets_deadline() {
    let (mut i, now) = known();
    i.set_on(ProfileTarget::Group, profile(1), now).unwrap();
    // 50 x 100 ms = 5 s, added to the 3 s the set had.
    assert!(
        i.on_message(&wait_with(
            CiDeviceId::Group,
            0x22,
            50,
            profile(1).to_bytes()
        ))
        .is_empty()
    );
    assert_eq!(
        i.deadline(),
        Some(now + PEER_REPLY_TIMEOUT + Duration::from_secs(5))
    );
    // Not timed out at 3 s, nor at 4 s, and the Report still resolves it.
    assert!(i.on_timeout(now + PEER_REPLY_TIMEOUT).is_empty());
    let at_4s = now + Duration::from_secs(4);
    assert!(i.on_timeout(at_4s).is_empty());
    let actions = i.on_message(&enabled(CiDeviceId::Group, profile(1), 0));
    assert!(matches!(
        actions.last(),
        Some(ProfileAction::Resolved {
            outcome: Outcome::Reported { .. },
            ..
        })
    ));
}

#[test]
fn a_timeout_wait_that_is_not_for_the_set_extends_nothing() {
    let (mut i, now) = known();
    i.set_on(ProfileTarget::Group, profile(1), now).unwrap();
    let before = i.deadline();
    for ack in [
        // Another Profile, another address, another message, another source, another destination.
        wait_with(CiDeviceId::Group, 0x22, 100, profile(2).to_bytes()),
        wait_with(channel(0), 0x22, 100, profile(1).to_bytes()),
        wait_with(CiDeviceId::Group, 0x23, 100, profile(1).to_bytes()),
        Decoded::Message(CiMessage::Ack {
            header: header_from(OTHER, CiDeviceId::Group),
            to: Muid::of(OURS),
            ack: Ack::new(U7::of(0x22), Ack::STATUS_TIMEOUT_WAIT)
                .with_status_data(U7::of(100))
                .with_details(profile(1).to_bytes().map(U7::of)),
        }),
        Decoded::Message(CiMessage::Ack {
            header: header_from(PEER, CiDeviceId::Group),
            to: Muid::of(OTHER),
            ack: Ack::new(U7::of(0x22), Ack::STATUS_TIMEOUT_WAIT)
                .with_status_data(U7::of(100))
                .with_details(profile(1).to_bytes().map(U7::of)),
        }),
    ] {
        assert!(i.on_message(&ack).is_empty());
        assert_eq!(i.deadline(), before);
    }
}

#[test]
fn a_zero_wait_changes_nothing_and_each_wait_adds_its_own() {
    // Table 14: the time to wait is 0 to 12.7 seconds, added to the time left
    // (M2-103 §12.2.1): more time, never less.
    let (mut i, now) = known();
    i.set_on(ProfileTarget::Group, profile(1), now).unwrap();
    let before = i.deadline().unwrap();
    i.on_message(&wait_with(
        CiDeviceId::Group,
        0x22,
        0,
        profile(1).to_bytes(),
    ));
    assert_eq!(i.deadline(), Some(before));
    for _ in 0..2 {
        i.on_message(&wait_with(
            CiDeviceId::Group,
            0x22,
            5,
            profile(1).to_bytes(),
        ));
    }
    assert_eq!(i.deadline(), Some(before + Duration::from_secs(1)));
}

#[test]
fn a_success_ack_does_not_end_a_set() {
    // The answer to a Set is the Report (§7.10, M2-102 §2.6), not an ACK 0x00.
    let (mut i, now) = known();
    i.set_on(ProfileTarget::Group, profile(1), now).unwrap();
    let ack = Decoded::Message(CiMessage::Ack {
        header: header_from(PEER, CiDeviceId::Group),
        to: Muid::of(OURS),
        ack: Ack::new(U7::of(0x22), Ack::STATUS_ACK)
            .with_details(profile(1).to_bytes().map(U7::of)),
    });
    assert!(i.on_message(&ack).is_empty());
    assert_eq!(i.deadline(), Some(now + PEER_REPLY_TIMEOUT));
}

#[test]
fn a_timeout_wait_for_nothing_in_flight_is_ignored() {
    let (mut i, _) = known();
    assert!(
        i.on_message(&wait_with(
            CiDeviceId::Group,
            0x22,
            50,
            profile(1).to_bytes()
        ))
        .is_empty()
    );
    assert!(
        i.on_message(&wait(CiDeviceId::FunctionBlock, 0x20, 50))
            .is_empty()
    );
    assert_eq!(i.deadline(), None);
}

#[test]
fn a_timeout_wait_extends_a_details_request() {
    let (mut i, now) = known();
    let target = InquiryTarget::NUMBER_OF_CHANNELS;
    i.details(channel(2), profile(1), target, now).unwrap();
    i.on_message(&wait_with(channel(2), 0x28, 100, profile(1).to_bytes()));
    assert_eq!(
        i.deadline(),
        Some(now + PEER_REPLY_TIMEOUT + Duration::from_secs(10))
    );
}

#[test]
fn an_inquiry_with_a_timeout_wait_outlives_the_profile_timeout() {
    let (mut i, _, now) = start();
    i.on_message(&wait(CiDeviceId::FunctionBlock, 0x20, 100));
    assert!(i.on_timeout(now + PEER_REPLY_TIMEOUT).is_empty());
    assert!(i.on_timeout(now + Duration::from_secs(9)).is_empty());
    // The set still completes at the final Reply, and a wait past the end is the timeout.
    let actions = i.on_message(&reply(CiDeviceId::FunctionBlock, &[], &[]));
    assert_eq!(actions, [ProfileAction::ProfilesKnown]);
    let (mut i, _, now) = start();
    i.on_message(&wait(CiDeviceId::FunctionBlock, 0x20, 100));
    assert!(
        i.on_timeout(now + PEER_REPLY_TIMEOUT + Duration::from_secs(10) - Duration::from_nanos(1))
            .is_empty()
    );
    assert_eq!(
        i.on_timeout(now + PEER_REPLY_TIMEOUT + Duration::from_secs(10)),
        [ProfileAction::InquiryTimedOut]
    );
}

// tutti's own Responder (`CiDevice`) has no Profiles: an Initiator's inquiry of it
// completes at the final Reply on 0x7F with nothing in the table (§7.13, M2-102 §2.4).
#[test]
fn an_inquiry_of_tuttis_own_responder_completes_with_an_empty_table() {
    use tutti_ump::ManufacturerId;
    let now = Instant::now();
    let identity = DeviceIdentity {
        manufacturer: ManufacturerId::of(0x41),
        family: [U7::of(1), U7::of(2)],
        model: [U7::of(3), U7::of(4)],
        revision: [U7::of(5), U7::of(6), U7::of(7), U7::of(8)],
    };
    let (mut device, _) = CiDevice::new(
        identity,
        CiCategories::empty(),
        MaxSysexSize::of(512),
        || 0x0200_0002,
        now,
    );
    assert_eq!(device.muid(), Muid::of(PEER));
    let (mut initiator, mut to_device) = ProfileInitiator::new(Muid::of(OURS), device.muid(), now);

    let mut events = Vec::new();
    for _ in 0..4 {
        if to_device.is_empty() {
            break;
        }
        let mut to_initiator = Vec::new();
        for action in to_device.drain(..) {
            let ProfileAction::Send(message) = action else {
                panic!("only sends, got {action:?}");
            };
            // Across the wire: encode, decode.
            for reply in device.on_message(&CiMessage::decode(&message.encode()), now) {
                if let CiAction::Send(reply) = reply {
                    to_initiator.push(reply);
                }
            }
        }
        for message in to_initiator {
            events.extend(initiator.on_message(&CiMessage::decode(&message.encode())));
        }
    }
    assert_eq!(events, [ProfileAction::ProfilesKnown]);
    assert!(initiator.is_known());
    assert!(initiator.table().is_empty());
    assert_eq!(initiator.deadline(), None);
}
