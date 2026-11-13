#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Every `unsafe` block says why it is sound, and every `unsafe fn` what it
// asks of its caller.
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![warn(unsafe_op_in_unsafe_fn)]

mod audio;
mod events;
mod guard;
mod gui;
mod host;
mod library;
mod plugin;
mod pool;
mod roles;
mod stream;
mod types;

pub use audio::{AudioSide, Planar, PortScratch, ProcessCall, ProcessError};
pub use events::{EventRef, EventSink, EventSource, PluginEvent, header_size};
#[cfg(feature = "testing")]
#[doc(hidden)]
pub use events::{input_raw_for_test, output_raw_for_test};
pub use gui::{EmbedOutcome, GuiError, GuiHints};
pub use host::{Host, HostCallbacks};
pub use library::{Bundle, DescriptorInfo, LoadError, Stage, chars_to_string};
pub use plugin::PortConfigRequest;
pub use plugin::{
    ActivateError, AudioPortInfo, CallError, Extension, Plugin, PortKind, off_thread_drops,
};
pub use pool::{GraphPool, PoolHandle, TaskPool};
pub use roles::{AudioClaim, LifecycleClaim, ThreadRoles};
pub use stream::{InputStream, OutputStream};
pub use types::{
    AmbisonicDecl, AudioPortFlags, AudioPortsConfig, AudioPortsRescanFlags, AutomationState,
    LogSeverity, NoteName, NotePortDecl, ParamInfo, ParamsRescanFlags, ProcessStatus, StateContext,
    VoiceInfo,
};

// Test-only global allocator for the no-allocation gates.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
