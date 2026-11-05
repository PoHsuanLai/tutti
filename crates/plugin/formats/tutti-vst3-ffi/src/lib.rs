#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every `unsafe` block says why it is sound, and every `unsafe fn` what it
// asks of its caller.
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![warn(unsafe_op_in_unsafe_fn)]

mod audio;
mod bus_buffers;
mod com;
#[cfg(feature = "conformance")]
mod conformance;
mod error;
mod guard;
mod module;
mod module_entry;
mod plugin;
mod sdk;
mod types;
mod view;

pub use audio::{AudioSide, ProcessIo};
pub use bus_buffers::{BoundBuses, BusBuffers};
#[cfg(feature = "conformance")]
pub use com::RunLoopActivity;
pub use com::{
    ComponentHandler, EventList, MIN_POINT_ROOM, NOTIFICATION_CAPACITY,
    PROGRESS_DESCRIPTION_CAPACITY, ParameterChangesImpl, ParameterEditEvent, PluginNotification,
    ProgressDescription, ProgressEvent, RestartFlags, UnitEvent, event_list_ptr, param_changes_ptr,
};
#[cfg(feature = "conformance")]
pub use conformance::{Observer, clear_observer, set_observer};
pub use error::Vst3Error;
pub use module::{ClassInfo, CreateError, FactoryInfo, LoadError, Module, factory_flags};
pub use plugin::{BusDirection, Component, MediaType, MidiLearn, Plugin};
pub use sdk::{
    SdkEnum, Zeroable, c_str_to_string, cid_to_string, guid_as_tuid, sdk_enum_i32, utf16_to_string,
    zeroed,
};
pub use types::*;
pub use view::{
    EditorError, detach_view, platform_type_refused, send_key_down, send_key_up, send_wheel,
    set_view_focus,
};

// Test-only global allocator for the allocation-free regression tests. Panics
// on any heap allocation inside `assert_no_alloc::assert_no_alloc(..)` scopes.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
