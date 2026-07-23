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
mod progress;
mod stream;
mod unit_handler;

#[cfg(test)]
mod tests;

pub use component_handler::{
    ComponentHandler, ParameterEditEvent, ProgressEvent, RestartFlags, UnitEvent,
};
pub use event_list::{event_list_ptr, EventList};
pub use host_application::HostApplication;
pub use param_changes::{param_changes_ptr, ParameterChangesImpl};
pub(crate) use plug_frame::HostPlugFrame;
pub use stream::BStream;

#[cfg(test)]
#[allow(unused_imports)]
pub use attr_list::AttributeList;
#[cfg(test)]
#[allow(unused_imports)]
pub use message::Message;
#[cfg(test)]
pub use param_queue::ParamValueQueueImpl;
#[cfg(test)]
pub use progress::ProgressHandler;
#[cfg(test)]
pub use unit_handler::UnitHandler;
