//! StateList and State (M2-111): the states of a device that an Initiator
//! saves and restores, and the calls and records around them.

use std::fmt;
use std::num::NonZeroU32;
use std::time::Instant;

use tutti_ump::DeviceIdentity;

use super::super::body::BodyLimit;
use super::super::chunk::ranged;
use super::super::header::PeEncoding;
use super::super::limits::{MediaType, NotInRange, NotValid, PropertyName, ResourceId, text_type};
use super::super::tree::{PeJson, PeObject};
use super::super::value::PeExtra;
use super::client::{CallError, CallId, ResourceInitiator};
use super::data::{OpaqueBody, PeData};
use super::entry::{EntryError, ResourceEntry};
use super::fields::{
    Fields, PeDataError, PeDataFault, PeExtensions, PeText, integer, required_text, text,
};
use super::kind::PeResource;
use super::request::{RequestRefused, ResourceGet, ResourceSet, ResourceTarget};
use super::store::{ResourceStore, StoreError};

ranged! {
    /// A time as Unix seconds: seconds since 1 January 1970 UTC, leap seconds not
    /// counted, as the `timestamp` of a state is (M2-111 §2.3, Table 4). The
    /// largest value is the largest JSON integer the crate reads.
    UnixSeconds(u64), "u64", 0, 0x7FFF_FFFF_FFFF_FFFF, "time in Unix seconds"
}

ranged! {
    /// The size of a state in bytes, the `size` of a StateList entry (M2-111 §2.3).
    StateSize(u64), "u64", 0, 0x7FFF_FFFF_FFFF_FFFF, "state size"
}

text_type! {
    /// The id of a state: 1 to 36 characters of letters, digits and `_`. The spec's
    /// text says `a-z`, `0-9` and `_` (M2-111 §2.3) and its own ids have capitals
    /// (`fullState`, Appendix A), so the letters are those of a Resource ID, which
    /// a State inquiry names it as (`resId`, M2-103 Table 13).
    StateId, "state id",
    |t| {
        (1..=36).contains(&t.len())
            && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }
}

text_type! {
    /// The device's own mark of the content of a state, the `stateRev` of a
    /// StateList entry and of a State reply (M2-111 §2.3, §3.1.1).
    StateRev, "state revision",
    |_| true
}

impl StateId {
    /// The `resId` an inquiry for the State names.
    #[must_use]
    pub fn resource_id(&self) -> ResourceId {
        // A state id is a subset of a Resource ID, which a test holds.
        ResourceId::new(self.as_str()).unwrap_or_else(|| unreachable!("a state id is a resId"))
    }
}

/// The states M2-111 Appendix A suggests for an instrument. A device is not
/// limited to them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommonState {
    /// `fullState`: everything, including every other state.
    Full,
    /// `allNonSystem`: everything but the system settings.
    AllNonSystem,
    /// `buffer`: the working settings, the edit buffer.
    Buffer,
    /// `system`: the system, global and utility settings.
    System,
    /// `programs`: every Program.
    Programs,
    /// `sequences`: the sequences, loops and MIDI files.
    Sequences,
    /// `arpeggios`: the arpeggiator algorithms.
    Arpeggios,
    /// `samples`: the audio samples.
    Samples,
    /// `effects`: every effect Program.
    Effects,
}

impl CommonState {
    /// The state id.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "fullState",
            Self::AllNonSystem => "allNonSystem",
            Self::Buffer => "buffer",
            Self::System => "system",
            Self::Programs => "programs",
            Self::Sequences => "sequences",
            Self::Arpeggios => "arpeggios",
            Self::Samples => "samples",
            Self::Effects => "effects",
        }
    }

    /// The state a state id names, or `None` for any other.
    #[must_use]
    pub fn new(id: &str) -> Option<Self> {
        [
            Self::Full,
            Self::AllNonSystem,
            Self::Buffer,
            Self::System,
            Self::Programs,
            Self::Sequences,
            Self::Arpeggios,
            Self::Samples,
            Self::Effects,
        ]
        .into_iter()
        .find(|s| s.as_str() == id)
    }
}

/// One state a device lists (M2-111 Table 3).
#[derive(Clone, Debug, PartialEq)]
pub struct PeStateEntry {
    /// The id the State Resource takes as `resId`.
    pub state_id: StateId,
    /// The name of the state.
    pub title: PeText,
    /// The device's mark of its content.
    pub rev: Option<StateRev>,
    /// When the device last changed it.
    pub timestamp: Option<UnixSeconds>,
    /// A description, in CommonMark (M2-103 §6.1.3).
    pub description: Option<String>,
    /// The size of the state in bytes.
    pub size: Option<StateSize>,
    /// The manufacturer-specific properties.
    pub extensions: PeExtensions,
}

impl PeStateEntry {
    /// An entry with an id and a title, and nothing else.
    #[must_use]
    pub fn new(state_id: StateId, title: PeText) -> Self {
        Self {
            state_id,
            title,
            rev: None,
            timestamp: None,
            description: None,
            size: None,
            extensions: PeExtensions::default(),
        }
    }

    fn to_json(&self) -> PeJson {
        let mut o = PeObject::new();
        o.insert("title", self.title.as_str().into());
        o.insert("stateId", self.state_id.as_str().into());
        if let Some(rev) = &self.rev {
            o.insert("stateRev", rev.as_str().into());
        }
        if let Some(t) = self.timestamp {
            o.insert("timestamp", integer_json(t.get()));
        }
        if let Some(d) = &self.description {
            o.insert("description", d.as_str().into());
        }
        if let Some(s) = self.size {
            o.insert("size", integer_json(s.get()));
        }
        self.extensions.write(&mut o);
        PeJson::Object(o)
    }

    fn from_json(json: &PeJson) -> Result<Self, PeDataError> {
        let mut f = Fields::new(json, "StateList entry")?;
        let title = required_text(&mut f, "title")?;
        let id = f.required("stateId", "a string", text)?.to_owned();
        let state_id = StateId::new(&id)
            .ok_or_else(|| f.fail("stateId", PeDataFault::NotValid("state id")))?;
        let rev = f.optional("stateRev", "a string", |j| text(j).and_then(StateRev::new))?;
        let timestamp = f.optional("timestamp", "an integer", unix_seconds)?;
        let description = f.optional("description", "a string", |j| text(j).map(str::to_owned))?;
        let size = f.optional("size", "an integer", |j| {
            u64::try_from(integer(j)?).ok().and_then(StateSize::new)
        })?;
        Ok(Self {
            state_id,
            title,
            rev,
            timestamp,
            description,
            size,
            extensions: f.finish()?,
        })
    }
}

fn integer_json(value: u64) -> PeJson {
    // Both ranged types stop at `i64::MAX`.
    PeJson::Integer(i64::try_from(value).unwrap_or(i64::MAX))
}

fn unix_seconds(json: &PeJson) -> Option<UnixSeconds> {
    UnixSeconds::new(u64::try_from(integer(json)?).ok()?)
}

super::channels::list_type! {
    /// The Property Data of StateList: the states a device has (M2-111 §2.3).
    PeStateList of PeStateEntry
}

impl PeStateList {
    pub(super) fn to_json(&self) -> PeJson {
        PeJson::Array(self.0.iter().map(PeStateEntry::to_json).collect())
    }

    pub(super) fn from_items(items: &[PeJson]) -> Result<Self, PeDataError> {
        items
            .iter()
            .map(PeStateEntry::from_json)
            .collect::<Result<_, _>>()
            .map(Self)
    }

    /// The entry of the state `id`.
    #[must_use]
    pub fn entry(&self, id: &StateId) -> Option<&PeStateEntry> {
        self.0.iter().find(|e| e.state_id == *id)
    }
}

/// The Property Data of State: what a device's state is, as the device gives it
/// (M2-111 §3), with the revision and time its reply carries in its header.
///
/// The body is the device's own: bytes with the media type that names them, or
/// JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct PeState {
    body: OpaqueBody,
    rev: Option<StateRev>,
    timestamp: Option<UnixSeconds>,
}

impl PeState {
    /// A state of `bytes` that are no format a media type names:
    /// `application/octet-stream`.
    #[must_use]
    pub fn octets(bytes: Vec<u8>) -> Self {
        let media_type = MediaType::new("application/octet-stream")
            .unwrap_or_else(|| unreachable!("a valid media type"));
        Self {
            body: OpaqueBody::Binary { media_type, bytes },
            rev: None,
            timestamp: None,
        }
    }

    /// A state of `bytes` of `media_type`, or `None` for `application/json`,
    /// which [`json`](Self::json) holds.
    #[must_use]
    pub fn binary(media_type: MediaType, bytes: Vec<u8>) -> Option<Self> {
        (!media_type.as_str().eq_ignore_ascii_case("application/json")).then_some(Self {
            body: OpaqueBody::Binary { media_type, bytes },
            rev: None,
            timestamp: None,
        })
    }

    /// A state that is JSON.
    #[must_use]
    pub fn json(json: PeJson) -> Self {
        Self {
            body: OpaqueBody::Json(json),
            rev: None,
            timestamp: None,
        }
    }

    /// The state with its revision.
    #[must_use]
    pub fn with_rev(mut self, rev: StateRev) -> Self {
        self.rev = Some(rev);
        self
    }

    /// The state with the time it was last changed.
    #[must_use]
    pub fn with_timestamp(mut self, timestamp: UnixSeconds) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    /// The body.
    #[must_use]
    pub fn body(&self) -> &OpaqueBody {
        &self.body
    }

    /// The revision the device gave.
    #[must_use]
    pub fn rev(&self) -> Option<&StateRev> {
        self.rev.as_ref()
    }

    /// The time the device gave.
    #[must_use]
    pub fn timestamp(&self) -> Option<UnixSeconds> {
        self.timestamp
    }

    /// The body as it is sent before its encoding: the media type, none for JSON,
    /// and the bytes.
    #[must_use]
    pub fn to_wire(&self) -> (Option<MediaType>, Vec<u8>) {
        match &self.body {
            OpaqueBody::Json(json) => (None, json.encode()),
            OpaqueBody::Binary { media_type, bytes } => (Some(media_type.clone()), bytes.clone()),
        }
    }

    /// The state in `bytes` of `media_type`, none for JSON.
    ///
    /// # Errors
    ///
    /// [`PeDataError`] for JSON text that is not JSON.
    pub fn from_wire(media_type: Option<&MediaType>, bytes: &[u8]) -> Result<Self, PeDataError> {
        match media_type {
            Some(media_type) => Self::binary(media_type.clone(), bytes.to_vec())
                .map_or_else(|| Self::parse_json(bytes), Ok),
            None => Self::parse_json(bytes),
        }
    }

    fn parse_json(bytes: &[u8]) -> Result<Self, PeDataError> {
        PeJson::parse(bytes)
            .map(Self::json)
            .map_err(|_| PeDataError::new("State", PeDataFault::NotValid("JSON text")))
    }

    /// Writes the revision and time into the header of a reply.
    pub(super) fn write_header(&self, extra: &mut PeExtra) {
        StateStamp {
            rev: self.rev.clone(),
            timestamp: self.timestamp,
        }
        .write(extra);
    }

    /// The state with the revision and time a reply's header carries.
    ///
    /// # Errors
    ///
    /// [`PeDataError`] for a `stateRev` that is no string and a `timestamp` that is
    /// no time.
    pub(super) fn read_header(mut self, extra: &PeExtra) -> Result<Self, PeDataError> {
        let stamp = StateStamp::read(extra)?;
        self.rev = stamp.rev.or(self.rev);
        self.timestamp = stamp.timestamp.or(self.timestamp);
        Ok(self)
    }

    pub(super) fn stamped_with(mut self, stamp: StateStamp) -> Self {
        self.rev = stamp.rev;
        self.timestamp = stamp.timestamp;
        self
    }
}

/// The revision and time a device gives a state: the `stateRev` and `timestamp` of
/// the header of a reply to Get State and to Set State (M2-111 §3.2, §3.3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateStamp {
    /// The device's mark of the state's content.
    pub rev: Option<StateRev>,
    /// When the device last changed the state.
    pub timestamp: Option<UnixSeconds>,
}

impl StateStamp {
    pub(super) fn write(&self, extra: &mut PeExtra) {
        // Both names are no field of the header types and both values scalars, so an
        // insert cannot be refused.
        if let (Some(rev), Some(name)) = (&self.rev, PropertyName::new("stateRev")) {
            let _ = extra.insert(name, rev.as_str().into());
        }
        if let (Some(t), Some(name)) = (self.timestamp, PropertyName::new("timestamp")) {
            let _ = extra.insert(name, integer_json(t.get()));
        }
    }

    /// The stamp a reply's header carries.
    ///
    /// # Errors
    ///
    /// [`PeDataError`] for a `stateRev` that is no string and a `timestamp` that is
    /// no time.
    pub(super) fn read(extra: &PeExtra) -> Result<Self, PeDataError> {
        let rev = match extra.get("stateRev") {
            Some(json) => Some(text(json).and_then(StateRev::new).ok_or_else(|| {
                PeDataError::new("State.stateRev", PeDataFault::WrongType("a string"))
            })?),
            None => None,
        };
        let timestamp = match extra.get("timestamp") {
            Some(json) => Some(unix_seconds(json).ok_or_else(|| {
                PeDataError::new(
                    "State.timestamp",
                    PeDataFault::NotValid("time in Unix seconds"),
                )
            })?),
            None => None,
        };
        Ok(Self { rev, timestamp })
    }
}

/// How much of a state's body a [`StateDevice`] is judging.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Arrival<'a> {
    /// The start of the body, decoded, while chunks are still coming. It is the
    /// whole of what the device can read yet: an encoding that cannot be read in
    /// part (zlib) gives no prefix, and the device judges the whole body.
    Prefix(&'a [u8]),
    /// The whole body, decoded.
    Whole(&'a PeState),
}

/// What a [`StateDevice`] makes of a state's body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// Not enough to tell yet.
    Undecided,
    /// The body is the device's.
    Sound,
    /// The body is not correct. Mid-transfer the Responder sends Terminate Inquiry
    /// and drops the rest; at the end it answers 400.
    Refuse,
}

/// The device's say over the states its Responder is sent (M2-111 §3.3): whether a
/// body is its own, and what revision and time an accepted one gets.
pub trait StateDevice: std::fmt::Debug {
    /// Judges the body of the state `id` that an Initiator is setting. It is called
    /// after each chunk with the prefix that has come, until it answers
    /// [`Sound`](Verdict::Sound) or [`Refuse`](Verdict::Refuse), and once more
    /// with the whole body. [`Undecided`](Verdict::Undecided) at the end accepts.
    fn judge(&mut self, id: &StateId, arrival: Arrival<'_>) -> Verdict;

    /// The revision and time of the state `id` just accepted, which the reply
    /// carries and the StateList then lists.
    fn stamp(&mut self, id: &StateId, state: &PeState) -> StateStamp;
}

/// Where a state was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefusedAt {
    /// Before the body was all in: the Responder ended the transfer.
    Midway,
    /// With the body all in: the Responder answered 400.
    End,
}

impl ResourceStore {
    /// Lists the state `id` as `state` is: its revision, time and size, with the
    /// title, description and extensions of an entry already there. A state the
    /// list lacks is listed with its id as the title.
    pub(super) fn note_state(&mut self, id: &StateId, state: &PeState) -> Result<(), StoreError> {
        let target = ResourceTarget::new(PeResource::StateList)
            .unwrap_or_else(|_| unreachable!("StateList has no resId"));
        let mut entries = match self.get(&target) {
            Some(PeData::StateList(list)) => list.clone().into_entries(),
            _ => Vec::new(),
        };
        let (_, bytes) = state.to_wire();
        let size = StateSize::new(bytes.len() as u64);
        match entries.iter_mut().find(|e| e.state_id == *id) {
            Some(entry) => {
                entry.rev = state.rev.clone();
                entry.timestamp = state.timestamp;
                entry.size = size;
            }
            None => {
                let title = PeText::new(id.as_str())
                    .unwrap_or_else(|| unreachable!("a state id is not empty"));
                let mut entry = PeStateEntry::new(id.clone(), title);
                entry.rev = state.rev.clone();
                entry.timestamp = state.timestamp;
                entry.size = size;
                entries.push(entry);
            }
        }
        self.put(None, PeData::StateList(PeStateList::new(entries)))
    }
}

impl ResourceEntry {
    /// The entry of State with the defaults M2-111 §3.4 gives: Get and a Full Set,
    /// one state to each `resId`, Mcoded7 and `application/octet-stream`.
    #[must_use]
    pub fn device_state() -> Self {
        let octets = MediaType::new("application/octet-stream")
            .unwrap_or_else(|| unreachable!("a valid media type"));
        Self::new(PeResource::State)
            .and_then(|e| e.with_encodings(vec![PeEncoding::Mcoded7]))
            .and_then(|e| e.with_media_types(vec![octets]))
            .unwrap_or_else(|_: EntryError| unreachable!("State takes its own defaults"))
    }

    /// The entry of StateList.
    #[must_use]
    pub fn state_list() -> Self {
        Self::new(PeResource::StateList)
            .unwrap_or_else(|_: EntryError| unreachable!("StateList has no custom name"))
    }
}

impl ResourceStore {
    /// Declares StateList and State, so that the store serves states.
    pub fn declare_states(&mut self) {
        self.declare(ResourceEntry::state_list());
        self.declare(ResourceEntry::device_state());
        // An empty list answers a Get before any state is put.
        let _ = self.put(None, PeData::StateList(PeStateList::default()));
    }

    /// Puts a state and lists it: the State under `entry.state_id` and `entry` in
    /// StateList, in place of an entry of the same id. The entry's `size` is the
    /// state's when `entry` gives none, and its revision and time are the
    /// state's.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotDeclared`] when [`declare_states`](Self::declare_states)
    /// has not been called.
    pub fn put_state(&mut self, mut entry: PeStateEntry, state: PeState) -> Result<(), StoreError> {
        let list_target = ResourceTarget::new(PeResource::StateList)
            .unwrap_or_else(|_| unreachable!("StateList has no resId"));
        let mut entries = match self.get(&list_target) {
            Some(PeData::StateList(list)) => list.clone().into_entries(),
            _ => Vec::new(),
        };
        entry.rev = entry.rev.or_else(|| state.rev.clone());
        entry.timestamp = entry.timestamp.or(state.timestamp);
        if entry.size.is_none() {
            let (_, bytes) = state.to_wire();
            entry.size = StateSize::new(bytes.len() as u64);
        }
        let id = entry.state_id.resource_id();
        match entries.iter_mut().find(|e| e.state_id == entry.state_id) {
            Some(old) => *old = entry,
            None => entries.push(entry),
        }
        self.put(Some(id), PeData::State(state))?;
        self.put(None, PeData::StateList(PeStateList::new(entries)))
    }
}

/// A state that is not sent where it was not taken from.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the state was saved from another device than the one it is restored to")]
pub struct WrongDevice {
    /// The identity the state was saved with.
    pub saved: DeviceIdentity,
    /// The identity of the device it was to be restored to.
    pub peer: DeviceIdentity,
}

/// A fetch of a state that cannot be made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GetStateError {
    /// The StateList says the state is bigger than the caller takes.
    #[error("the state is {} bytes, past the limit of {}", .size.get(), .limit.get())]
    TooLarge {
        /// The size the entry gives.
        size: StateSize,
        /// The caller's limit.
        limit: BodyLimit,
    },
    /// The Get cannot be made.
    #[error(transparent)]
    Call(#[from] CallError),
}

/// A restore that cannot be made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RestoreError {
    /// The state belongs to another device.
    #[error(transparent)]
    WrongDevice(#[from] WrongDevice),
    /// The Set cannot be made.
    #[error(transparent)]
    Call(#[from] CallError),
}

/// A state saved from a device, to restore to it later: the device's identity, the
/// state's id, and its Property Data with the revision and time the device gave
/// (M2-111 §3.1.1).
///
/// A state is useful only to the device it came from, or one of the same model
/// and version, so [`restore`](Self::restore) refuses a device whose identity
/// differs. With the `serde` feature it is a value an app writes in the format it
/// chooses.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "wire::Wire", into = "wire::Wire"))]
pub struct SavedState {
    device: DeviceIdentity,
    state_id: StateId,
    state: PeState,
}

impl SavedState {
    /// The state `state_id` as `device` gave it.
    #[must_use]
    pub fn new(device: DeviceIdentity, state_id: StateId, state: PeState) -> Self {
        Self {
            device,
            state_id,
            state,
        }
    }

    /// The saved state with `now` as its time when the device gave none, which the
    /// spec leaves the Initiator to tag (M2-111 §3.1.1).
    #[must_use]
    pub fn stamped(mut self, now: UnixSeconds) -> Self {
        self.state.timestamp.get_or_insert(now);
        self
    }

    /// The device it was saved from.
    #[must_use]
    pub fn device(&self) -> &DeviceIdentity {
        &self.device
    }

    /// The id of the state.
    #[must_use]
    pub fn state_id(&self) -> &StateId {
        &self.state_id
    }

    /// The state.
    #[must_use]
    pub fn state(&self) -> &PeState {
        &self.state
    }

    /// The Set that restores the state to `peer`.
    ///
    /// # Errors
    ///
    /// [`WrongDevice`] unless `peer` is the identity the state was saved with.
    pub fn restore(&self, peer: &DeviceIdentity) -> Result<ResourceSet, RestoreError> {
        if self.device != *peer {
            return Err(WrongDevice {
                saved: self.device,
                peer: *peer,
            }
            .into());
        }
        let target = ResourceTarget::entry(PeResource::State, self.state_id.resource_id())?;
        Ok(ResourceSet::full(
            target,
            PeData::State(self.state.clone()),
        )?)
    }
}

#[cfg(feature = "serde")]
mod wire {
    //! The document of a saved state: plain fields that read back through the
    //! checks of their types.

    use super::{PeState, SavedState, StateId, StateRev, UnixSeconds};
    use crate::ci::pe::limits::MediaType;
    use tutti_ump::DeviceIdentity;

    #[derive(serde::Serialize, serde::Deserialize)]
    pub(super) struct Wire {
        pub(super) device: DeviceIdentity,
        pub(super) state_id: String,
        pub(super) rev: Option<String>,
        pub(super) timestamp: Option<UnixSeconds>,
        pub(super) media_type: Option<String>,
        pub(super) bytes: Vec<u8>,
    }

    impl From<SavedState> for Wire {
        fn from(saved: SavedState) -> Self {
            let (media_type, bytes) = saved.state.to_wire();
            Self {
                device: saved.device,
                state_id: saved.state_id.as_str().to_owned(),
                rev: saved.state.rev.map(|r| r.as_str().to_owned()),
                timestamp: saved.state.timestamp,
                media_type: media_type.map(|m| m.as_str().to_owned()),
                bytes,
            }
        }
    }

    impl TryFrom<Wire> for SavedState {
        type Error = String;

        fn try_from(wire: Wire) -> Result<Self, String> {
            let state_id = StateId::new(&wire.state_id)
                .ok_or_else(|| format!("{:?} is not a state id", wire.state_id))?;
            let media_type = wire
                .media_type
                .as_deref()
                .map(|m| MediaType::new(m).ok_or_else(|| format!("{m:?} is not a media type")))
                .transpose()?;
            let mut state =
                PeState::from_wire(media_type.as_ref(), &wire.bytes).map_err(|e| e.to_string())?;
            state.rev = wire
                .rev
                .as_deref()
                .map(|r| StateRev::new(r).ok_or_else(|| format!("{r:?} is not a revision")))
                .transpose()?;
            state.timestamp = wire.timestamp;
            Ok(Self::new(wire.device, state_id, state))
        }
    }
}

impl ResourceInitiator {
    /// The `encoding` a call for `resource` is made in: ASCII where the entry
    /// takes it, and the first it lists otherwise.
    fn state_encoding(&self, resource: &PeResource) -> PeEncoding {
        match self.resource_list().and_then(|l| l.entry(resource)) {
            Some(entry) if !entry.encodings().contains(&PeEncoding::Ascii) => entry
                .encodings()
                .first()
                .copied()
                .unwrap_or(PeEncoding::Ascii),
            _ => PeEncoding::Ascii,
        }
    }

    fn state_get(&self, target: ResourceTarget) -> ResourceGet {
        let encoding = self.state_encoding(target.resource());
        let get = ResourceGet::new(target).encoded(encoding);
        let flows = self
            .resource_list()
            .and_then(|l| l.entry(get.target().resource()))
            .is_some_and(ResourceEntry::fc_on_get);
        if flows { get.with_flow_control() } else { get }
    }

    /// Gets the list of the device's states. The result is a
    /// [`ResourceEvent::Got`](super::ResourceEvent::Got) with a
    /// [`PeData::StateList`].
    ///
    /// # Errors
    ///
    /// As for [`get_list`](Self::get_list).
    pub fn list_states(
        &mut self,
        page_size: NonZeroU32,
        now: Instant,
    ) -> Result<(CallId, Vec<super::ResourceAction>), CallError> {
        let target = ResourceTarget::new(PeResource::StateList)?;
        let get = self.state_get(target);
        self.get_list(&get, page_size, now)
    }

    /// Gets the state `entry` lists, under Flow Control when the entry of State
    /// declares it, as long as it stays within `limit`. The result is a
    /// [`ResourceEvent::Got`](super::ResourceEvent::Got) with a
    /// [`PeData::State`], or a [`PeOutcome::TooLarge`](super::super::PeOutcome::TooLarge)
    /// failure when the body grew past `limit` and the Responder was told to stop
    /// (M2-111 §3.2).
    ///
    /// # Errors
    ///
    /// [`GetStateError::TooLarge`] before anything is sent when `entry` gives a size
    /// past `limit`, and [`GetStateError::Call`] as for [`get`](Self::get).
    pub fn get_state(
        &mut self,
        entry: &PeStateEntry,
        limit: BodyLimit,
        now: Instant,
    ) -> Result<(CallId, Vec<super::ResourceAction>), GetStateError> {
        if let Some(size) = entry.size
            && size.get() > limit.get() as u64
        {
            return Err(GetStateError::TooLarge { size, limit });
        }
        let target = ResourceTarget::entry(PeResource::State, entry.state_id.resource_id())
            .map_err(CallError::from)?;
        let get = self.state_get(target).limited(limit);
        Ok(self.get(&get, now)?)
    }

    /// Restores a saved state to the device `peer`, the identity of which the
    /// caller has from Discovery or DeviceInfo.
    ///
    /// # Errors
    ///
    /// [`RestoreError::WrongDevice`] before anything is sent when `peer` is not the
    /// device the state came from, and [`RestoreError::Call`] as for
    /// [`set`](Self::set).
    pub fn restore_state(
        &mut self,
        saved: &SavedState,
        peer: &DeviceIdentity,
        now: Instant,
    ) -> Result<(CallId, Vec<super::ResourceAction>), RestoreError> {
        let set = saved
            .restore(peer)?
            .encoded(self.state_encoding(&PeResource::State));
        Ok(self.set(&set, now)?)
    }
}

impl From<RequestRefused> for RestoreError {
    fn from(refused: RequestRefused) -> Self {
        Self::Call(refused.into())
    }
}

#[cfg(test)]
mod tests;
