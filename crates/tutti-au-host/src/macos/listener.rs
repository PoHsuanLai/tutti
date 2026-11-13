//! Parameter-change notification: seeing what the plugin's own UI does, and
//! writing in a way that tells the plugin's editor.
//!
//! The listener, its events and the notifying writes are `tutti-au-ffi`'s; they
//! are re-exported here under the paths this crate has always used.

pub use tutti_au_ffi::{
    AuEvent, AuParameterListener, EventAddress, emit_gesture, notify_all_parameters,
    notify_parameter, set_parameter_notifying,
};
