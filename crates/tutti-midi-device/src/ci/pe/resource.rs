//! The standard Resources of Property Exchange, typed (M2-103 §8 to §14, M2-105
//! to M2-109, M2-111, M2-112, M2-117).

mod channels;
mod client;
mod controllers;
mod data;
mod entry;
mod fields;
mod info;
mod kind;
mod link;
mod programs;
mod request;
mod server;
mod simple;
mod state;
mod store;

pub use channels::{PeChannelEntry, PeChannelList, PeCluster};
pub use client::{
    CallError, CallId, MAX_LIST_ENTRIES, PaginationFault, ResourceAction, ResourceEvent,
    ResourceFailure, ResourceInitiator, UpdateKind,
};
pub use controllers::{
    CcIndex, CtrlControl, CtrlRange, DefaultMapNotCc, ListedTwice, PeAllCtrlEntry, PeAllCtrlList,
    PeChCtrlEntry, PeChCtrlList, PeController, PeCtrl, PeCtrlMapEntry, PeCtrlMapList,
};
pub use data::{OpaqueBody, PeData, PeOpaque};
pub use entry::{EntryError, PeResourceList, ResourceEntry};
pub use fields::{
    ClusterKind, CtrlKind, CtrlValueKind, ModeId, NotVendorName, ParamPath, PeBankProgram,
    PeChannel, PeDataError, PeDataFault, PeExtensions, PeMidiMode, PeText, Priority, RoleName,
    SignificantBits, StepCount, TypeHint,
};
pub use info::{PeDeviceInfo, PeJsonSchema};
pub use kind::{CanSet, CustomResource, PeResource, ResIdUse, ResourceDefaults};
pub use link::{
    ColumnSource, ConfirmIcon, ConfirmType, Confirmation, LinkRole, PeColumn, PeLink,
    SelectorAction, SimpleAction,
};
pub use programs::{PeModeEntry, PeModeList, PeProgramEntry, PeProgramList};
pub use request::{ActionError, RequestRefused, ResourceGet, ResourceSet, ResourceTarget, SetBody};
pub use server::{
    ChangeKind, PublishError, ResourceResponder, ResponderAction, ResponderEvent, ResponderSettings,
};
pub use simple::{
    PeBasicChannelRx, PeBasicChannelTx, PeChannelMode, PeCurrentMode, PeExternalSync, PeLocalOn,
};
pub use state::{
    CommonState, PeState, PeStateEntry, PeStateList, RestoreError, SavedState, StateId, StateRev,
    StateSize, UnixSeconds, WrongDevice,
};
pub use store::{ResourceStore, StoreError};

#[cfg(test)]
mod behaviour;
#[cfg(test)]
mod exchange;
#[cfg(test)]
mod tests;
