//! The COM objects the host implements for a plugin: each is a Rust type
//! behind a `ComWrapper<T>`, so a plugin sees the standard `IFoo` vtables, with
//! every method body under the panic guard. The crate root re-exports what a
//! caller needs.

mod attr_list;
mod component_handler;
mod event_list;
mod host_application;
mod message;
mod param_changes;
mod param_queue;
mod plug_frame;
#[cfg(target_os = "linux")]
mod run_loop;
mod stream;

/// What one run-loop pump actually did, plus what the plugin has registered.
///
/// Only meaningful to assert on: "the plugin registered a timer and the pump
/// fired it" is otherwise invisible from outside — the handlers are plugin-side
/// COM objects and the effects land in the plugin's own GUI.
///
/// Defined on every platform even though only Linux has a host-provided run
/// loop, so a conformance test can read it without a `cfg`.
#[cfg(feature = "conformance")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunLoopActivity {
    /// Timers the plugin currently has registered.
    pub timers_registered: usize,
    /// File descriptors the plugin currently has registered.
    pub event_handlers_registered: usize,
    /// Cumulative `ITimerHandler::onTimer` calls this loop has made.
    pub timers_fired: u64,
    /// Cumulative `IEventHandler::onFDIsSet` calls this loop has made.
    pub fds_dispatched: u64,
}

pub use component_handler::{
    ComponentHandler, NOTIFICATION_CAPACITY, PROGRESS_DESCRIPTION_CAPACITY, ParameterEditEvent,
    PluginNotification, ProgressDescription, ProgressEvent, RestartFlags, UnitEvent,
};
pub use event_list::{EventList, event_list_ptr};
pub(crate) use host_application::HostApplication;
pub use param_changes::{ParameterChangesImpl, param_changes_ptr};
pub use param_queue::MIN_POINT_ROOM;
pub(crate) use plug_frame::HostPlugFrame;
#[cfg(target_os = "linux")]
pub(crate) use run_loop::RunLoop;
pub(crate) use stream::BStream;
