//! Butler thread controller: spawn, stop, and expose shared handles.
//!
//! The controller is UI-side — callers hold a `ButlerThread`, send commands
//! via `command_sender`, and read metrics / cache / stream states through
//! accessors. The thread body and command handlers live in sibling modules.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use dashmap::DashMap;
use smol::channel::{bounded, Receiver, Sender};
use thread_priority::ThreadPriority;
use tutti_core::RtPublish;
use tutti_core::Samples;

use super::cache::LruCache;
use super::command::ButlerCommand;
use super::config::BufferConfig;
use super::handlers::Handles;
use super::loop_body::butler_loop_async;
use super::metrics::Metrics;
use super::plan::ChannelPlan;

/// Butler thread for asynchronous disk I/O.
pub struct ButlerThread {
    tx: Sender<ButlerCommand>,
    rx: Option<Receiver<ButlerCommand>>,
    thread_handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    shared: Handles,
    config: BufferConfig,
    sample_rate: f64,
}

impl ButlerThread {
    pub fn with_config(channel_capacity: usize, sample_rate: f64, config: BufferConfig) -> Self {
        let (tx, rx) = bounded(channel_capacity);

        let shared = Handles {
            plans: Arc::new(DashMap::new()),
            cache: Arc::new(LruCache::new(
                config.cache_max_entries,
                config.cache_max_bytes,
            )),
            metrics: Arc::new(Metrics::new()),
            pdc: None,
        };

        Self {
            tx,
            rx: Some(rx),
            thread_handle: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            shared,
            config,
            sample_rate,
        }
    }

    /// Subscribe to a per-channel delay-compensation table.
    ///
    /// Published by whoever runs `tutti_core::latency::compensate` over the
    /// audio graph. Readers call `.load()` to obtain a current snapshot.
    pub fn with_pdc(mut self, snapshot: Arc<RtPublish<Vec<Samples>>>) -> Self {
        self.shared.pdc = Some(snapshot);
        self
    }

    pub fn command_sender(&self) -> Sender<ButlerCommand> {
        self.tx.clone()
    }

    pub fn start(&mut self) {
        if self.thread_handle.is_some() {
            return;
        }

        // Fatal-init invariant: `rx` is `Some` for the whole pre-start lifetime
        // and is only taken here. The `thread_handle.is_some()` guard above
        // returns early once the thread is running, so `start()` reaches this
        // point exactly once — the `take()` can never observe `None`.
        let rx = self.rx.take().expect("rx already taken");
        let shutdown = Arc::clone(&self.shutdown);
        let shared = self.shared.clone();
        let config = self.config;
        let sample_rate = self.sample_rate;

        let handle = thread::Builder::new()
            .name("tutti-butler".into())
            .spawn(move || {
                let _ = thread_priority::set_current_thread_priority(ThreadPriority::Max);
                smol::block_on(butler_loop_async(rx, shared, config, sample_rate, shutdown));
            })
            // Fatal init: spawning the disk-I/O butler thread is a prerequisite
            // for all streaming/recording. A spawn failure means the OS is out of
            // threads — unrecoverable at this layer, and every downstream stream
            // command would silently stall on a channel with no consumer. Panic
            // loudly at startup rather than degrade into a mute engine.
            .expect("Failed to spawn butler thread");

        self.thread_handle = Some(handle);
    }

    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.tx.send_blocking(ButlerCommand::Shutdown);

        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }

    /// Access stream states for creating StreamingSamplerUnit instances.
    pub fn plans(&self) -> Arc<DashMap<usize, ChannelPlan>> {
        Arc::clone(&self.shared.plans)
    }
}

impl Drop for ButlerThread {
    fn drop(&mut self) {
        self.stop();
    }
}
