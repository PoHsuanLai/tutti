//! Butler thread body: the async main loop.
//!
//! Runs on the butler thread inside `smol::block_on`. Owns `Local`, borrows
//! `Handles` and config from its caller. The actual command logic lives in
//! `handlers`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use smol::channel::Receiver;
use smol::Timer;

use super::command::ButlerCommand;
use super::config::BufferConfig;
use super::handlers::{handle_command, handle_seek_stream, Handles, Local};
use super::io::refill::{refill_all, refill_all_parallel};
use super::loops::handle_loops;
use super::preroll::apply_pdc_updates;

/// Main butler thread entry point (async).
pub(super) async fn butler_loop_async(
    rx: Receiver<ButlerCommand>,
    shared: Handles,
    config: BufferConfig,
    sample_rate: f64,
    shutdown: Arc<AtomicBool>,
) {
    let base_chunk_size = config.chunk_size;
    let parallel_io = config.parallel_io;

    let mut local = Local::new(base_chunk_size);

    loop {
        // Check shutdown
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        // Drain all immediately available commands (non-blocking)
        while let Ok(cmd) = rx.try_recv() {
            handle_command(cmd, &shared, &config, sample_rate, &mut local);
        }

        // Idle: race command recv against 1ms timer
        if shared.plans.is_empty() {
            let timeout = async {
                Timer::after(Duration::from_millis(1)).await;
                Err(smol::channel::RecvError)
            };
            if let Ok(cmd) = futures_lite::future::or(rx.recv(), timeout).await {
                handle_command(cmd, &shared, &config, sample_rate, &mut local);
            }
            continue;
        }

        // Active: synchronous CPU-bound work (unchanged)
        apply_pdc_updates(
            &shared.pdc,
            &shared.plans,
            &mut local.regions,
            &shared.cache,
            &shared.metrics,
            &config,
        );

        // Audio-thread-requested timeline seeks: apply BEFORE refill so the
        // ring refills from the new disk offset this cycle.
        apply_seek_requests(&shared, &config, &mut local);

        handle_loops(
            &shared.plans,
            &mut local.regions,
            &shared.cache,
            &shared.metrics,
        );

        if parallel_io && shared.plans.len() >= 3 {
            refill_all_parallel(
                &shared.plans,
                &mut local.regions,
                &shared.cache,
                &shared.metrics,
                base_chunk_size,
                local.buffer_margin,
            );
        } else {
            refill_all(
                &shared.plans,
                &mut local.regions,
                &shared.cache,
                &shared.metrics,
                base_chunk_size,
                local.buffer_margin,
                &mut local.interleave_buffer,
            );
        }

        // Adaptive pacing: if every active ring buffer is above its refill
        // threshold, nothing is urgent — race the command channel against a
        // short timer (like the idle branch) instead of spinning this
        // max-priority thread. A genuine refill need keeps a buffer below
        // threshold, which drops us straight back to yield-and-loop, so this
        // adds no latency to refills.
        if buffers_healthy(&shared.plans, local.buffer_margin) {
            let timeout = async {
                Timer::after(Duration::from_millis(HEALTHY_SLEEP_MS)).await;
                Err(smol::channel::RecvError)
            };
            if let Ok(cmd) = futures_lite::future::or(rx.recv(), timeout).await {
                handle_command(cmd, &shared, &config, sample_rate, &mut local);
            }
            continue;
        }

        // Yield so any spawned async tasks can progress
        futures_lite::future::yield_now().await;
    }
}

/// Apply any audio-thread-requested timeline seeks. For each streaming channel
/// whose [`RtState`](super::rt_state::RtState) has a fresh seek request (epoch
/// changed vs. the butler's last-applied), reposition the live stream to the
/// requested absolute file offset via the same click-free path as PDC/loop
/// reposition. Coalesces rapid seeks (only the latest target survives) — the
/// desired behavior for scrubbing.
///
/// Channel indices + targets are collected first so the plan refs are released
/// before [`handle_seek_stream`] re-acquires them (avoids DashMap re-entrancy).
fn apply_seek_requests(shared: &Handles, config: &BufferConfig, local: &mut Local) {
    let mut pending: Vec<(usize, u64)> = Vec::new();
    for entry in shared.plans.iter() {
        let plan = entry.value();
        if plan.link.is_none() {
            continue;
        }
        if let Some(target) = plan.rt_state.take_seek_request() {
            pending.push((*entry.key(), target));
        }
    }

    for (channel_index, file_position) in pending {
        handle_seek_stream(channel_index, file_position, shared, config, local);
    }
}

/// Short sleep (ms) taken when all active buffers are healthy — long enough to
/// stop the thread from busy-spinning, short enough to stay well inside the
/// smallest ring buffer's drain time so refills never fall behind.
const HEALTHY_SLEEP_MS: u64 = 3;

/// True when no stream needs a refill — i.e. the loop can safely park on a
/// short timer instead of spinning.
fn buffers_healthy(
    plans: &dashmap::DashMap<usize, super::plan::ChannelPlan>,
    buffer_margin: f64,
) -> bool {
    let fill_threshold = (0.75 / buffer_margin) as f32;

    // Every streaming channel must be at or above its refill threshold. A
    // channel with no active link imposes no refill work.
    plans.iter().all(|entry| {
        let plan = entry.value();
        if plan.link.is_none() {
            return true;
        }
        plan.rt_state.buffer_fill() >= fill_threshold
    })
}
