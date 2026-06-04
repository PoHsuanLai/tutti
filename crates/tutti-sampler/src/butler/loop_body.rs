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
use super::handlers::{handle_command, Handles, Local};
use super::io::capture::flush_all;
use super::io::loops::handle_loops;
use super::io::pdc::apply_pdc_updates;
use super::io::refill::{refill_all, refill_all_parallel};

/// Main butler thread entry point (async).
pub(super) async fn butler_loop_async(
    rx: Receiver<ButlerCommand>,
    shared: Handles,
    config: BufferConfig,
    sample_rate: f64,
    shutdown: Arc<AtomicBool>,
) {
    let base_chunk_size = config.chunk_size;
    let flush_threshold = config.flush_threshold;
    let parallel_io = config.parallel_io;

    let mut local = Local::new(base_chunk_size);

    loop {
        // Check shutdown
        if shutdown.load(Ordering::SeqCst) {
            flush_all(&mut local.captures, &shared.metrics, flush_threshold, true);
            break;
        }

        // Drain all immediately available commands (non-blocking)
        while let Ok(cmd) = rx.try_recv() {
            handle_command(cmd, &shared, &config, sample_rate, &mut local);
        }

        // Idle: race command recv against 1ms timer
        if shared.plans.is_empty() && local.captures.is_empty() {
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

        flush_all(&mut local.captures, &shared.metrics, flush_threshold, false);

        // Yield so any spawned async tasks can progress
        futures_lite::future::yield_now().await;
    }
}
