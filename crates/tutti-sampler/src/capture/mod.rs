//! Internal recording bookkeeping. Public surface lives at
//! `crate::capture` in `lib.rs`.

pub(crate) mod automation_manager;
pub(crate) mod automation_recorder;
pub(crate) mod automation_target;
pub(crate) mod config;
pub(crate) mod events;
pub(crate) mod manager;
pub(crate) mod session;
