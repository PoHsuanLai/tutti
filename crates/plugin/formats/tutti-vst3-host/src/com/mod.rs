//! Host-side COM interface implementations for VST3 hosting.
//!
//! Every type here is reachable from the consumer's perspective only through
//! re-exports at the crate root (notably [`ParameterEditEvent`],
//! [`ProgressEvent`], [`UnitEvent`]). Internally each type lives behind a
//! `ComWrapper<T>` so plugins see the standard `IFoo` vtables.

mod attr_list;
mod component_handler;
mod event_list;
mod host_application;
mod message;
mod param_changes;
mod param_queue;
mod plug_frame;
#[cfg(target_os = "linux")]
pub(crate) mod run_loop;
mod progress;
mod stream;
mod unit_handler;

#[cfg(test)]
mod tests;

/// What one run-loop pump actually did, plus what the plugin has registered.
///
/// Only meaningful to assert on: "the plugin registered a timer and our pump
/// fired it" is otherwise invisible from outside — the handlers are plugin-side
/// COM objects and the effects land in the plugin's own GUI.
///
/// Defined on every platform even though only Linux has a host-provided run
/// loop, so a conformance test can read it unconditionally. Off Linux the OS
/// owns the loop, nothing is ever registered with us, and every field stays
/// zero — see [`Vst3Instance::run_loop_activity`](crate::Vst3Instance::run_loop_activity).
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
    ComponentHandler, ParameterEditEvent, ProgressEvent, RestartFlags, UnitEvent,
};
pub use event_list::{event_list_ptr, EventList};
pub use host_application::HostApplication;
pub use param_changes::{param_changes_ptr, ParameterChangesImpl};
pub(crate) use plug_frame::HostPlugFrame;
pub use stream::BStream;

#[cfg(test)]
pub use param_queue::ParamValueQueueImpl;
#[cfg(test)]
pub use progress::ProgressHandler;
#[cfg(test)]
pub use unit_handler::UnitHandler;
