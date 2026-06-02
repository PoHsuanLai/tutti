//! Shared async scaffolding for non-realtime subsystem work.
//!
//! Several Tutti subsystems do bounded-but-blocking work that must NOT
//! run on the realtime audio thread *or* stall the Bevy main thread:
//! offline export, region rendering, plugin-catalog scanning, host-side
//! wave import + peak computation, SoundFont decoding. The blessed shape
//! for that work is Bevy's [`AsyncComputeTaskPool`] + a stored
//! [`Task<T>`], drained once per frame.
//!
//! [`AsyncComputeTaskPool`]: bevy_tasks::AsyncComputeTaskPool
//!
//! # The convention (every subsystem follows this)
//!
//! 1. **Kick off** in a trigger system:
//!    ```rust,ignore
//!    use bevy_tasks::AsyncComputeTaskPool;
//!    let task = AsyncComputeTaskPool::get().spawn(async move {
//!        // pure blocking work, no ECS access, owns its inputs
//!        do_blocking_work(inputs) // -> Result<Output, Error>
//!    });
//!    commands.entity(e).insert(XInProgress { task });
//!    // …or, for non-entity work, store it in a small in-flight resource
//!    // local to the subsystem module (never the shared `resources.rs`).
//!    ```
//! 2. **Drain** in a poll system with [`poll_task`]:
//!    ```rust,ignore
//!    fn poll_x(mut commands: Commands, mut q: Query<(Entity, &mut XInProgress)>) {
//!        for (e, mut prog) in &mut q {
//!            if let Some(result) = poll_task(&mut prog.task) {
//!                let mut ent = commands.entity(e);
//!                ent.remove::<XInProgress>();
//!                match result {
//!                    Ok(out) => { ent.insert(XComplete(out)); }
//!                    Err(err) => { ent.insert(XFailed(err)); }
//!                }
//!            }
//!        }
//!    }
//!    ```
//!
//! This is a documentation convention plus one helper function, NOT a
//! runtime trait — the shapes differ enough (entity-scoped vs resource,
//! progress channel or not) that a `TuttiSubsystem` trait would only
//! obscure them. The canonical reference implementation this mirrors is
//! `dawai-frontend/src/extensions/lifecycle.rs::poll_in_flight_activations`.
//!
//! ## Progress reporting
//!
//! When a UI wants intermediate progress (export phase, import peaks),
//! capture a [`crossbeam_channel`] sender into the async closure and keep
//! the receiver beside the `Task<T>`; the poll system reads any pending
//! progress before checking for completion. Subsystems that only need
//! Pending/Done skip the channel.
//!
//! ## What this is NOT for
//!
//! The realtime CPAL callback, the sampler butler/streaming thread, PDC,
//! lock-free param atomics, and metering snapshots stay exactly as they
//! are. This helper is only for one-shot, non-RT background jobs.

use bevy_tasks::Task;
use bevy_tasks::block_on;
use bevy_tasks::futures_lite::future;

/// Poll a [`Task`] exactly once without blocking.
///
/// Returns `Some(output)` if the task has finished (the output is moved
/// out and the task is consumed by the caller's `match`/drop), or `None`
/// if it is still running — in which case the task is left untouched and
/// should be polled again next frame.
///
/// This wraps `block_on(poll_once(task))`: despite the name, `block_on`
/// here does not block, because `poll_once` resolves immediately to
/// `None` while the inner future is pending. It is the same idiom used by
/// `poll_in_flight_activations` in dawai-frontend.
///
/// The output type is fully generic; subsystems typically use
/// `T = Result<Output, Error>` and `match` on the returned result, but
/// any `T` works (e.g. a bare value for infallible jobs).
#[inline]
pub fn poll_task<T>(task: &mut Task<T>) -> Option<T> {
    block_on(future::poll_once(task))
}

#[cfg(test)]
mod tests {
    use super::poll_task;
    use bevy_tasks::AsyncComputeTaskPool;

    #[test]
    fn poll_task_yields_none_then_some() {
        // The compute pool must exist before spawning.
        let pool = AsyncComputeTaskPool::get_or_init(Default::default);
        let mut task = pool.spawn(async { 21_u32 * 2 });

        // Drive it to completion (a trivial async block may already be
        // ready on the first poll; loop to be robust either way).
        let mut out = None;
        for _ in 0..10_000 {
            if let Some(v) = poll_task(&mut task) {
                out = Some(v);
                break;
            }
        }
        assert_eq!(out, Some(42));
    }
}
