//! [`ResourceInitiator`]: the Initiator of Property Exchange speaking in
//! Resources (M2-103 §3.2, §8 to §11, §14), over [`PeInitiator`].

use std::num::NonZeroU32;
use std::time::Instant;

use super::super::capabilities::PeCapabilities;
use super::super::chunk::PeRequestId;
use super::super::header::PeSetScope;
use super::super::header::{PeEncoding, PePage, PeReply, PeStatus};
use super::super::initiator::{
    PeAction, PeCall, PeCapabilitiesFailure, PeChange, PeError, PeGet, PeInitiator, PeKind,
    PeNotification, PeOutcome, PeResponse, PeSet, PeSettings,
};
use super::super::limits::{MediaType, SubscribeId};
use super::super::partial::PartialUpdate;
use super::super::reassemble::PeFlowControl;
use super::super::tree::{PeJson, PeJsonError};
use super::data::PeData;
use super::entry::{PeResourceList, ResourceEntry};
use super::fields::PeDataError;
use super::kind::PeResource;
use super::request::{RequestRefused, ResourceGet, ResourceSet, ResourceTarget, SetBody};
use crate::{CiMessage, Decoded, Muid};

/// The most entries of a List Resource a walk gathers, so a peer that never
/// ends its list cannot make the walk grow without bound.
pub const MAX_LIST_ENTRIES: usize = 1 << 16;

/// Names a call of a [`ResourceInitiator`] in the events it ends with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CallId(u32);

impl CallId {
    /// The number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A call that cannot be made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CallError {
    /// The inquiry breaks a rule of the Resource or of its ResourceList entry.
    #[error(transparent)]
    Refused(#[from] RequestRefused),
    /// The transaction cannot be started.
    #[error(transparent)]
    Pe(#[from] PeError),
    /// The Subscription is not held.
    #[error("no subscription {0}")]
    NoSubscription(SubscribeId),
}

/// A pagination that cannot be completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PaginationFault {
    /// A reply to a paginated Get has no `totalCount`, which the spec has it
    /// carry (M2-103 §8.6.2).
    #[error("the reply has no totalCount")]
    NoTotalCount,
    /// A page has no entry while fewer than `totalCount` have come.
    #[error("a page is empty with {have} of {total} entries")]
    EmptyPage {
        /// How many entries have come.
        have: usize,
        /// The `totalCount`.
        total: u32,
    },
    /// The list is longer than [`MAX_LIST_ENTRIES`].
    #[error("the list is longer than {MAX_LIST_ENTRIES} entries")]
    TooLong,
}

/// Why a call did not end in the data wanted.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ResourceFailure {
    /// The transaction did not end in a reply of success.
    #[error("the transaction ended {0:?}")]
    Peer(Box<PeOutcome>),
    /// The Property Data is not JSON.
    #[error(transparent)]
    Json(#[from] PeJsonError),
    /// The Property Data is not the Resource's.
    #[error(transparent)]
    Data(#[from] PeDataError),
    /// The pages of a list do not make one.
    #[error(transparent)]
    Pagination(#[from] PaginationFault),
    /// A request the call needed next could not be started.
    #[error(transparent)]
    Request(#[from] PeError),
}

/// What an update of a Subscription did to the data held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UpdateKind {
    /// The first Get after the Subscription started.
    Seeded,
    /// A `full` update replaced the data.
    Full,
    /// A `partial` update was applied to the data.
    Partial,
    /// A Get made after a `notify`, or after an update that could not be used,
    /// replaced the data (M2-103 §11.2).
    Refreshed,
}

/// What the Initiator tells its caller happened.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ResourceEvent {
    /// The peer's Capabilities arrived and the ResourceList is being got.
    Ready(PeCapabilities),
    /// The Capabilities exchange failed.
    CapabilitiesFailed(PeCapabilitiesFailure),
    /// The ResourceList arrived. Calls are checked against it from now on.
    ResourceList(PeResourceList),
    /// The ResourceList could not be got.
    ResourceListFailed(ResourceFailure),
    /// A Get, or a walk of a List Resource, ended.
    Got {
        /// The call.
        call: CallId,
        /// What was got.
        target: ResourceTarget,
        /// The data, or why not.
        result: Result<PeData, ResourceFailure>,
    },
    /// A Set ended.
    Set {
        /// The call.
        call: CallId,
        /// The Resource set.
        resource: PeResource,
        /// The status of the reply, 200 or 201, or why not.
        result: Result<PeStatus, ResourceFailure>,
    },
    /// A Subscription started and the Resource is being got.
    Subscribed {
        /// The call.
        call: CallId,
        /// The Subscription.
        subscribe_id: SubscribeId,
        /// The Resource subscribed to.
        target: ResourceTarget,
    },
    /// A Subscription did not start.
    SubscribeFailed {
        /// The call.
        call: CallId,
        /// The Resource asked for.
        target: ResourceTarget,
        /// Why not.
        failure: ResourceFailure,
    },
    /// The Initiator ended a Subscription.
    Unsubscribed {
        /// The call.
        call: CallId,
        /// The Subscription.
        subscribe_id: SubscribeId,
        /// The reply of the Responder, or why it did not come. The Subscription is
        /// over either way.
        result: Result<PeStatus, ResourceFailure>,
    },
    /// The data of a Subscription changed.
    Updated {
        /// The Subscription.
        subscribe_id: SubscribeId,
        /// The Resource.
        target: ResourceTarget,
        /// The data now held.
        data: PeData,
        /// What changed it.
        how: UpdateKind,
    },
    /// The data of a Subscription could not be got after an update that could
    /// not be used, so what is held is out of date.
    SyncFailed {
        /// The Subscription.
        subscribe_id: SubscribeId,
        /// Why.
        failure: ResourceFailure,
    },
    /// The Responder ended a Subscription.
    SubscriptionEnded {
        /// The Subscription.
        subscribe_id: SubscribeId,
        /// The Resource.
        target: ResourceTarget,
    },
}

/// What the driver does next.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ResourceAction {
    /// Send this message to the peer.
    Send(CiMessage),
    /// Something happened.
    Event(Box<ResourceEvent>),
}

fn event(event: ResourceEvent) -> ResourceAction {
    ResourceAction::Event(Box::new(event))
}

#[derive(Debug)]
struct Walk {
    call: CallId,
    target: ResourceTarget,
    encoding: PeEncoding,
    size: NonZeroU32,
    pages: Vec<PeJson>,
    have: usize,
}

#[derive(Debug)]
enum Op {
    List,
    Get {
        call: CallId,
        target: ResourceTarget,
    },
    Walk(Walk),
    Set {
        call: CallId,
        resource: PeResource,
    },
    Subscribe {
        call: CallId,
        target: ResourceTarget,
    },
    Unsubscribe {
        call: CallId,
        subscribe_id: SubscribeId,
    },
    Sync {
        subscribe_id: SubscribeId,
        how: UpdateKind,
    },
}

#[derive(Debug)]
struct Sub {
    id: SubscribeId,
    target: ResourceTarget,
    data: Option<PeData>,
}

/// The Initiator of Property Exchange with one peer, speaking in Resources.
///
/// It runs [`PeInitiator`]'s transactions and gives the Property Data typed:
///
/// - After the Capabilities Reply it gets the ResourceList on its own
///   (M2-103 §3.2), and checks every call against the entry the peer gave the
///   Resource: Get, Set, Partial Set, Subscribe, `resId`, pagination, encoding
///   and Flow Control. Before the list arrives only the rules of the Resource
///   itself apply.
/// - [`get_list`](Self::get_list) walks a List Resource that paginates, page after
///   page, to the whole list.
/// - [`subscribe`](Self::subscribe) starts a Subscription and then holds the
///   data: it gets the Resource once, applies each `full` and `partial` update
///   to it, and gets it again after a `notify` or after an update it cannot
///   use (M2-103 §11.2). Each change is a [`ResourceEvent::Updated`].
#[derive(Debug)]
pub struct ResourceInitiator {
    pe: PeInitiator,
    list: Option<PeResourceList>,
    ops: Vec<(PeRequestId, Op)>,
    subs: Vec<Sub>,
    next_call: u32,
}

impl ResourceInitiator {
    /// An Initiator for `peer`, from this device's MUID `own`, and the actions that
    /// start it at `now`: the Capabilities Inquiry.
    #[must_use]
    pub fn new(
        own: Muid,
        peer: Muid,
        settings: PeSettings,
        now: Instant,
    ) -> (Self, Vec<ResourceAction>) {
        let (pe, actions) = PeInitiator::new(own, peer, settings, now);
        let mut this = Self {
            pe,
            list: None,
            ops: Vec::new(),
            subs: Vec::new(),
            next_call: 0,
        };
        let out = this.map(actions, now);
        (this, out)
    }

    /// The ResourceList of the peer, once it has come.
    #[must_use]
    pub fn resource_list(&self) -> Option<&PeResourceList> {
        self.list.as_ref()
    }

    /// The transaction layer, to read what it holds.
    #[must_use]
    pub fn transactions(&self) -> &PeInitiator {
        &self.pe
    }

    /// The Subscriptions held, with the data each has.
    pub fn subscriptions(
        &self,
    ) -> impl Iterator<Item = (&SubscribeId, &ResourceTarget, Option<&PeData>)> {
        self.subs
            .iter()
            .map(|s| (&s.id, &s.target, s.data.as_ref()))
    }

    /// When [`on_timeout`](Self::on_timeout) is next needed.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.pe.deadline()
    }

    /// Time has passed.
    pub fn on_timeout(&mut self, now: Instant) -> Vec<ResourceAction> {
        let actions = self.pe.on_timeout(now);
        self.map(actions, now)
    }

    /// A CI message of the connection's Group arrived.
    pub fn on_message(&mut self, decoded: &Decoded, now: Instant) -> Vec<ResourceAction> {
        let actions = self.pe.on_message(decoded, now);
        self.map(actions, now)
    }

    fn call_id(&mut self) -> CallId {
        let id = CallId(self.next_call);
        self.next_call = self.next_call.wrapping_add(1);
        id
    }

    /// The entry of the ResourceList for `resource`, when the list is known.
    fn entry(&self, resource: &PeResource) -> Result<Option<&ResourceEntry>, RequestRefused> {
        match &self.list {
            None => Ok(None),
            Some(_) if *resource == PeResource::ResourceList => Ok(None),
            Some(list) => list
                .entry(resource)
                .map(Some)
                .ok_or_else(|| RequestRefused::NotListed(resource.as_str().to_owned())),
        }
    }

    fn start(
        &mut self,
        call: PeCall,
        op: Op,
        now: Instant,
    ) -> Result<Vec<ResourceAction>, PeError> {
        let (id, actions) = self.pe.start(call, now)?;
        self.ops.push((id, op));
        Ok(self.map(actions, now))
    }

    /// Get Property Data.
    ///
    /// # Errors
    ///
    /// [`CallError::Refused`] when the entry of the ResourceList does not allow
    /// the inquiry, [`CallError::Pe`] when it cannot be started.
    pub fn get(
        &mut self,
        get: &ResourceGet,
        now: Instant,
    ) -> Result<(CallId, Vec<ResourceAction>), CallError> {
        if let Some(entry) = self.entry(get.target().resource())? {
            get.check_get(entry)?;
        }
        let call = self.call_id();
        let op = Op::Get {
            call,
            target: get.target().clone(),
        };
        let actions = self.start(PeCall::Get(pe_get(get)), op, now)?;
        Ok((call, actions))
    }

    /// Gets the whole of a List Resource: page by page, of `page_size` entries,
    /// when the entry declares `canPaginate`, and in one Get otherwise.
    ///
    /// The walk asks for offset 0, then for the offset after the entries it has,
    /// until it has `totalCount` of them. A reply with no `totalCount`, or an
    /// empty page before the end, fails the call.
    ///
    /// # Errors
    ///
    /// As for [`get`](Self::get), and [`RequestRefused::NotPaginable`] for a
    /// Resource that is no List Resource.
    pub fn get_list(
        &mut self,
        get: &ResourceGet,
        page_size: NonZeroU32,
        now: Instant,
    ) -> Result<(CallId, Vec<ResourceAction>), CallError> {
        if !get.target().resource().is_list() {
            return Err(
                RequestRefused::NotPaginable(get.target().resource().as_str().to_owned()).into(),
            );
        }
        let paginates = self
            .entry(get.target().resource())?
            .is_some_and(ResourceEntry::can_paginate);
        if !paginates {
            return self.get(get, now);
        }
        let first = get.clone().paged(PePage {
            offset: 0,
            limit: page_size,
        })?;
        if let Some(entry) = self.entry(get.target().resource())? {
            first.check_get(entry)?;
        }
        let call = self.call_id();
        let walk = Walk {
            call,
            target: get.target().clone(),
            encoding: get.encoding(),
            size: page_size,
            pages: Vec::new(),
            have: 0,
        };
        let actions = self.start(PeCall::Get(pe_get(&first)), Op::Walk(walk), now)?;
        Ok((call, actions))
    }

    /// Set Property Data.
    ///
    /// # Errors
    ///
    /// As for [`get`](Self::get).
    pub fn set(
        &mut self,
        set: &ResourceSet,
        now: Instant,
    ) -> Result<(CallId, Vec<ResourceAction>), CallError> {
        let flow = match self.entry(set.target().resource())? {
            Some(entry) => {
                set.check(entry)?;
                if entry.fc_on_set() {
                    PeFlowControl::On
                } else {
                    PeFlowControl::Off
                }
            }
            None => PeFlowControl::Off,
        };
        let (scope, media_type, body) = match set.body() {
            SetBody::Full(data) => {
                let (media_type, bytes) = data.to_wire();
                (PeSetScope::Whole, media_type, bytes)
            }
            SetBody::Partial(update) => (PeSetScope::Partial, None, update.to_json().encode()),
        };
        let call = self.call_id();
        let op = Op::Set {
            call,
            resource: set.target().resource().clone(),
        };
        let pe_set = PeSet {
            resource: set.target().resource().name(),
            res_id: set.target().res_id().cloned(),
            scope,
            encoding: set.encoding(),
            flow_control: flow,
            media_type,
            body,
        };
        let actions = self.start(PeCall::Set(pe_set), op, now)?;
        Ok((call, actions))
    }

    /// Starts a Subscription to a Resource. When it has started the data is got
    /// and held, and every change comes as [`ResourceEvent::Updated`].
    ///
    /// # Errors
    ///
    /// As for [`get`](Self::get), and [`RequestRefused::CannotSubscribe`] for an
    /// entry that is not `canSubscribe`.
    pub fn subscribe(
        &mut self,
        get: &ResourceGet,
        now: Instant,
    ) -> Result<(CallId, Vec<ResourceAction>), CallError> {
        if let Some(entry) = self.entry(get.target().resource())? {
            get.check_subscribe(entry)?;
        }
        let call = self.call_id();
        let op = Op::Subscribe {
            call,
            target: get.target().clone(),
        };
        let actions = self.start(PeCall::Subscribe(pe_get(get)), op, now)?;
        Ok((call, actions))
    }

    /// Ends a Subscription. It no longer holds data from this moment.
    ///
    /// # Errors
    ///
    /// [`CallError::NoSubscription`] for one that is not held.
    pub fn unsubscribe(
        &mut self,
        id: &SubscribeId,
        now: Instant,
    ) -> Result<(CallId, Vec<ResourceAction>), CallError> {
        let at = self
            .subs
            .iter()
            .position(|s| s.id == *id)
            .ok_or_else(|| CallError::NoSubscription(id.clone()))?;
        let call = self.call_id();
        let op = Op::Unsubscribe {
            call,
            subscribe_id: id.clone(),
        };
        let actions = self.start(PeCall::Unsubscribe(id.clone()), op, now)?;
        self.subs.remove(at);
        Ok((call, actions))
    }

    fn map(&mut self, actions: Vec<PeAction>, now: Instant) -> Vec<ResourceAction> {
        let mut out = Vec::new();
        for action in actions {
            match action {
                PeAction::Send(message) => out.push(ResourceAction::Send(message)),
                PeAction::Capabilities(capabilities) => {
                    out.push(event(ResourceEvent::Ready(capabilities)));
                    self.fetch_list(now, &mut out);
                }
                PeAction::CapabilitiesFailed(failure) => {
                    out.push(event(ResourceEvent::CapabilitiesFailed(failure)));
                }
                PeAction::Finished {
                    request_id,
                    kind,
                    outcome,
                } => self.finished(request_id, kind, outcome, now, &mut out),
                PeAction::Notification(notification) => self.notified(notification, now, &mut out),
            }
        }
        out
    }

    fn fetch_list(&mut self, now: Instant, out: &mut Vec<ResourceAction>) {
        let get = PeGet::new(PeResource::ResourceList.name());
        match self.pe.start(PeCall::Get(get), now) {
            Ok((id, actions)) => {
                self.ops.push((id, Op::List));
                let more = self.map(actions, now);
                out.extend(more);
            }
            Err(error) => out.push(event(ResourceEvent::ResourceListFailed(error.into()))),
        }
    }

    fn finished(
        &mut self,
        id: PeRequestId,
        _kind: PeKind,
        outcome: PeOutcome,
        now: Instant,
        out: &mut Vec<ResourceAction>,
    ) {
        let Some(at) = self.ops.iter().position(|(held, _)| *held == id) else {
            return;
        };
        let (_, op) = self.ops.remove(at);
        let reply = match outcome {
            PeOutcome::Success(response) => Ok(response),
            other => Err(ResourceFailure::Peer(Box::new(other))),
        };
        match op {
            Op::List => match reply.and_then(|r| decode(&PeResource::ResourceList, &r)) {
                Ok(PeData::ResourceList(list)) => {
                    self.list = Some(list.clone());
                    out.push(event(ResourceEvent::ResourceList(list)));
                }
                Ok(_) => {}
                Err(failure) => out.push(event(ResourceEvent::ResourceListFailed(failure))),
            },
            Op::Get { call, target } => {
                let result = reply.and_then(|r| decode(target.resource(), &r));
                out.push(event(ResourceEvent::Got {
                    call,
                    target,
                    result,
                }));
            }
            Op::Walk(walk) => self.walked(walk, reply, now, out),
            Op::Set { call, resource } => {
                let result = reply.map(|r| r.status());
                out.push(event(ResourceEvent::Set {
                    call,
                    resource,
                    result,
                }));
            }
            Op::Subscribe { call, target } => match reply {
                // The transaction layer ends a success with no `subscribeId` as
                // unreadable, so a success here has one.
                Ok(PeResponse {
                    header:
                        PeReply {
                            subscribe_id: Some(subscribe_id),
                            ..
                        },
                    ..
                }) => {
                    self.subs.push(Sub {
                        id: subscribe_id.clone(),
                        target: target.clone(),
                        data: None,
                    });
                    out.push(event(ResourceEvent::Subscribed {
                        call,
                        subscribe_id: subscribe_id.clone(),
                        target,
                    }));
                    self.sync(&subscribe_id, UpdateKind::Seeded, now, out);
                }
                Ok(_) => {}
                Err(failure) => out.push(event(ResourceEvent::SubscribeFailed {
                    call,
                    target,
                    failure,
                })),
            },
            Op::Unsubscribe { call, subscribe_id } => {
                out.push(event(ResourceEvent::Unsubscribed {
                    call,
                    subscribe_id,
                    result: reply.map(|r| r.status()),
                }));
            }
            Op::Sync { subscribe_id, how } => {
                let Some(sub) = self.subs.iter_mut().find(|s| s.id == subscribe_id) else {
                    return;
                };
                match reply.and_then(|r| decode(sub.target.resource(), &r)) {
                    Ok(data) => {
                        sub.data = Some(data.clone());
                        out.push(event(ResourceEvent::Updated {
                            subscribe_id,
                            target: sub.target.clone(),
                            data,
                            how,
                        }));
                    }
                    Err(failure) => {
                        sub.data = None;
                        out.push(event(ResourceEvent::SyncFailed {
                            subscribe_id,
                            failure,
                        }));
                    }
                }
            }
        }
    }

    /// A page of a walk arrived: ask for the next, or end the walk.
    fn walked(
        &mut self,
        mut walk: Walk,
        reply: Result<PeResponse, ResourceFailure>,
        now: Instant,
        out: &mut Vec<ResourceAction>,
    ) {
        let step = reply.and_then(|response| {
            let page = parse(&response)?;
            let items = page.as_array().map_or(0, <[PeJson]>::len);
            let total = response
                .header
                .total_count
                .ok_or(PaginationFault::NoTotalCount)?;
            Ok((page, items, total))
        });
        let (page, items, total) = match step {
            Ok(step) => step,
            Err(failure) => return self.walk_ended(walk, Err(failure), out),
        };
        walk.have += items;
        walk.pages.push(page);
        let have = u32::try_from(walk.have).unwrap_or(u32::MAX);
        if walk.have > MAX_LIST_ENTRIES {
            return self.walk_ended(walk, Err(PaginationFault::TooLong.into()), out);
        }
        if have >= total {
            let joined = PeData::decode_pages(walk.target.resource(), &walk.pages)
                .map_err(ResourceFailure::from);
            return self.walk_ended(walk, joined, out);
        }
        if items == 0 {
            let fault = PaginationFault::EmptyPage {
                have: walk.have,
                total,
            };
            return self.walk_ended(walk, Err(fault.into()), out);
        }
        let next = ResourceGet::new(walk.target.clone())
            .encoded(walk.encoding)
            .paged(PePage {
                offset: have,
                limit: walk.size,
            });
        let Ok(next) = next else { return };
        let call = PeCall::Get(pe_get(&next));
        match self.pe.start(call, now) {
            Ok((id, actions)) => {
                self.ops.push((id, Op::Walk(walk)));
                let more = self.map(actions, now);
                out.extend(more);
            }
            Err(error) => self.walk_ended(walk, Err(error.into()), out),
        }
    }

    fn walk_ended(
        &mut self,
        walk: Walk,
        result: Result<PeData, ResourceFailure>,
        out: &mut Vec<ResourceAction>,
    ) {
        out.push(event(ResourceEvent::Got {
            call: walk.call,
            target: walk.target,
            result,
        }));
    }

    /// Gets the data of a Subscription again.
    fn sync(
        &mut self,
        id: &SubscribeId,
        how: UpdateKind,
        now: Instant,
        out: &mut Vec<ResourceAction>,
    ) {
        let Some(sub) = self.subs.iter().find(|s| s.id == *id) else {
            return;
        };
        let encoding = self.preferred_encoding(sub.target.resource());
        let get = ResourceGet::new(sub.target.clone()).encoded(encoding);
        match self.pe.start(PeCall::Get(pe_get(&get)), now) {
            Ok((request, actions)) => {
                self.ops.push((
                    request,
                    Op::Sync {
                        subscribe_id: id.clone(),
                        how,
                    },
                ));
                let more = self.map(actions, now);
                out.extend(more);
            }
            Err(error) => out.push(event(ResourceEvent::SyncFailed {
                subscribe_id: id.clone(),
                failure: error.into(),
            })),
        }
    }

    /// ASCII where the entry takes it, and the first encoding it lists otherwise.
    fn preferred_encoding(&self, resource: &PeResource) -> PeEncoding {
        let listed = self
            .list
            .as_ref()
            .and_then(|l| l.entry(resource))
            .map(ResourceEntry::encodings);
        match listed {
            Some(encodings) if !encodings.contains(&PeEncoding::Ascii) => {
                encodings.first().copied().unwrap_or(PeEncoding::Ascii)
            }
            _ => PeEncoding::Ascii,
        }
    }

    fn notified(
        &mut self,
        notification: PeNotification,
        now: Instant,
        out: &mut Vec<ResourceAction>,
    ) {
        match notification {
            PeNotification::Ended {
                subscribe_id,
                resource: _,
            } => {
                if let Some(at) = self.subs.iter().position(|s| s.id == subscribe_id) {
                    let sub = self.subs.remove(at);
                    out.push(event(ResourceEvent::SubscriptionEnded {
                        subscribe_id,
                        target: sub.target,
                    }));
                }
            }
            PeNotification::Changed {
                subscribe_id,
                change,
                resource: _,
                media_type,
            } => {
                let Some(sub) = self.subs.iter_mut().find(|s| s.id == subscribe_id) else {
                    return;
                };
                let applied = match &change {
                    PeChange::Full(bytes) => {
                        read_full(sub.target.resource(), media_type.as_ref(), bytes)
                            .map(|d| (d, UpdateKind::Full))
                    }
                    PeChange::Partial(bytes) => sub
                        .data
                        .as_ref()
                        .and_then(|held| apply_partial(held, bytes))
                        .map(|d| (d, UpdateKind::Partial)),
                    PeChange::Notify | PeChange::Unreadable(_) => None,
                };
                match applied {
                    Some((data, how)) => {
                        sub.data = Some(data.clone());
                        out.push(event(ResourceEvent::Updated {
                            subscribe_id,
                            target: sub.target.clone(),
                            data,
                            how,
                        }));
                    }
                    None => self.sync(&subscribe_id, UpdateKind::Refreshed, now, out),
                }
            }
        }
    }
}

fn pe_get(get: &ResourceGet) -> PeGet {
    PeGet {
        resource: get.target().resource().name(),
        res_id: get.target().res_id().cloned(),
        page: get.page(),
        encoding: get.encoding(),
        flow_control: get.flow(),
    }
}

fn parse(response: &PeResponse) -> Result<PeJson, ResourceFailure> {
    Ok(PeJson::parse(&response.body)?)
}

/// The data of a reply: JSON, or the bytes of its media type.
fn decode(resource: &PeResource, response: &PeResponse) -> Result<PeData, ResourceFailure> {
    let data = match response.header.media_type.as_ref() {
        Some(media_type) => PeData::decode_bytes(resource, Some(media_type), &response.body)?,
        None => PeData::decode(resource, &parse(response)?)?,
    };
    Ok(data.read_header(&response.header.extra)?)
}

fn read_full(
    resource: &PeResource,
    media_type: Option<&MediaType>,
    bytes: &[u8],
) -> Option<PeData> {
    PeData::decode_bytes(resource, media_type, bytes).ok()
}

/// The data with the changes of a `partial` update made, when they can be.
fn apply_partial(held: &PeData, bytes: &[u8]) -> Option<PeData> {
    let update = PartialUpdate::from_json(&PeJson::parse(bytes).ok()?).ok()?;
    let changed = update.apply(&held.to_json()?).ok()?;
    PeData::decode(&held.resource(), &changed).ok()
}
