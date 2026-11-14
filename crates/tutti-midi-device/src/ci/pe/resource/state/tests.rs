//! StateList and State (M2-111). Each test names the property of the spec it holds.

use std::num::NonZeroU32;

use tutti_ump::{DeviceIdentity, ManufacturerId, U7};

use super::super::super::capabilities::PeCiMessage;
use super::super::super::flow::PeMessageKind;
use super::super::super::header::{PeEncoding, PeHeader, PeStatus};
use super::super::super::limits::{MediaType, PropertyName};
use super::super::super::tree::PeJson;
use super::super::super::value::PeExtra;
use super::super::exchange::{Link, octets, target};
use super::super::*;
use crate::CiMessage;

fn json(text: &str) -> PeJson {
    PeJson::parse(text.as_bytes()).unwrap()
}

fn state_id(text: &str) -> StateId {
    StateId::new(text).unwrap()
}

fn text(t: &str) -> PeText {
    PeText::new(t).unwrap()
}

/// The device the states of these tests come from: manufacturer 7D, family 0,
/// model 48, version 1.0.
fn device() -> DeviceIdentity {
    DeviceIdentity {
        manufacturer: ManufacturerId::from_sysex_bytes([U7::of(0x7D), U7::of(0), U7::of(0)])
            .unwrap(),
        family: [U7::of(0), U7::of(0)],
        model: [U7::of(48), U7::of(0)],
        revision: [U7::of(0), U7::of(0), U7::of(1), U7::of(0)],
    }
}

fn other_version() -> DeviceIdentity {
    DeviceIdentity {
        revision: [U7::of(0), U7::of(0), U7::of(2), U7::of(0)],
        ..device()
    }
}

/// `len` bytes, the top bit of some set.
fn bytes(len: usize) -> Vec<u8> {
    (0..len).map(|n| (n * 13 % 256) as u8).collect()
}

fn stored(len: usize) -> PeState {
    PeState::octets(bytes(len))
        .with_rev(StateRev::new("buui890adj").unwrap())
        .with_timestamp(UnixSeconds::of(1_586_786_400))
}

fn store_with(states: &[(&str, PeState)]) -> ResourceStore {
    let mut store = ResourceStore::new();
    store.declare_states();
    for (id, state) in states {
        let entry = PeStateEntry::new(state_id(id), text(id));
        store.put_state(entry, state.clone()).unwrap();
    }
    store
}

fn page() -> NonZeroU32 {
    NonZeroU32::new(8).unwrap()
}

// M2-111 §2.3, Table 3 and its example: the list's properties read, and written back
// as they came.
#[test]
fn a_state_list_entry_has_the_properties_of_table_3() {
    let text = r#"[{"title":"User Programs","stateId":"userPrograms","stateRev":"ahd822nkdla","timestamp":1583330400,"description":"all","size":300}]"#;
    let lower = text.to_owned();
    let PeData::StateList(list) = PeData::decode(&PeResource::StateList, &json(&lower)).unwrap()
    else {
        panic!("not a StateList");
    };
    let entry = &list.entries()[0];
    assert_eq!(entry.title.as_str(), "User Programs");
    assert_eq!(entry.state_id.as_str(), "userPrograms");
    assert_eq!(entry.rev.as_ref().unwrap().as_str(), "ahd822nkdla");
    assert_eq!(entry.timestamp.unwrap().get(), 1_583_330_400);
    assert_eq!(entry.size.unwrap().get(), 300);
    assert_eq!(PeData::StateList(list).to_json(), Some(json(&lower)));
}

// M2-111 §2.3, Table 3: `title` and `stateId` are required, the rest optional.
#[test]
fn a_state_list_entry_requires_title_and_state_id() {
    let decode = |t: &str| PeData::decode(&PeResource::StateList, &json(t));
    assert!(decode(r#"[{"title":"A","stateId":"a"}]"#).is_ok());
    for bad in [r#"[{"stateId":"a"}]"#, r#"[{"title":"A"}]"#] {
        let error = decode(bad).unwrap_err();
        assert_eq!(error.fault, PeDataFault::Missing, "{bad}");
    }
}

// M2-111 §2.3 and Appendix A: a stateId is 1 to 36 letters, digits and `_`; the
// spec's own ids have capitals.
#[test]
fn a_state_id_is_36_of_letters_digits_and_underscore() {
    assert!(StateId::new("full_state_2").is_some());
    assert!(StateId::new("fullState").is_some());
    assert!(StateId::new(&"a".repeat(36)).is_some());
    for bad in ["", "a-b", "a b", "é", &"a".repeat(37)] {
        assert!(StateId::new(bad).is_none(), "{bad:?}");
    }
    let error = PeData::decode(
        &PeResource::StateList,
        &json(r#"[{"title":"A","stateId":"B-d"}]"#),
    )
    .unwrap_err();
    assert_eq!(error.fault, PeDataFault::NotValid("state id"));
}

// M2-111 §2.3: `timestamp` and `size` are integers, never below zero.
#[test]
fn a_state_list_entry_refuses_a_negative_time_and_size() {
    for bad in [
        r#"[{"title":"A","stateId":"a","timestamp":-1}]"#,
        r#"[{"title":"A","stateId":"a","size":-5}]"#,
        r#"[{"title":"A","stateId":"a","size":"big"}]"#,
    ] {
        assert!(
            PeData::decode(&PeResource::StateList, &json(bad)).is_err(),
            "{bad}"
        );
    }
}

// M2-111 Appendix A: the suggested ids are valid state ids, and each is found again.
#[test]
fn the_suggested_state_ids_are_state_ids() {
    for state in [
        CommonState::Full,
        CommonState::AllNonSystem,
        CommonState::Buffer,
        CommonState::System,
        CommonState::Programs,
        CommonState::Sequences,
        CommonState::Arpeggios,
        CommonState::Samples,
        CommonState::Effects,
    ] {
        assert!(StateId::new(state.as_str()).is_some());
        let named = CommonState::new(state.as_str());
        assert_eq!(named, Some(state));
    }
    assert_eq!(CommonState::Buffer.as_str(), "buffer");
    assert_eq!(CommonState::new("elsewhere"), None);
}

// M2-111 §2.4, §3.4: StateList is a List Resource that is read only, does not
// paginate and takes no resId; State is got and set whole, with a resId, and is
// not subscribed to.
#[test]
fn the_resources_have_the_defaults_of_the_entries() {
    let list = PeResource::StateList.defaults();
    assert!(list.can_get && !list.can_subscribe && !list.can_paginate && !list.require_res_id);
    assert_eq!(list.can_set, CanSet::None);
    assert!(PeResource::StateList.is_list());
    assert_eq!(PeResource::StateList.res_id_use(), ResIdUse::Unused);

    let state = PeResource::State.defaults();
    assert!(state.can_get && !state.can_subscribe && state.require_res_id);
    assert_eq!(state.can_set, CanSet::Full);
    assert!(!PeResource::State.is_list());
    assert_eq!(PeResource::State.res_id_use(), ResIdUse::Required);
    assert!(!PeResource::State.is_overridable() && !PeResource::StateList.is_overridable());
}

// M2-111 §3.4: the entry of State lists Mcoded7 and `application/octet-stream`.
#[test]
fn the_state_entry_lists_mcoded7_and_octet_stream() {
    let entry = ResourceEntry::device_state();
    assert_eq!(entry.encodings(), &[PeEncoding::Mcoded7]);
    assert_eq!(entry.media_types(), vec![octets()]);
    assert_eq!(entry.can_set(), CanSet::Full);
    assert!(entry.require_res_id() && entry.can_get() && !entry.can_subscribe());
    // The spec's fixed properties stay fixed.
    assert!(entry.clone().with_can_set(CanSet::Partial).is_err());
    assert!(entry.with_require_res_id(false).is_err());
}

// M2-111 §2.2, §3.2, §3.3: list the states, get one, and set it back; the body is
// 3000 bytes under Flow Control so it takes many chunks each way.
#[test]
fn a_state_is_listed_fetched_and_restored_over_many_chunks() {
    let original = stored(3000);
    let mut store = store_with(&[("buffer", original.clone()), ("system", stored(40))]);
    store.declare(
        ResourceEntry::device_state()
            .with_fc_on_get(true)
            .unwrap()
            .with_fc_on_set(true)
            .unwrap(),
    );
    let mut link = Link::new(store, 256);

    let made = link.init.list_states(page(), link.now);
    let call = link.call(made);
    let Ok(PeData::StateList(list)) = link.got(call) else {
        panic!("{:?}", link.events);
    };
    assert_eq!(list.entries().len(), 2);
    let buffer = list.entry(&state_id("buffer")).unwrap();
    assert_eq!(buffer.size.unwrap().get(), 3000);
    assert_eq!(buffer.rev.as_ref().unwrap().as_str(), "buui890adj");

    let made = link.init.get_state(&state_id("buffer"), link.now);
    let call = link.call(made);
    let Ok(PeData::State(got)) = link.got(call) else {
        panic!("{:?}", link.events);
    };
    assert_eq!(got, original);
    let chunks_to_initiator = chunks(&link, false, PeMessageKind::GET_REPLY);
    assert!(chunks_to_initiator > 10, "{chunks_to_initiator} chunks");

    // The responder's copy is changed, then the Initiator restores what it saved.
    let saved = SavedState::new(device(), state_id("buffer"), got);
    let changed = PeState::octets(bytes(50));
    link.resp
        .publish(
            &ResourceTarget::entry(PeResource::State, state_id("buffer").resource_id()).unwrap(),
            PeData::State(changed.clone()),
            link.now,
        )
        .unwrap();
    let (call, actions) = link
        .init
        .restore_state(&saved, &device(), link.now)
        .unwrap();
    link.run(actions, Vec::new());
    assert_eq!(link.set_result(call), Ok(PeStatus::Ok));
    let chunks_to_responder = chunks(&link, true, PeMessageKind::SET_INQUIRY);
    assert!(chunks_to_responder > 10, "{chunks_to_responder} chunks");
    let held = link
        .resp
        .store()
        .get(&ResourceTarget::entry(PeResource::State, state_id("buffer").resource_id()).unwrap())
        .unwrap();
    let PeData::State(held) = held else {
        panic!("not a State");
    };
    assert_eq!(held.to_wire(), original.to_wire());
    assert!(matches!(
        link.served.last(),
        Some(ResponderEvent::Changed {
            how: ChangeKind::Full,
            ..
        })
    ));
}

fn chunks(link: &Link, to_responder: bool, kind: PeMessageKind) -> usize {
    link.wire
        .iter()
        .filter(|(to, message)| {
            *to == to_responder
                && matches!(
                    message,
                    CiMessage::PropertyExchange {
                        message: PeCiMessage::Chunk { kind: k, .. },
                        ..
                    } if *k == kind
                )
        })
        .count()
}

// M2-111 §3.2: the reply to a Get of State names the media type and carries the
// state's revision and time in its header.
#[test]
fn the_reply_header_carries_the_media_type_state_rev_and_timestamp() {
    let mut link = Link::new(store_with(&[("buffer", stored(10))]), 4096);
    let made = link.init.get_state(&state_id("buffer"), link.now);
    link.call(made);
    let reply = link
        .wire
        .iter()
        .find_map(|(to, message)| match message {
            CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { kind, chunk },
                ..
            } if !*to && *kind == PeMessageKind::GET_REPLY && !chunk.header().is_empty() => {
                match PeHeader::parse(chunk.header()) {
                    Ok(PeHeader::Reply(reply)) if reply.media_type.is_some() => Some(reply),
                    _ => None,
                }
            }
            _ => None,
        })
        .expect("a State reply");
    assert_eq!(reply.media_type, Some(octets()));
    assert_eq!(
        reply.extra.get("stateRev"),
        Some(&PeJson::from("buui890adj"))
    );
    assert_eq!(
        reply.extra.get("timestamp"),
        Some(&PeJson::Integer(1_586_786_400))
    );
}

// M2-111 §3.1.1: a reply whose `timestamp` is no time is not a state.
#[test]
fn a_reply_header_with_a_bad_timestamp_is_refused() {
    let mut extra = PeExtra::default();
    extra
        .insert(
            PropertyName::new("timestamp").unwrap(),
            PeJson::from("noon"),
        )
        .unwrap();
    let error = PeState::octets(vec![1]).read_header(&extra).unwrap_err();
    assert_eq!(error.place, "State.timestamp");
    let mut extra = PeExtra::default();
    extra
        .insert(PropertyName::new("timestamp").unwrap(), PeJson::Integer(-3))
        .unwrap();
    assert!(PeState::octets(vec![1]).read_header(&extra).is_err());
}

// M2-103 Table 15 as M2-111 uses it: a state id the device does not have is 404.
#[test]
fn a_state_the_device_does_not_have_is_answered_404() {
    let mut link = Link::new(store_with(&[("buffer", stored(10))]), 256);
    let made = link.init.get_state(&state_id("samples"), link.now);
    let call = link.call(made);
    let Err(ResourceFailure::Peer(outcome)) = link.got(call) else {
        panic!("{:?}", link.events);
    };
    let super::super::super::initiator::PeOutcome::Failed(response) = *outcome else {
        panic!("not a failure");
    };
    assert_eq!(response.status(), PeStatus::NotFound);
}

// M2-111 §3.4: State lists `application/octet-stream`, so a state of another media
// type is refused before it is sent.
#[test]
fn a_state_of_an_unlisted_media_type_is_refused_before_it_is_sent() {
    let mut link = Link::new(store_with(&[("buffer", stored(10))]), 256);
    let wav = MediaType::new("audio/wav").unwrap();
    let state = PeState::binary(wav, bytes(4)).unwrap();
    let saved = SavedState::new(device(), state_id("buffer"), state);
    let sent = link.wire.len();
    let error = link
        .init
        .restore_state(&saved, &device(), link.now)
        .unwrap_err();
    assert!(
        matches!(
            error,
            RestoreError::Call(CallError::Refused(
                RequestRefused::MediaTypeNotDeclared { .. }
            ))
        ),
        "{error:?}"
    );
    assert_eq!(link.wire.len(), sent);
}

// M2-111 §2.4: StateList is `canSet: none`, so a Set of it is refused.
#[test]
fn a_set_of_the_state_list_is_refused() {
    let mut link = Link::new(store_with(&[("buffer", stored(10))]), 256);
    let set = ResourceSet::full(
        target(PeResource::StateList),
        PeData::StateList(PeStateList::default()),
    )
    .unwrap();
    let error = link.init.set(&set, link.now).unwrap_err();
    assert!(matches!(
        error,
        CallError::Refused(RequestRefused::CannotSet { .. })
    ));
}

// M2-111 §1.5: a device that does not declare State has no entry for it, so a restore
// is refused as not listed.
#[test]
fn a_restore_to_a_device_without_state_is_refused_as_not_listed() {
    let mut link = Link::new(ResourceStore::new(), 256);
    let saved = SavedState::new(device(), state_id("buffer"), stored(10));
    let error = link
        .init
        .restore_state(&saved, &device(), link.now)
        .unwrap_err();
    assert!(matches!(
        error,
        RestoreError::Call(CallError::Refused(RequestRefused::NotListed(_)))
    ));
}

// M2-111 §3.1.1, §3.3: a state is for the device it came from, and one of another
// version is not sent.
#[test]
fn a_restore_to_another_device_is_refused_and_sends_nothing() {
    let mut link = Link::new(store_with(&[("buffer", stored(10))]), 256);
    let saved = SavedState::new(device(), state_id("buffer"), stored(10));
    let sent = link.wire.len();
    let error = link
        .init
        .restore_state(&saved, &other_version(), link.now)
        .unwrap_err();
    assert_eq!(
        error,
        RestoreError::WrongDevice(WrongDevice {
            saved: device(),
            peer: other_version(),
        })
    );
    assert_eq!(link.wire.len(), sent);
    assert!(saved.restore(&device()).is_ok());
    assert!(saved.restore(&other_version()).is_err());
}

// M2-111 §3.1.1: when the Responder gives no time, the Initiator tags the state with
// its own; a time the Responder gave stays.
#[test]
fn the_initiator_tags_a_state_without_a_time_and_keeps_the_devices() {
    let bare = SavedState::new(device(), state_id("buffer"), PeState::octets(vec![1]));
    let tagged = bare.clone().stamped(UnixSeconds::of(5));
    assert_eq!(tagged.state().timestamp(), Some(UnixSeconds::of(5)));
    let given = SavedState::new(device(), state_id("buffer"), stored(1));
    let kept = given.clone().stamped(UnixSeconds::of(5));
    assert_eq!(
        kept.state().timestamp(),
        Some(UnixSeconds::of(1_586_786_400))
    );
}

// M2-111 §2.3: the list the store serves follows what is put: an entry is replaced by
// the same id, the size is the body's, and the revision and time are the state's.
#[test]
fn the_store_lists_each_state_it_holds_with_its_size() {
    let mut store = store_with(&[("buffer", stored(10)), ("system", stored(20))]);
    store
        .put_state(
            PeStateEntry::new(state_id("buffer"), text("Buffer")),
            stored(99),
        )
        .unwrap();
    let Some(PeData::StateList(list)) = store.get(&target(PeResource::StateList)) else {
        panic!("no list");
    };
    assert_eq!(list.entries().len(), 2);
    let buffer = list.entry(&state_id("buffer")).unwrap();
    assert_eq!(buffer.size.unwrap().get(), 99);
    assert_eq!(buffer.title.as_str(), "Buffer");
    assert_eq!(buffer.timestamp.unwrap().get(), 1_586_786_400);
    assert_eq!(
        list.entry(&state_id("system")).unwrap().size.unwrap().get(),
        20
    );
}

// M2-111 §3.4: a store that did not declare the states refuses them.
#[test]
fn a_store_without_the_states_declared_refuses_one() {
    let mut store = ResourceStore::new();
    let error = store
        .put_state(PeStateEntry::new(state_id("buffer"), text("B")), stored(1))
        .unwrap_err();
    assert!(matches!(error, StoreError::NotDeclared(_)));
}

// M2-111 §3.2.1: a State body that is JSON is JSON, and is not the bytes of a media
// type.
#[test]
fn a_json_state_is_json_and_an_unnamed_media_type_is_refused() {
    let state = PeState::json(json(r#"{"a":1}"#));
    let (media, wire) = state.to_wire();
    assert_eq!(media, None);
    assert_eq!(PeState::from_wire(None, &wire), Ok(state));
    assert!(PeState::binary(MediaType::new("application/json").unwrap(), vec![]).is_none());
    assert!(PeState::from_wire(None, b"{").is_err());
}

// M2-103 §8.5 as State has it: an inquiry for State names a state with `resId`.
#[test]
fn an_inquiry_for_state_without_a_res_id_is_refused_by_the_type() {
    assert!(matches!(
        ResourceTarget::new(PeResource::State),
        Err(RequestRefused::ResIdRequired(_))
    ));
    assert!(ResourceTarget::entry(PeResource::State, state_id("buffer").resource_id()).is_ok());
    assert!(matches!(
        ResourceTarget::entry(PeResource::StateList, state_id("buffer").resource_id()),
        Err(RequestRefused::ResIdUnused(_))
    ));
}
