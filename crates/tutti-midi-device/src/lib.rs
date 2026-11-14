#![doc = include_str!("../README.md")]
//! The pure core of MIDI device detection and the MIDI-CI codec.
//!
//! The CI codec ([`CiMessage`], [`Muid`], [`DiscoveryData`])
//! turns Discovery, Invalidate MUID, Inquiry: Endpoint, ACK, NAK, the Profile Configuration messages
//! and the Process Inquiry messages
//! into SysEx payload bytes and back;
//! it holds no state and no transport. [`CiDevice`] is the Management state
//! machine over it (Discovery, MUID collisions, Invalidate MUID, NAK, Endpoint
//! Inquiry), shaped as [`Connection`] is, and [`ProfileInitiator`] the Initiator of Profile
//! Configuration with one peer, and [`ProcessInitiator`] the Initiator of Process Inquiry.
//!
//! A backend reports what the OS says as an [`OsChange`]; [`DeviceTable::apply`]
//! returns the [`DeviceEvent`] that change causes, or `None` when it alters
//! nothing. The table holds no OS handle, thread or clock, so every transition
//! is a plain test.
//!
//! ```
//! use tutti_midi_device::{
//!     DeviceEvent, DeviceTable, PortDirection, EndpointId, Identity, OsChange, OsDevice,
//! };
//!
//! let mut table = DeviceTable::default();
//! let keys = OsDevice {
//!     handle: EndpointId::from_raw(1),
//!     identity: Identity::alsa("Keystep", "Keystep MIDI 1"),
//!     name: "Keystep: Keystep MIDI 1".into(),
//!     direction: PortDirection::Input,
//! };
//! let Some(DeviceEvent::Added(info)) = table.apply(OsChange::Present(keys.clone())) else {
//!     unreachable!()
//! };
//! assert!(table.list().eq([&info]));
//! // The same report again changes nothing.
//! assert_eq!(table.apply(OsChange::Present(keys)), None);
//! assert!(matches!(
//!     table.apply(OsChange::Gone(EndpointId::from_raw(1))),
//!     Some(DeviceEvent::Removed(_))
//! ));
//! ```

//! The UMP [`Endpoint`] and [`Connection`] are the other half: an endpoint's declared
//! state with its answers ([`Endpoint::reply`]), and the discoverer's steps over
//! received messages and time as a value (M2-104 §7.1, M2-100 Table 4).
//!
//! ```
//! use std::time::{Duration, Instant};
//! use tutti_midi_device::{Action, Connection, Ending, STEP_TIMEOUT};
//!
//! let start = Instant::now();
//! let (mut connection, first) = Connection::new(start);
//! // `first` is `Action::Send(discovery)`: the driver loop handles it like any
//! // other action. A peer that never answers is sent the discovery again after
//! // each `STEP_TIMEOUT`, and is a MIDI 1.0 device after the third.
//! assert_eq!(connection.on_timeout(start + STEP_TIMEOUT), first);
//! connection.on_timeout(start + 2 * STEP_TIMEOUT);
//! assert_eq!(
//!     connection.on_timeout(start + 3 * STEP_TIMEOUT + Duration::from_millis(1)),
//!     Action::Settled(Ending::Midi1Fallback)
//! );
//! ```
//!
#![forbid(unsafe_code)]

mod ci;
mod connection;
mod endpoint;
mod id;
mod inquiry;
mod jr;
mod learned;
mod name;
mod table;

pub use ci::{
    Ack, ActionError, BodyLimit, BufferLimit, CI_SUB_ID_1, CI_UNIVERSAL_SYSEX, CI_VERSION,
    CallError, CallId, CanSet, CcIndex, ChangeKind, ChannelControllers, ChannelsDetail, ChunkNo,
    ChunkPosition, ChunkTotal, CiAction, CiCategories, CiDevice, CiDeviceId, CiHeader,
    CiInstanceId, CiMessage, CiPeer, CiVersion, ClusterKind, ColumnSource, ConfirmIcon,
    ConfirmType, Confirmation, CtrlControl, CtrlKind, CtrlRange, CtrlValueKind, CustomResource,
    DISCOVERY_WINDOW, DataControl, Decoded, DefaultMapNotCc, Destination, DetailsData,
    DiscoveryData, EndpointInfo, EndpointStatus, EntryError, InquiryTarget, JsonPointer, LinkRole,
    ListTooLong, ListedTwice, MAX_LIST_ENTRIES, ManufacturerProfile, MaxSysexSize, MediaType,
    MessageText, ModeId, Muid, Nak, NotADataControl, NotADeviceId, NotAMuid, NotASysexSize,
    NotAVersion, NotInRange, NotValid, NotVendorName, NoteData, OpaqueBody, Outcome,
    PE_JSON_MAX_DEPTH, PE_MAX_CHUNK_DATA_LEN, PE_MAX_HEADER_LEN, PE_TIMEOUT, PROCESS_TIMEOUT,
    PROFILE_TIMEOUT, PaginationFault, ParamPath, PartialError, PartialUpdate, PeAction,
    PeAllCtrlEntry, PeAllCtrlList, PeAnswer, PeBankProgram, PeBasicChannelRx, PeBasicChannelTx,
    PeBodyError, PeBuffered, PeCall, PeCapabilities, PeCapabilitiesFailure, PeChCtrlEntry,
    PeChCtrlList, PeChange, PeChannel, PeChannelEntry, PeChannelList, PeChannelMode, PeChunk,
    PeChunkError, PeCiMessage, PeCluster, PeColumn, PeController, PeCtrl, PeCtrlMapEntry,
    PeCtrlMapList, PeCurrentMode, PeData, PeDataError, PeDataFault, PeDecimal, PeDeviceInfo,
    PeEncoding, PeEndedBy, PeError, PeExpired, PeExtensions, PeExternalSync, PeExtra, PeExtraError,
    PeFlowControl, PeGet, PeHeader, PeHeaderError, PeHeaderTooLong, PeInitiator, PeJson,
    PeJsonError, PeJsonSchema, PeKind, PeLink, PeLocalOn, PeMessage, PeMessageKind, PeMidiMode,
    PeModeEntry, PeModeList, PeNotification, PeObject, PeOpaque, PeOutcome, PePage, PeProgramEntry,
    PeProgramList, PeReassembler, PeReassemblyError, PeReassemblyLimits, PeReceived, PeRefused,
    PeReply, PeRequest, PeRequestId, PeResource, PeResourceList, PeResponse, PeSet, PeSetScope,
    PeSettings, PeSignal, PeStatus, PeStatusClass, PeStep, PeSubscribed, PeSubscription,
    PeTerminator, PeText, PeTransaction, PeUnreadable, PeUpdateCommand, PeVersion, Priority,
    ProcessAction, ProcessError, ProcessFeatures, ProcessInitiator, ProfileAction, ProfileChange,
    ProfileChannels, ProfileDataError, ProfileEntry, ProfileId, ProfileInitiator, ProfileLists,
    ProfileState, ProfileTable, ProfileTarget, PropertyName, PublishError, ReportRequest,
    ReportScope, Request, RequestError, RequestRefused, ResIdUse, ResourceAction, ResourceDefaults,
    ResourceEntry, ResourceEvent, ResourceFailure, ResourceGet, ResourceId, ResourceInitiator,
    ResourceName, ResourceResponder, ResourceSet, ResourceStore, ResourceTarget, ResponderAction,
    ResponderEvent, ResponderSettings, RoleName, SUB_ID2_ACK, SUB_ID2_DISCOVERY,
    SUB_ID2_DISCOVERY_REPLY, SUB_ID2_ENDPOINT_INQUIRY, SUB_ID2_ENDPOINT_REPLY,
    SUB_ID2_INVALIDATE_MUID, SUB_ID2_MESSAGE_REPORT_END, SUB_ID2_MESSAGE_REPORT_INQUIRY,
    SUB_ID2_MESSAGE_REPORT_REPLY, SUB_ID2_NAK, SUB_ID2_PE_CAPABILITIES_INQUIRY,
    SUB_ID2_PE_CAPABILITIES_REPLY, SUB_ID2_PE_NOTIFY, SUB_ID2_PROCESS_CAPABILITIES_INQUIRY,
    SUB_ID2_PROCESS_CAPABILITIES_REPLY, SUB_ID2_PROFILE_ADDED, SUB_ID2_PROFILE_DETAILS_INQUIRY,
    SUB_ID2_PROFILE_DETAILS_REPLY, SUB_ID2_PROFILE_DISABLED, SUB_ID2_PROFILE_ENABLED,
    SUB_ID2_PROFILE_INQUIRY, SUB_ID2_PROFILE_REMOVED, SUB_ID2_PROFILE_REPLY,
    SUB_ID2_PROFILE_SPECIFIC_DATA, SUB_ID2_SET_PROFILE_OFF, SUB_ID2_SET_PROFILE_ON, SelectorAction,
    SetBody, SignificantBits, SimpleAction, SimultaneousRequests, SpecificData, StandardProfile,
    StepCount, StoreError, StreamLimit, SubscribeId, SystemMessages, TimeoutWait, TypeHint, Unread,
    UnreadReason, UpdateKind, decode_body, encode_body, mcoded7, split_message,
};

pub use connection::{Action, Connection, Ending, MAX_ATTEMPTS, NAME_GRACE, STEP_TIMEOUT};
pub use endpoint::{
    BlockError, Capabilities, Endpoint, FunctionBlock, FunctionBlockBuilder, MAX_FUNCTION_BLOCKS,
    UMP_VERSION,
};
pub use id::{DeviceId, DeviceInfo, EndpointId, Identity, PortDirection};
pub use inquiry::{INQUIRY_ATTEMPTS, INQUIRY_TIMEOUT, IdentityInquiry, InquiryAction};
pub use jr::{
    ClockHeard, DRIFT_SPAN, JrLock, JrReceiver, JrTime, LOST_AFTER, TICK_HZ, Unmapped, WINDOW,
};
pub use learned::{Learned, Match, SavedDevice};
pub use name::{BlockName, EndpointName, Name, NameError, ProductInstanceId};
pub use table::{BindPolicy, DeviceEvent, DeviceTable, OsChange, OsDevice, Resolved};
