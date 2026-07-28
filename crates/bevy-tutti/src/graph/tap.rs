//! The analysis tap's ECS wrapper.
//!
//! The tap is born in [`build_into`](crate::engine::build_into), which shares it
//! with the RT callback and inserts it here. Unlike the meter it is **not**
//! opened at build: while closed, the audio thread's push is one atomic load and
//! a return, so a host that never analyses pays nothing.
//!
//! Before this wrapper existed the tap was constructed, cloned into the
//! callback, and then dropped — the local went out of scope and nothing else
//! held a handle. The callback pushed every block into a ring whose consumer end
//! no ECS code could ever ask for.

use bevy_ecs::prelude::*;

use tutti_core::metering::AudioTap;

/// The master output's lock-free analysis tap.
///
/// Opt-in. Call `open()` through the [`Deref`](std::ops::Deref) to get the
/// consumer end and start paying for the copy; `close()` to stop. The consumer
/// is owned and drained by whoever opened it — it is a ring-buffer half, not
/// something an ECS resource can hold for you, and opening twice orphans the
/// first consumer.
///
/// ```rust,ignore
/// fn start_analysis(tap: Res<AudioTapRes>) {
///     let consumer = tap.open();
///     std::thread::spawn(move || { /* drain `consumer` */ });
/// }
/// ```
#[derive(Resource, Clone, Default)]
pub struct AudioTapRes(pub AudioTap);

impl std::ops::Deref for AudioTapRes {
    type Target = AudioTap;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
