#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every `unsafe` block says why it is sound, and every `unsafe fn` what it
// asks of its caller.
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![warn(unsafe_op_in_unsafe_fn)]

mod com;
mod guard;
mod sdk;
mod types;

#[cfg(target_os = "linux")]
pub use com::RunLoop;
#[cfg(feature = "conformance")]
pub use com::RunLoopActivity;
pub use com::{
    BStream, ComponentHandler, EventList, HostApplication, HostPlugFrame, MIN_POINT_ROOM,
    NOTIFICATION_CAPACITY, PROGRESS_DESCRIPTION_CAPACITY, ParameterChangesImpl, ParameterEditEvent,
    PluginNotification, ProgressDescription, ProgressEvent, RestartFlags, UnitEvent,
    event_list_ptr, param_changes_ptr,
};
pub use sdk::{
    SdkEnum, c_str_to_string, cid_to_string, guid_as_tuid, sdk_enum_i32, utf16_to_string,
};
pub use types::*;

// Test-only global allocator for the allocation-free regression tests. Panics
// on any heap allocation inside `assert_no_alloc::assert_no_alloc(..)` scopes.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
