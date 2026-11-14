//! The Initiator against the Responder, and against a scripted peer: Get, Set,
//! pagination and Subscriptions as M2-103 has them. Each test names the property of
//! the spec it holds.

use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::time::Instant;

use tutti_ump::{Bank, U7};

use super::super::capabilities::{PeCapabilities, PeCiMessage, SimultaneousRequests};
use super::super::chunk::{PeRequestId, split_message};
use super::super::flow::PeMessageKind;
use super::super::header::{
    PeEncoding, PeEndedBy, PeHeader, PePage, PeReply, PeStatus, PeSubscription, PeUpdateCommand,
};
use super::super::initiator::{PeAction, PeCall, PeGet, PeInitiator, PeOutcome, PeSet, PeSettings};
use super::super::limits::{MediaType, ResourceId, ResourceName, SubscribeId};
use super::super::partial::{JsonPointer, PartialUpdate};
use super::super::reassemble::{BufferLimit, PeFlowControl, PeReassemblyLimits, StreamLimit};
use super::super::tree::PeJson;
use super::*;
use crate::ci::testing::{OURS, PEER};
use crate::{BodyLimit, CiHeader, CiMessage, Decoded, MaxSysexSize, Muid};

pub(super) fn json(text: &str) -> PeJson {
    PeJson::parse(text.as_bytes()).unwrap()
}

pub(super) fn id(text: &str) -> ResourceId {
    ResourceId::new(text).unwrap()
}

pub(super) fn text(text: &str) -> PeText {
    PeText::new(text).unwrap()
}

pub(super) fn custom(name: &str) -> PeResource {
    PeResource::Custom(CustomResource::new(name).unwrap())
}

pub(super) fn target(resource: PeResource) -> ResourceTarget {
    ResourceTarget::new(resource).unwrap()
}

pub(super) fn entry_target(resource: PeResource, res_id: &str) -> ResourceTarget {
    ResourceTarget::entry(resource, id(res_id)).unwrap()
}

pub(super) fn get(resource: PeResource) -> ResourceGet {
    ResourceGet::new(target(resource))
}

pub(super) fn pointer(text: &str) -> JsonPointer {
    JsonPointer::new(text).unwrap()
}

pub(super) fn program(n: u8) -> PeProgramEntry {
    PeProgramEntry::new(
        text(&format!("Program {n}")),
        PeBankProgram {
            bank: Bank {
                msb: U7::of(0),
                lsb: U7::of(0),
            },
            program: U7::of(n),
        },
    )
}

pub(super) fn channel(n: u16, title: &str) -> PeChannelEntry {
    PeChannelEntry::new(text(title), PeChannel::new(n).unwrap())
}

pub(super) fn device_info() -> PeDeviceInfo {
    PeDeviceInfo::from_json_for_tests()
}

impl PeDeviceInfo {
    /// M2-105 Table 6's data.
    pub(super) fn from_json_for_tests() -> Self {
        let table_6 = json(
            r#"{"version":"1.0","manufacturerId":[125,0,0],"manufacturer":"Educational Use",
            "familyId":[0,0],"family":"Test Range","modelId":[48,0],"model":"MIDI-CI Test Workbench",
            "versionId":[0,0,1,0],"links":[{"resource":"X-SystemSettings"}]}"#,
        );
        match PeData::decode(&PeResource::DeviceInfo, &table_6) {
            Ok(PeData::DeviceInfo(info)) => info,
            other => panic!("{other:?}"),
        }
    }
}

pub(super) fn program_edit() -> PeData {
    // M2-103 Table 32.
    PeData::decode(
        &custom("X-ProgramEdit"),
        &json(
            r#"{"name":"PIANO 4","lfoSpeed":30,"lfoWaveform":"triangle",
            "pitchEnvelope":{"rates":[94,67,95,60],"levels":[50,50,50,50]}}"#,
        ),
    )
    .unwrap()
}

/// A device with the Resources the tests use.
pub(super) fn store() -> ResourceStore {
    let mut store = ResourceStore::new();
    store.declare(ResourceEntry::new(PeResource::DeviceInfo).unwrap());
    store.put(None, PeData::DeviceInfo(device_info())).unwrap();
    store.declare(
        ResourceEntry::new(PeResource::ChannelList)
            .unwrap()
            .with_can_subscribe(true)
            .with_fc_on_get(true)
            .unwrap()
            .with_encodings(vec![
                PeEncoding::Ascii,
                PeEncoding::Mcoded7,
                PeEncoding::ZlibMcoded7,
            ])
            .unwrap(),
    );
    store
        .put(
            None,
            PeData::ChannelList(PeChannelList::new(vec![
                channel(1, "Lead"),
                channel(10, "Drums"),
            ])),
        )
        .unwrap();
    store.declare(ResourceEntry::new(PeResource::ProgramList).unwrap());
    store
        .put(
            Some(id("general")),
            PeData::ProgramList(PeProgramList::new((0..7).map(program).collect())),
        )
        .unwrap();
    store.declare(ResourceEntry::new(PeResource::LocalOn).unwrap());
    store.put(None, PeData::LocalOn(PeLocalOn(false))).unwrap();
    store.declare(ResourceEntry::new(PeResource::ChannelMode).unwrap());
    store
        .put(
            None,
            PeData::ChannelMode(PeChannelMode(PeMidiMode::OmniOffPoly)),
        )
        .unwrap();
    store.declare(
        ResourceEntry::custom(custom("X-ProgramEdit"), json(r#"{"title":"Program Edit"}"#))
            .unwrap()
            .with_can_set(CanSet::Partial)
            .unwrap()
            .with_require_res_id(true)
            .unwrap()
            .with_can_subscribe(true),
    );
    store.put(Some(id("abcd")), program_edit()).unwrap();
    for (name, flow) in [("X-Blob", false), ("X-BigBlob", true)] {
        let blob = ResourceEntry::custom(custom(name), json(r#"{"title":"Blob"}"#))
            .unwrap()
            .with_can_set(CanSet::Full)
            .unwrap()
            .with_fc_on_set(flow)
            .unwrap()
            .with_can_subscribe(true)
            .with_media_types(vec![octets()])
            .unwrap()
            .with_encodings(vec![PeEncoding::Mcoded7])
            .unwrap();
        store.declare(blob);
        store.put(None, blob_data(name, &blob_bytes(300))).unwrap();
    }
    store
}

pub(super) fn octets() -> MediaType {
    MediaType::new("application/octet-stream").unwrap()
}

/// `len` bytes, the top bit of some set, which only an encoding carries.
pub(super) fn blob_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|n| (n * 7 % 256) as u8).collect()
}

pub(super) fn blob_data(name: &str, bytes: &[u8]) -> PeData {
    PeData::Opaque(PeOpaque::binary(custom(name), octets(), bytes.to_vec()).unwrap())
}

pub(super) fn initiator_settings(max: u32) -> PeSettings {
    initiator_settings_taking(max, 4)
}

/// Settings of an Initiator that takes `requests` Subscription messages at once.
pub(super) fn initiator_settings_taking(max: u32, requests: u8) -> PeSettings {
    PeSettings {
        capabilities: PeCapabilities::new(SimultaneousRequests::of(requests)),
        peer_max_sysex: MaxSysexSize::of(max),
        reassembly: PeReassemblyLimits {
            streams: StreamLimit::new(8).unwrap(),
            bytes: BufferLimit::new(1 << 20).unwrap(),
        },
        body_limit: BodyLimit::new(1 << 16).unwrap(),
    }
}

pub(super) fn responder_settings() -> ResponderSettings {
    ResponderSettings {
        capabilities: PeCapabilities::new(SimultaneousRequests::of(4)),
        reassembly: PeReassemblyLimits {
            streams: StreamLimit::new(8).unwrap(),
            bytes: BufferLimit::new(1 << 20).unwrap(),
        },
        body_limit: BodyLimit::new(1 << 16).unwrap(),
    }
}

/// An Initiator and a Responder joined by a wire that delivers every message.
pub(super) struct Link {
    pub(super) init: ResourceInitiator,
    pub(super) resp: ResourceResponder,
    pub(super) now: Instant,
    pub(super) events: Vec<ResourceEvent>,
    pub(super) served: Vec<ResponderEvent>,
    /// Every message, with `true` when it went to the Responder.
    pub(super) wire: Vec<(bool, CiMessage)>,
}

impl Link {
    pub(super) fn new(store: ResourceStore, max: u32) -> Self {
        Self::taking(store, max, 4)
    }

    /// A link whose Initiator takes `requests` Subscription messages at once.
    pub(super) fn taking(store: ResourceStore, max: u32, requests: u8) -> Self {
        let now = Instant::now();
        let (init, actions) = ResourceInitiator::new(
            Muid::of(OURS),
            Muid::of(PEER),
            initiator_settings_taking(max, requests),
            now,
        );
        let mut resp = ResourceResponder::new(Muid::of(PEER), responder_settings(), store);
        resp.accept(Muid::of(OURS), MaxSysexSize::of(max));
        let mut link = Self {
            init,
            resp,
            now,
            events: Vec::new(),
            served: Vec::new(),
            wire: Vec::new(),
        };
        link.run(actions, Vec::new());
        link
    }

    /// Delivers what each side sent, and what that provoked, until the wire is quiet.
    pub(super) fn run(&mut self, from_init: Vec<ResourceAction>, from_resp: Vec<ResponderAction>) {
        let mut to_resp: VecDeque<CiMessage> = VecDeque::new();
        let mut to_init: VecDeque<CiMessage> = VecDeque::new();
        self.absorb_init(from_init, &mut to_resp);
        self.absorb_resp(from_resp, &mut to_init);
        loop {
            if let Some(message) = to_resp.pop_front() {
                self.wire.push((true, message.clone()));
                let actions = self.resp.on_message(&Decoded::Message(message), self.now);
                self.absorb_resp(actions, &mut to_init);
            } else if let Some(message) = to_init.pop_front() {
                self.wire.push((false, message.clone()));
                let actions = self.init.on_message(&Decoded::Message(message), self.now);
                self.absorb_init(actions, &mut to_resp);
            } else {
                break;
            }
        }
    }

    pub(super) fn absorb_init(
        &mut self,
        actions: Vec<ResourceAction>,
        to_resp: &mut VecDeque<CiMessage>,
    ) {
        for action in actions {
            match action {
                ResourceAction::Send(message) => to_resp.push_back(message),
                ResourceAction::Event(event) => self.events.push(*event),
            }
        }
    }

    pub(super) fn absorb_resp(
        &mut self,
        actions: Vec<ResponderAction>,
        to_init: &mut VecDeque<CiMessage>,
    ) {
        for action in actions {
            match action {
                ResponderAction::Send(message) => to_init.push_back(message),
                ResponderAction::Event(event) => self.served.push(event),
            }
        }
    }

    pub(super) fn call(
        &mut self,
        made: Result<(CallId, Vec<ResourceAction>), CallError>,
    ) -> CallId {
        let (call, actions) = made.unwrap();
        self.run(actions, Vec::new());
        call
    }

    pub(super) fn publish(&mut self, actions: Vec<ResponderAction>) {
        self.run(Vec::new(), actions);
    }

    /// The result of the Get that call `call` made.
    pub(super) fn got(&self, call: CallId) -> Result<PeData, ResourceFailure> {
        self.events
            .iter()
            .find_map(|e| match e {
                ResourceEvent::Got {
                    call: c, result, ..
                } if *c == call => Some(result.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no Got for {call:?}: {:?}", self.events))
    }

    pub(super) fn set_result(&self, call: CallId) -> Result<PeStatus, ResourceFailure> {
        self.events
            .iter()
            .find_map(|e| match e {
                ResourceEvent::Set {
                    call: c, result, ..
                } if *c == call => Some(result.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no Set for {call:?}: {:?}", self.events))
    }

    /// The headers of the Get inquiries the Initiator sent, in order.
    pub(super) fn get_requests(&self) -> Vec<super::super::header::PeRequest> {
        self.wire
            .iter()
            .filter_map(|(to_resp, message)| match message {
                CiMessage::PropertyExchange {
                    message: PeCiMessage::Chunk { kind, chunk },
                    ..
                } if *to_resp
                    && *kind == PeMessageKind::GET_INQUIRY
                    && !chunk.header().is_empty() =>
                {
                    match PeHeader::parse(chunk.header()) {
                        Ok(PeHeader::Request(request)) => Some(request),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect()
    }

    pub(super) fn updates(&self) -> Vec<(SubscribeId, PeData, UpdateKind)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ResourceEvent::Updated {
                    subscribe_id,
                    data,
                    how,
                    ..
                } => Some((subscribe_id.clone(), data.clone(), *how)),
                _ => None,
            })
            .collect()
    }
}

// M2-103 §3.2 steps 2 and 3: after the Capabilities Reply the Initiator gets
// ResourceList, which has an entry for each Resource and none for itself.
#[test]
fn the_resource_list_is_got_after_the_capabilities() {
    let link = Link::new(store(), 256);
    assert!(matches!(link.events[0], ResourceEvent::Ready(_)));
    let ResourceEvent::ResourceList(list) = &link.events[1] else {
        panic!("{:?}", link.events);
    };
    assert_eq!(*list, link.resp.store().resource_list());
    assert_eq!(link.init.resource_list(), Some(list));
    assert!(list.entry(&PeResource::ResourceList).is_none());
    assert!(list.entry(&PeResource::DeviceInfo).is_some());
    assert!(
        list.entry(&custom("X-ProgramEdit"))
            .unwrap()
            .require_res_id()
    );
}

// M2-103 §8.2 Tables 17 and 18, M2-105 Table 6: Get DeviceInfo.
#[test]
fn get_returns_the_data_of_the_resource() {
    let mut link = Link::new(store(), 256);
    let made = link.init.get(&get(PeResource::DeviceInfo), link.now);
    let call = link.call(made);
    assert_eq!(link.got(call), Ok(PeData::DeviceInfo(device_info())));
}

// M2-103 §6.1.6, Table 12: the data comes back in the encoding the Get named, ASCII,
// Mcoded7 or zlib and Mcoded7, and the reply names it (§7.3).
#[test]
fn get_returns_the_data_in_each_encoding_the_entry_lists() {
    let mut link = Link::new(store(), 128);
    for encoding in [
        PeEncoding::Ascii,
        PeEncoding::Mcoded7,
        PeEncoding::ZlibMcoded7,
    ] {
        let made = link
            .init
            .get(&get(PeResource::ChannelList).encoded(encoding), link.now);
        let call = link.call(made);
        let Ok(PeData::ChannelList(list)) = link.got(call) else {
            panic!("{encoding:?}: {:?}", link.events);
        };
        assert_eq!(list.entries().len(), 2, "{encoding:?}");
        assert_eq!(list.entries()[1].channel.get(), 10);
    }
}

// M2-103 §14.2: an encoding the entry does not list is refused before it is sent.
#[test]
fn an_unlisted_encoding_is_refused_by_the_initiator() {
    let mut link = Link::new(store(), 256);
    let sent = link.wire.len();
    let made = link.init.get(
        &get(PeResource::DeviceInfo).encoded(PeEncoding::Mcoded7),
        link.now,
    );
    assert!(matches!(
        made,
        Err(CallError::Refused(
            RequestRefused::EncodingNotDeclared { .. }
        ))
    ));
    assert_eq!(link.wire.len(), sent);
}

// M2-103 §14.2: a Resource the ResourceList does not have is refused before it is sent.
#[test]
fn a_resource_not_in_the_list_is_refused_by_the_initiator() {
    let mut link = Link::new(store(), 256);
    let made = link.init.get(&get(PeResource::ModeList), link.now);
    assert!(matches!(
        made,
        Err(CallError::Refused(RequestRefused::NotListed(_)))
    ));
}

/// A Responder and a Get-and-Set Initiator of the transaction layer, without the
/// checks of the ResourceList: what the Responder does with a request it should
/// refuse.
pub(super) struct Raw {
    pub(super) init: PeInitiator,
    pub(super) resp: ResourceResponder,
    pub(super) now: Instant,
}

impl Raw {
    pub(super) fn new() -> Self {
        let now = Instant::now();
        let (init, actions) =
            PeInitiator::new(Muid::of(OURS), Muid::of(PEER), initiator_settings(256), now);
        let mut resp = ResourceResponder::new(Muid::of(PEER), responder_settings(), store());
        resp.accept(Muid::of(OURS), MaxSysexSize::of(256));
        let mut raw = Self { init, resp, now };
        raw.pump(actions);
        raw
    }

    pub(super) fn pump(&mut self, actions: Vec<PeAction>) -> Vec<PeAction> {
        let mut seen = Vec::new();
        let mut queue: VecDeque<PeAction> = actions.into();
        while let Some(action) = queue.pop_front() {
            match action {
                PeAction::Send(message) => {
                    for reply in self.resp.on_message(&Decoded::Message(message), self.now) {
                        if let ResponderAction::Send(m) = reply {
                            queue.extend(self.init.on_message(&Decoded::Message(m), self.now));
                        }
                    }
                }
                other => seen.push(other),
            }
        }
        seen
    }

    pub(super) fn outcome(&mut self, call: PeCall) -> PeOutcome {
        let (_, actions) = self.init.start(call, self.now).unwrap();
        let seen = self.pump(actions);
        seen.into_iter()
            .find_map(|a| match a {
                PeAction::Finished { outcome, .. } => Some(outcome),
                _ => None,
            })
            .expect("the request ended")
    }

    pub(super) fn status(&mut self, call: PeCall) -> PeStatus {
        match self.outcome(call) {
            PeOutcome::Success(r) | PeOutcome::Retry(r) | PeOutcome::Failed(r) => r.status(),
            other => panic!("{other:?}"),
        }
    }
}

pub(super) fn pe_get(name: &str) -> PeGet {
    PeGet::new(ResourceName::new(name).unwrap())
}

// M2-103 Table 15: 404 is a Resource the device does not support or find, and the
// same for an entry of one it has.
#[test]
fn the_responder_answers_404_for_what_it_does_not_have() {
    let mut raw = Raw::new();
    assert_eq!(
        raw.status(PeCall::Get(pe_get("ModeList"))),
        PeStatus::NotFound
    );
    let mut other_entry = pe_get("ProgramList");
    other_entry.res_id = ResourceId::new("nothere");
    assert_eq!(raw.status(PeCall::Get(other_entry)), PeStatus::NotFound);
}

// M2-103 §8.5, Table 15: an inquiry that breaks the rule of `resId`, or asks a
// Resource that does not paginate for a page, is 400.
#[test]
fn the_responder_answers_400_for_a_request_that_breaks_a_rule() {
    let mut raw = Raw::new();
    // ProgramList needs a resId.
    assert_eq!(
        raw.status(PeCall::Get(pe_get("ProgramList"))),
        PeStatus::BadRequest
    );
    // DeviceInfo has none.
    let mut with_id = pe_get("DeviceInfo");
    with_id.res_id = ResourceId::new("x");
    assert_eq!(raw.status(PeCall::Get(with_id)), PeStatus::BadRequest);
    // DeviceInfo does not paginate.
    let mut paged = pe_get("DeviceInfo");
    paged.page = Some(PePage {
        offset: 0,
        limit: NonZeroU32::new(1).unwrap(),
    });
    assert_eq!(raw.status(PeCall::Get(paged)), PeStatus::BadRequest);
}

// M2-103 Table 15: 415 is an encoding the Resource does not support.
#[test]
fn the_responder_answers_415_for_an_encoding_the_entry_does_not_list() {
    let mut raw = Raw::new();
    let mut get = pe_get("DeviceInfo");
    get.encoding = PeEncoding::Mcoded7;
    assert_eq!(raw.status(PeCall::Get(get)), PeStatus::UnsupportedMedia);
}

// M2-103 Table 15, §15.2: 406 is Flow Control that is not supported.
#[test]
fn the_responder_answers_406_for_flow_control_it_does_not_do() {
    let mut raw = Raw::new();
    let mut get = pe_get("DeviceInfo");
    get.flow_control = PeFlowControl::On;
    assert_eq!(
        raw.status(PeCall::Get(get)),
        PeStatus::FlowControlUnsupported
    );
}

// M2-103 §14.2, Table 15: 405 is a Set the entry does not allow, and a
// Subscription to a Resource that is not `canSubscribe`.
#[test]
fn the_responder_answers_405_for_a_set_or_subscribe_the_entry_does_not_allow() {
    let mut raw = Raw::new();
    let set = PeSet::new(ResourceName::new("DeviceInfo").unwrap(), b"{}".to_vec());
    assert_eq!(raw.status(PeCall::Set(set)), PeStatus::NotAllowed);
    let resource_list = PeSet::new(ResourceName::new("ResourceList").unwrap(), b"[]".to_vec());
    assert_eq!(raw.status(PeCall::Set(resource_list)), PeStatus::NotAllowed);
    assert_eq!(
        raw.status(PeCall::Subscribe(pe_get("DeviceInfo"))),
        PeStatus::NotAllowed
    );
    // A Partial Set to a Resource whose `canSet` is `full`.
    let mut partial = PeSet::new(
        ResourceName::new("LocalOn").unwrap(),
        br#"{"":true}"#.to_vec(),
    );
    partial.scope = super::super::header::PeSetScope::Partial;
    assert_eq!(raw.status(PeCall::Set(partial)), PeStatus::NotAllowed);
}

// M2-103 Table 15: 400 is Property Data that is not right.
#[test]
fn the_responder_answers_400_for_data_that_is_not_the_resources() {
    let mut raw = Raw::new();
    for body in ["1", r#""yes""#, "{", ""] {
        let set = PeSet::new(
            ResourceName::new("LocalOn").unwrap(),
            body.as_bytes().to_vec(),
        );
        assert_eq!(
            raw.status(PeCall::Set(set)),
            PeStatus::BadRequest,
            "{body:?}"
        );
    }
    let mut partial = PeSet::new(
        ResourceName::new("X-ProgramEdit").unwrap(),
        br#"{"/nowhere/x":1}"#.to_vec(),
    );
    partial.res_id = ResourceId::new("abcd");
    partial.scope = super::super::header::PeSetScope::Partial;
    assert_eq!(raw.status(PeCall::Set(partial)), PeStatus::BadRequest);
}

// M2-103 §8.6.2 Tables 19 to 22: a page is `limit` entries from `offset`, and the reply
// gives the `totalCount` of the list.
#[test]
fn a_page_is_a_slice_of_the_list_with_its_total_count() {
    let mut raw = Raw::new();
    let mut get = pe_get("ProgramList");
    get.res_id = ResourceId::new("general");
    get.page = Some(PePage {
        offset: 5,
        limit: NonZeroU32::new(5).unwrap(),
    });
    let PeOutcome::Success(response) = raw.outcome(PeCall::Get(get)) else {
        panic!("not a success");
    };
    assert_eq!(response.header.total_count, Some(7));
    let page = PeJson::parse(&response.body).unwrap();
    assert_eq!(page.as_array().unwrap().len(), 2);
    // Past the end is an empty page, with the same total.
    let mut past = pe_get("ProgramList");
    past.res_id = ResourceId::new("general");
    past.page = Some(PePage {
        offset: 50,
        limit: NonZeroU32::new(5).unwrap(),
    });
    let PeOutcome::Success(response) = raw.outcome(PeCall::Get(past)) else {
        panic!("not a success");
    };
    assert_eq!(response.header.total_count, Some(7));
    assert_eq!(
        PeJson::parse(&response.body).unwrap(),
        PeJson::Array(vec![])
    );
}

// M2-103 §8.6.2: the walk asks for offset 0, then for the entries after those it
// has, and ends with `totalCount` of them, in order.
#[test]
fn the_walk_gathers_the_pages_of_a_list() {
    let mut link = Link::new(store(), 256);
    let get = ResourceGet::new(entry_target(PeResource::ProgramList, "general"));
    let made = link
        .init
        .get_list(&get, NonZeroU32::new(3).unwrap(), link.now);
    let call = link.call(made);
    let Ok(PeData::ProgramList(list)) = link.got(call) else {
        panic!("{:?}", link.events);
    };
    let titles: Vec<&str> = list.entries().iter().map(|p| p.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "Program 0",
            "Program 1",
            "Program 2",
            "Program 3",
            "Program 4",
            "Program 5",
            "Program 6"
        ]
    );
    let pages: Vec<(u32, u32)> = link
        .get_requests()
        .iter()
        .filter(|r| r.resource.as_str() == "ProgramList")
        .map(|r| {
            let page = r.page.unwrap();
            (page.offset, page.limit.get())
        })
        .collect();
    assert_eq!(pages, [(0, 3), (3, 3), (6, 3)]);
}

// M2-103 §8.6.2: "If offset and limit are not used, the Property Data shall contain
// the whole list": a list whose entry does not paginate is got in one Get.
#[test]
fn a_list_that_does_not_paginate_is_got_whole() {
    let mut link = Link::new(store(), 256);
    let made = link.init.get_list(
        &get(PeResource::ChannelList),
        NonZeroU32::new(1).unwrap(),
        link.now,
    );
    let call = link.call(made);
    let Ok(PeData::ChannelList(list)) = link.got(call) else {
        panic!("{:?}", link.events);
    };
    assert_eq!(list.entries().len(), 2);
    let requests = link.get_requests();
    let channel_gets = requests
        .iter()
        .filter(|r| r.resource.as_str() == "ChannelList")
        .count();
    assert_eq!(channel_gets, 1);
    assert!(requests.iter().all(|r| r.page.is_none()));
    // A Resource that is no list is not walked.
    assert!(matches!(
        link.init.get_list(
            &get(PeResource::LocalOn),
            NonZeroU32::new(1).unwrap(),
            link.now
        ),
        Err(CallError::Refused(RequestRefused::NotPaginable(_)))
    ));
}

/// An Initiator whose peer is the test: it answers what the test says.
pub(super) struct Script {
    pub(super) init: ResourceInitiator,
    pub(super) now: Instant,
    pub(super) events: Vec<ResourceEvent>,
    pub(super) sent: Vec<CiMessage>,
}

impl Script {
    /// An Initiator that has the peer's capabilities and a ResourceList of `list`.
    pub(super) fn new(list: &PeResourceList) -> Self {
        let now = Instant::now();
        let (init, actions) =
            ResourceInitiator::new(Muid::of(OURS), Muid::of(PEER), initiator_settings(512), now);
        let mut script = Self {
            init,
            now,
            events: Vec::new(),
            sent: Vec::new(),
        };
        script.take(actions);
        let caps = PeCiMessage::CapabilitiesReply(PeCapabilities::new(SimultaneousRequests::of(4)));
        script.peer_sends(caps);
        let request = script.last_request_id();
        script.reply(
            request,
            PeStatus::Ok,
            None,
            &PeData::ResourceList(list.clone()).to_json().unwrap(),
        );
        script.events.clear();
        script.sent.clear();
        script
    }

    pub(super) fn take(&mut self, actions: Vec<ResourceAction>) {
        for action in actions {
            match action {
                ResourceAction::Send(m) => self.sent.push(m),
                ResourceAction::Event(e) => self.events.push(*e),
            }
        }
    }

    pub(super) fn peer_sends(&mut self, message: PeCiMessage) {
        let decoded = Decoded::Message(CiMessage::PropertyExchange {
            header: CiHeader::new(Muid::of(PEER)),
            to: Muid::of(OURS),
            message,
        });
        let actions = self.init.on_message(&decoded, self.now);
        self.take(actions);
    }

    pub(super) fn last_request_id(&self) -> PeRequestId {
        self.sent
            .iter()
            .rev()
            .find_map(|m| match m {
                CiMessage::PropertyExchange {
                    message: PeCiMessage::Chunk { chunk, .. },
                    ..
                } => Some(chunk.request_id()),
                _ => None,
            })
            .unwrap()
    }

    /// The Request ID of the Get inquiry sent last.
    pub(super) fn last_get_id(&self) -> PeRequestId {
        self.sent
            .iter()
            .rev()
            .find_map(|m| match m {
                CiMessage::PropertyExchange {
                    message: PeCiMessage::Chunk { kind, chunk },
                    ..
                } if *kind == PeMessageKind::GET_INQUIRY => Some(chunk.request_id()),
                _ => None,
            })
            .unwrap()
    }

    /// The Reply to Get of `status`, and `body`, to request `id`.
    pub(super) fn reply(
        &mut self,
        id: PeRequestId,
        status: PeStatus,
        total: Option<u32>,
        body: &PeJson,
    ) {
        let mut header = PeReply::new(status);
        header.total_count = total;
        self.reply_with(id, PeMessageKind::GET_REPLY, header, body.encode());
    }

    pub(super) fn reply_with(
        &mut self,
        id: PeRequestId,
        kind: PeMessageKind,
        header: PeReply,
        body: Vec<u8>,
    ) {
        let chunks =
            split_message(id, &PeHeader::Reply(header), &body, MaxSysexSize::of(512)).unwrap();
        for chunk in chunks {
            self.peer_sends(PeCiMessage::Chunk { kind, chunk });
        }
    }

    /// A Subscription message of the peer, with `body`.
    pub(super) fn subscription(&mut self, id: u8, header: PeSubscription, body: &[u8]) {
        let chunks = split_message(
            PeRequestId::of(id),
            &PeHeader::Subscription(header),
            body,
            MaxSysexSize::of(512),
        )
        .unwrap();
        for chunk in chunks {
            self.peer_sends(PeCiMessage::Chunk {
                kind: PeMessageKind::SUBSCRIPTION_INQUIRY,
                chunk,
            });
        }
    }
}

pub(super) fn paginating_list() -> PeResourceList {
    PeResourceList::new().with(ResourceEntry::new(PeResource::ProgramList).unwrap())
}

pub(super) fn program_page(from: u8, to: u8) -> PeJson {
    PeData::ProgramList(PeProgramList::new((from..to).map(program).collect()))
        .to_json()
        .unwrap()
}

// M2-103 §8.6.2: a reply to a paginated Get shall have `totalCount`.
#[test]
fn a_page_without_a_total_count_fails_the_walk() {
    let mut script = Script::new(&paginating_list());
    let get = ResourceGet::new(entry_target(PeResource::ProgramList, "g"));
    let (call, actions) = script
        .init
        .get_list(&get, NonZeroU32::new(3).unwrap(), script.now)
        .unwrap();
    script.take(actions);
    let id = script.last_request_id();
    script.reply(id, PeStatus::Ok, None, &program_page(0, 3));
    assert!(matches!(
        script.events.as_slice(),
        [ResourceEvent::Got { call: c, result: Err(ResourceFailure::Pagination(PaginationFault::NoTotalCount)), .. }]
            if *c == call
    ));
}

// A page with no entry before `totalCount` are in is a list that does not end: the
// walk fails and does not ask again.
#[test]
fn an_empty_page_before_the_end_fails_the_walk() {
    let mut script = Script::new(&paginating_list());
    let get = ResourceGet::new(entry_target(PeResource::ProgramList, "g"));
    let (_, actions) = script
        .init
        .get_list(&get, NonZeroU32::new(3).unwrap(), script.now)
        .unwrap();
    script.take(actions);
    let id = script.last_request_id();
    script.reply(id, PeStatus::Ok, Some(9), &program_page(0, 3));
    let next = script.last_request_id();
    script.events.clear();
    script.reply(next, PeStatus::Ok, Some(9), &program_page(0, 0));
    assert!(matches!(
        script.events.as_slice(),
        [ResourceEvent::Got {
            result: Err(ResourceFailure::Pagination(PaginationFault::EmptyPage {
                have: 3,
                total: 9
            })),
            ..
        }]
    ));
}

// A reply that is not a success ends a walk with that status.
#[test]
fn a_failed_page_fails_the_walk_with_its_status() {
    let mut script = Script::new(&paginating_list());
    let get = ResourceGet::new(entry_target(PeResource::ProgramList, "g"));
    let (_, actions) = script
        .init
        .get_list(&get, NonZeroU32::new(3).unwrap(), script.now)
        .unwrap();
    script.take(actions);
    let id = script.last_request_id();
    script.reply(id, PeStatus::NotFound, None, &PeJson::Null);
    let [
        ResourceEvent::Got {
            result: Err(ResourceFailure::Peer(outcome)),
            ..
        },
    ] = script.events.as_slice()
    else {
        panic!("{:?}", script.events);
    };
    assert!(matches!(**outcome, PeOutcome::Failed(ref r) if r.status() == PeStatus::NotFound));
}

// M2-103 §8.7, M2-109 §2.3 Table: Set LocalOn, then Get it.
#[test]
fn set_replaces_the_data_and_get_reads_it_back() {
    let mut link = Link::new(store(), 256);
    let set = ResourceSet::full(
        target(PeResource::LocalOn),
        PeData::LocalOn(PeLocalOn(true)),
    )
    .unwrap();
    let made = link.init.set(&set, link.now);
    let call = link.call(made);
    assert_eq!(link.set_result(call), Ok(PeStatus::Ok));
    assert!(matches!(
        link.served.last(),
        Some(ResponderEvent::Changed {
            how: ChangeKind::Full,
            ..
        })
    ));
    let made = link.init.get(&get(PeResource::LocalOn), link.now);
    let call = link.call(made);
    assert_eq!(link.got(call), Ok(PeData::LocalOn(PeLocalOn(true))));
}

// M2-103 §10.2 Tables 31 to 38: a Partial Set changes the two elements and the
// Get after it has them.
#[test]
fn a_partial_set_changes_what_a_later_get_returns() {
    let mut link = Link::new(store(), 256);
    let program = entry_target(custom("X-ProgramEdit"), "abcd");
    let update = PartialUpdate::new()
        .with(pointer("/pitchEnvelope/rates/0"), PeJson::Integer(80))
        .unwrap()
        .with(pointer("/pitchEnvelope/levels/0"), PeJson::Integer(60))
        .unwrap();
    let made = link
        .init
        .set(&ResourceSet::partial(program.clone(), update), link.now);
    let call = link.call(made);
    assert_eq!(link.set_result(call), Ok(PeStatus::Ok));
    assert!(matches!(
        link.served.last(),
        Some(ResponderEvent::Changed {
            how: ChangeKind::Partial,
            ..
        })
    ));
    let made = link.init.get(&ResourceGet::new(program), link.now);
    let call = link.call(made);
    let expected = PeData::decode(
        &custom("X-ProgramEdit"),
        &json(
            r#"{"name":"PIANO 4","lfoSpeed":30,"lfoWaveform":"triangle",
            "pitchEnvelope":{"rates":[80,67,95,60],"levels":[60,50,50,50]}}"#,
        ),
    )
    .unwrap();
    assert_eq!(link.got(call), Ok(expected));
}

// M2-103 §10: a Set the entry does not allow is not sent; and one the Responder
// refuses comes back with its status.
#[test]
fn a_set_the_entry_does_not_allow_is_refused_before_it_is_sent() {
    let mut link = Link::new(store(), 256);
    let sent = link.wire.len();
    let set = ResourceSet::full(
        target(PeResource::ChannelMode),
        PeData::ChannelMode(PeChannelMode(PeMidiMode::OmniOnPoly)),
    )
    .unwrap();
    assert!(matches!(
        link.init.set(&set, link.now),
        Err(CallError::Refused(RequestRefused::CannotSet { .. }))
    ));
    assert_eq!(link.wire.len(), sent);
}

// M2-103 §11.1, §11.4: Start Subscription gets a `subscribeId` from the Responder, the
// Initiator has the data, and a `full` update replaces it.
#[test]
fn a_full_update_replaces_the_data_of_a_subscription() {
    let mut link = Link::new(store(), 256);
    let made = link.init.subscribe(&get(PeResource::ChannelList), link.now);
    let call = link.call(made);
    let Some(ResourceEvent::Subscribed { subscribe_id, .. }) = link
        .events
        .iter()
        .find(|e| matches!(e, ResourceEvent::Subscribed { call: c, .. } if *c == call))
    else {
        panic!("{:?}", link.events);
    };
    let subscribe_id = subscribe_id.clone();
    assert!(subscribe_id.as_str().len() <= 8);
    let updates = link.updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].2, UpdateKind::Seeded);
    assert_eq!(link.resp.subscriptions().count(), 1);

    let channels = PeData::ChannelList(PeChannelList::new(vec![channel(3, "Strings")]));
    let actions = link
        .resp
        .publish(&target(PeResource::ChannelList), channels.clone(), link.now)
        .unwrap();
    link.publish(actions);
    let updates = link.updates();
    assert_eq!(updates.len(), 2);
    assert_eq!(
        updates[1],
        (subscribe_id.clone(), channels.clone(), UpdateKind::Full)
    );
    // The data the Initiator holds is the Responder's.
    let held = link
        .init
        .subscriptions()
        .find(|(id, _, _)| **id == subscribe_id)
        .and_then(|(_, _, data)| data.cloned());
    assert_eq!(held, Some(channels));
}

// M2-103 §11.1, §10.2: a `partial` update uses the format of a Partial Set, and the
// Initiator applies it to the data it holds.
#[test]
fn a_partial_update_changes_the_data_the_initiator_holds() {
    let mut link = Link::new(store(), 256);
    let program = entry_target(custom("X-ProgramEdit"), "abcd");
    let made = link
        .init
        .subscribe(&ResourceGet::new(program.clone()), link.now);
    link.call(made);
    let update = PartialUpdate::new()
        .with(pointer("/lfoSpeed"), PeJson::Integer(10))
        .unwrap();
    let actions = link.resp.change(&program, update, link.now).unwrap();
    link.publish(actions);
    let updates = link.updates();
    let (_, data, how) = updates.last().unwrap();
    assert_eq!(*how, UpdateKind::Partial);
    let expected = PeData::decode(
        &custom("X-ProgramEdit"),
        &json(
            r#"{"name":"PIANO 4","lfoSpeed":10,"lfoWaveform":"triangle",
            "pitchEnvelope":{"rates":[94,67,95,60],"levels":[50,50,50,50]}}"#,
        ),
    )
    .unwrap();
    assert_eq!(*data, expected);
    // The Responder's data is the changed data too.
    assert_eq!(link.resp.store().get(&program), Some(&expected));
}

// M2-103 §11, §11.1.1: when an Initiator's Set changes the data of a Resource it
// subscribed to, the Responder sends the update to the subscribers.
#[test]
fn a_set_is_announced_to_the_subscribers() {
    let mut link = Link::new(store(), 256);
    let program = entry_target(custom("X-ProgramEdit"), "abcd");
    let made = link
        .init
        .subscribe(&ResourceGet::new(program.clone()), link.now);
    link.call(made);
    let update = PartialUpdate::new()
        .with(pointer("/pitchEnvelope/levels/3"), PeJson::Integer(99))
        .unwrap();
    let made = link
        .init
        .set(&ResourceSet::partial(program, update), link.now);
    link.call(made);
    let updates = link.updates();
    let (_, data, how) = updates.last().unwrap();
    assert_eq!(*how, UpdateKind::Partial);
    let PeData::Opaque(opaque) = data else {
        panic!("not opaque");
    };
    assert_eq!(
        opaque
            .json()
            .unwrap()
            .as_object()
            .unwrap()
            .get("pitchEnvelope"),
        Some(&json(r#"{"rates":[94,67,95,60],"levels":[50,50,50,99]}"#))
    );
}

// M2-103 §11.1.1: when the data does not fit one chunk the Responder sends `notify`,
// and the Initiator gets the Resource again.
#[test]
fn an_update_too_big_for_a_chunk_is_a_notify_and_a_get() {
    let mut link = Link::new(store(), 128);
    let made = link.init.subscribe(&get(PeResource::ChannelList), link.now);
    link.call(made);
    let big: Vec<PeChannelEntry> = (1..=12)
        .map(|n| channel(n, &format!("Channel number {n}")))
        .collect();
    let data = PeData::ChannelList(PeChannelList::new(big));
    let actions = link
        .resp
        .publish(&target(PeResource::ChannelList), data.clone(), link.now)
        .unwrap();
    // The Subscription message the Responder sent is a `notify`, with no data.
    let notify = actions.iter().find_map(|a| match a {
        ResponderAction::Send(CiMessage::PropertyExchange {
            message: PeCiMessage::Chunk { chunk, .. },
            ..
        }) => PeHeader::parse(chunk.header()).ok(),
        _ => None,
    });
    assert!(matches!(
        notify,
        Some(PeHeader::Subscription(PeSubscription::Update {
            command: PeUpdateCommand::Notify,
            ..
        }))
    ));
    link.publish(actions);
    let updates = link.updates();
    let (_, held, how) = updates.last().unwrap();
    assert_eq!(*how, UpdateKind::Refreshed);
    assert_eq!(*held, data);
}

// M2-103 §11.5: the Initiator ends a Subscription, and the Responder's table has none.
#[test]
fn the_initiator_ends_a_subscription() {
    let mut link = Link::new(store(), 256);
    let made = link.init.subscribe(&get(PeResource::ChannelList), link.now);
    link.call(made);
    let (subscribe_id, _, _) = link
        .init
        .subscriptions()
        .next()
        .map(|(i, t, d)| (i.clone(), t.clone(), d.cloned()))
        .unwrap();
    let made = link.init.unsubscribe(&subscribe_id, link.now);
    let call = link.call(made);
    assert!(link.events.iter().any(|e| matches!(
        e,
        ResourceEvent::Unsubscribed { call: c, result: Ok(PeStatus::Ok), .. } if *c == call
    )));
    assert_eq!(link.resp.subscriptions().count(), 0);
    assert_eq!(link.init.subscriptions().count(), 0);
    assert!(matches!(
        link.served.last(),
        Some(ResponderEvent::Unsubscribed {
            by: PeEndedBy::Initiator,
            ..
        })
    ));
    // Nothing is sent to an Initiator that no longer subscribes.
    let before = link.wire.len();
    let actions = link
        .resp
        .publish(
            &target(PeResource::ChannelList),
            PeData::ChannelList(PeChannelList::default()),
            link.now,
        )
        .unwrap();
    assert!(actions.is_empty());
    assert_eq!(link.wire.len(), before);
    assert!(matches!(
        link.init.unsubscribe(&subscribe_id, link.now),
        Err(CallError::NoSubscription(_))
    ));
}

// M2-103 §11.1, §11.5: the Responder ends a Subscription with `end` and
// `endedBy` `responder`.
#[test]
fn the_responder_ends_a_subscription() {
    let mut link = Link::new(store(), 256);
    let made = link.init.subscribe(&get(PeResource::ChannelList), link.now);
    link.call(made);
    let subscribe_id = link.init.subscriptions().next().unwrap().0.clone();
    let actions = link.resp.end_subscription(&subscribe_id, link.now);
    link.publish(actions);
    assert!(link.events.iter().any(|e| matches!(
        e,
        ResourceEvent::SubscriptionEnded { subscribe_id: s, .. } if *s == subscribe_id
    )));
    assert_eq!(link.init.subscriptions().count(), 0);
    assert_eq!(link.resp.subscriptions().count(), 0);
    let end = link.wire.iter().find_map(|(to_resp, m)| match m {
        CiMessage::PropertyExchange {
            message: PeCiMessage::Chunk { chunk, .. },
            ..
        } if !to_resp => match PeHeader::parse(chunk.header()) {
            Ok(PeHeader::Subscription(PeSubscription::End { ended_by, .. })) => Some(ended_by),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(end, Some(Some(PeEndedBy::Responder)));
}

// M2-103 §14.2: a Subscription to a Resource that is not `canSubscribe` is refused.
#[test]
fn a_subscription_the_entry_does_not_allow_is_refused() {
    let mut link = Link::new(store(), 256);
    assert!(matches!(
        link.init.subscribe(&get(PeResource::DeviceInfo), link.now),
        Err(CallError::Refused(RequestRefused::CannotSubscribe(_)))
    ));
}

pub(super) fn subscribed_script() -> (Script, SubscribeId) {
    let list = PeResourceList::new().with(
        ResourceEntry::new(PeResource::LocalOn)
            .unwrap()
            .with_can_subscribe(true),
    );
    let mut script = Script::new(&list);
    let (_, actions) = script
        .init
        .subscribe(&get(PeResource::LocalOn), script.now)
        .unwrap();
    script.take(actions);
    let start = script.last_request_id();
    let mut reply = PeReply::new(PeStatus::Ok);
    let subscribe_id = SubscribeId::new("s1").unwrap();
    reply.subscribe_id = Some(subscribe_id.clone());
    script.reply_with(start, PeMessageKind::SUBSCRIPTION_REPLY, reply, Vec::new());
    let seed = script.last_request_id();
    script.reply(seed, PeStatus::Ok, None, &PeJson::Bool(false));
    (script, subscribe_id)
}

pub(super) fn update(command: PeUpdateCommand, id: &SubscribeId) -> PeSubscription {
    PeSubscription::Update {
        command,
        subscribe_id: id.clone(),
        mutual_encoding: None,
        media_type: None,
        extra: Default::default(),
    }
}

pub(super) fn sent_gets(script: &Script) -> usize {
    script
        .sent
        .iter()
        .filter(|m| {
            matches!(
                m,
                CiMessage::PropertyExchange {
                    message: PeCiMessage::Chunk { kind, .. },
                    ..
                } if *kind == PeMessageKind::GET_INQUIRY
            )
        })
        .count()
}

// M2-103 §11.1: a `full` update of a Simple Property Resource is the value, which is
// the whole Property Data (§11.1.1).
#[test]
fn a_full_update_of_a_simple_resource_is_its_value() {
    let (mut script, id) = subscribed_script();
    script.events.clear();
    script.subscription(5, update(PeUpdateCommand::Full, &id), b"true");
    assert!(matches!(
        script.events.as_slice(),
        [ResourceEvent::Updated {
            data: PeData::LocalOn(PeLocalOn(true)),
            how: UpdateKind::Full,
            ..
        }]
    ));
}

// M2-103 §11.2: an Initiator that receives a `partial` it cannot handle replies with
// success and Gets the Resource to stay in step.
#[test]
fn an_update_that_cannot_be_applied_is_answered_and_the_resource_got_again() {
    let (mut script, id) = subscribed_script();
    script.events.clear();
    let gets = sent_gets(&script);
    // The pointer names no place in a boolean.
    script.subscription(
        6,
        update(PeUpdateCommand::Partial, &id),
        br#"{"/nowhere":1}"#,
    );
    // The reply to the Subscription message is a success.
    let reply = script.sent.iter().rev().find_map(|m| match m {
        CiMessage::PropertyExchange {
            message: PeCiMessage::Chunk { kind, chunk },
            ..
        } if *kind == PeMessageKind::SUBSCRIPTION_REPLY => match PeHeader::parse(chunk.header()) {
            Ok(PeHeader::Reply(r)) => Some(r.status),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(reply, Some(PeStatus::Ok));
    assert_eq!(sent_gets(&script), gets + 1);
    let get_id = script.last_get_id();
    script.reply(get_id, PeStatus::Ok, None, &PeJson::Bool(true));
    assert!(matches!(
        script.events.as_slice(),
        [ResourceEvent::Updated {
            data: PeData::LocalOn(PeLocalOn(true)),
            how: UpdateKind::Refreshed,
            ..
        }]
    ));
    // The same for a `full` whose data is not the Resource's, and for `notify`.
    script.events.clear();
    script.subscription(7, update(PeUpdateCommand::Full, &id), b"\"yes\"");
    assert_eq!(sent_gets(&script), gets + 2);
    script.subscription(8, update(PeUpdateCommand::Notify, &id), b"");
    assert_eq!(sent_gets(&script), gets + 3);
}

// M2-103 §11.2: a Subscription message for a Subscription the device does not hold
// is answered with a failure.
#[test]
fn an_update_for_an_unknown_subscription_is_answered_with_400() {
    let (mut script, _) = subscribed_script();
    script.subscription(
        9,
        update(PeUpdateCommand::Full, &SubscribeId::new("zz").unwrap()),
        b"true",
    );
    let reply = script.sent.iter().rev().find_map(|m| match m {
        CiMessage::PropertyExchange {
            message: PeCiMessage::Chunk { kind, chunk },
            ..
        } if *kind == PeMessageKind::SUBSCRIPTION_REPLY => match PeHeader::parse(chunk.header()) {
            Ok(PeHeader::Reply(r)) => Some(r.status),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(reply, Some(PeStatus::BadRequest));
}

// M2-103 §11.2: a failure in reply to a Subscription message may end the
// Subscription, which this Responder does.
#[test]
fn a_failed_reply_to_an_update_ends_the_subscription() {
    let mut link = Link::new(store(), 256);
    let made = link.init.subscribe(&get(PeResource::ChannelList), link.now);
    link.call(made);
    let data = PeData::ChannelList(PeChannelList::new(vec![channel(2, "Next")]));
    let actions = link
        .resp
        .publish(&target(PeResource::ChannelList), data, link.now)
        .unwrap();
    let request_id = actions
        .iter()
        .find_map(|a| match a {
            ResponderAction::Send(CiMessage::PropertyExchange {
                message: PeCiMessage::Chunk { chunk, .. },
                ..
            }) => Some(chunk.request_id()),
            _ => None,
        })
        .unwrap();
    // The Initiator never saw the update: the reply is the test's.
    let reply = PeReply::new(PeStatus::BadRequest);
    let chunks = split_message(
        request_id,
        &PeHeader::Reply(reply),
        &[],
        MaxSysexSize::of(256),
    )
    .unwrap();
    let mut out = Vec::new();
    for chunk in chunks {
        let message = CiMessage::PropertyExchange {
            header: CiHeader::new(Muid::of(OURS)),
            to: Muid::of(PEER),
            message: PeCiMessage::Chunk {
                kind: PeMessageKind::SUBSCRIPTION_REPLY,
                chunk,
            },
        };
        out.extend(link.resp.on_message(&Decoded::Message(message), link.now));
    }
    assert_eq!(link.resp.subscriptions().count(), 0);
    assert!(out.iter().any(|a| matches!(
        a,
        ResponderAction::Event(ResponderEvent::Unsubscribed {
            by: PeEndedBy::Responder,
            ..
        })
    )));
}

// M2-101 §8: a message from a device the Responder has not been told of is not
// answered, and a device it forgets has no Subscriptions (M2-103 §11.5).
#[test]
fn a_responder_serves_only_the_peers_it_accepted() {
    let mut link = Link::new(store(), 256);
    let stranger = CiMessage::PropertyExchange {
        header: CiHeader::new(Muid::of(0x0400_0004)),
        to: Muid::of(PEER),
        message: PeCiMessage::CapabilitiesInquiry(PeCapabilities::new(SimultaneousRequests::of(1))),
    };
    assert!(
        link.resp
            .on_message(&Decoded::Message(stranger), link.now)
            .is_empty()
    );
    let made = link.init.subscribe(&get(PeResource::ChannelList), link.now);
    link.call(made);
    assert_eq!(link.resp.subscriptions().count(), 1);
    link.resp.forget(Muid::of(OURS));
    assert_eq!(link.resp.subscriptions().count(), 0);
}

// M2-103 §7.4.1, Table 15: a Reply uses a status of the table, and 400 carries a hint.
#[test]
fn a_refusal_carries_a_message() {
    let mut raw = Raw::new();
    let PeOutcome::Failed(response) = raw.outcome(PeCall::Get(pe_get("ProgramList"))) else {
        panic!("not a failure");
    };
    assert!(response.header.message.is_some());
}
