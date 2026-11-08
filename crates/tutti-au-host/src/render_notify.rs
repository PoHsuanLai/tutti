//! Render notifications and sample-accurate parameter scheduling.
//!
//! The registration, the notification vocabulary and `schedule` are
//! `tutti-au-ffi`'s; they are re-exported here under the paths this crate has
//! always used.

#![cfg(target_os = "macos")]

pub use tutti_au_ffi::{
    ParamEvent, RenderNotification, RenderNotify, RenderPhase, RenderUnit, ScheduleAddress,
    schedule,
};
