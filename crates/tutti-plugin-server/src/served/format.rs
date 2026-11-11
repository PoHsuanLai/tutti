//! What the formats still differ in, that only the server process does.
//!
//! [`ServerSide`] has a default for everything, so a format that differs in
//! nothing is an empty `impl`. What differs is not the format's own behaviour
//! (that is its `-host` crate's, behind [`Control`](tutti_plugin_types::Control)
//! and [`Loaded`](tutti_plugin_types::Loaded)) but what this process does around
//! it: the pool it serves a plugin's thread-pool extension on, what it drops
//! unread, and how a write waiting for a block is delivered early.

use tutti_plugin_types::{Lifecycle, MainThread, OutOfProcess, Request};

/// What the server does to a loaded plugin of format `Self`, beyond what the
/// format's [`OutOfProcess::load_served`] and its `Control` give.
pub(crate) trait ServerSide: OutOfProcess {
    /// Called once on a plugin just loaded, before it is activated: what this
    /// process lends it (CLAP's `clap.thread-pool`).
    fn adopt(&mut self) {}

    /// Called at the head of every pass over what the plugin asked of the host:
    /// what the server has no use for and drops unread, so a bounded queue the
    /// plugin fills holds no backlog (VST3's edits, progress and unit news; the
    /// server has never forwarded them).
    fn before_requests(lifecycle: &Lifecycle<Self>) {
        let _ = lifecycle;
    }

    /// What the plugin raised since the pass's first take, on a later pass of the
    /// same call (a restart a pass performs may make the plugin ask for more).
    /// The first take is [`Control::take_requests`](tutti_plugin_types::Control::take_requests),
    /// which serves what is due on the main thread (CLAP's callbacks, timers and
    /// descriptors); taking it again would serve a period-0 timer on every
    /// pass. The default takes it again, for a format whose take is only what
    /// the plugin raised.
    fn take_raised(lifecycle: &mut Lifecycle<Self>, main: &MainThread, out: &mut Vec<Request>) {
        if let Some(control) = lifecycle.control() {
            control.take_requests(main, out);
        }
    }

    /// Delivers the writes the Processor has not drained, so a state save sees
    /// them: the server serves requests on the thread that renders, so without
    /// this a save right after a write would read the old value until the next
    /// block. The default retries what a full edit ring refused, which is all a
    /// format that applies a write at once needs.
    fn settle_edits(lifecycle: &mut Lifecycle<Self>, main: &MainThread) {
        if let Some(control) = lifecycle.control() {
            control.flush_edits(main);
        }
    }
}

#[cfg(feature = "clap")]
pub(in crate::served) mod clap {
    use tutti_clap_host::ClapLoaded;
    use tutti_plugin_types::{Control, Lifecycle, MainThread, Request, Seat};

    use super::ServerSide;

    /// The pool this process serves `clap.thread-pool` requests on, built the
    /// first time a plugin that implements the extension loads; `None` on a
    /// machine too small for one (below) or that refuses a thread.
    ///
    /// The engine's own pool lives in the host process, and a plugin's `process`
    /// runs here, in another: a request cannot reach it. So the server keeps a
    /// `WorkerPool` of its own, with its helpers asking for the audio thread's
    /// priority (the highest the OS grants, not real time), as `tutti_graph::WorkerPool` requires of helpers. A
    /// plugin that does not implement the extension costs no threads.
    ///
    /// Sized by [`pool_participants`]: half the cores, at most 4, the server's
    /// audio thread included. Each plugin instance has a server process of its
    /// own, so every pooled instance adds a pool, and the engine's pool in the
    /// host wants the same cores; half leaves the other half to them, and the cap
    /// keeps a few pooled instances from oversubscribing a large machine, where
    /// one block's tasks rarely keep more than a few threads busy anyway. Below
    /// two participants (fewer than 4 cores) no pool is built, and requests are
    /// refused: the plugin's own fallback does as well as a pool of one.
    ///
    /// A request is one span of one block, so its helpers spin only briefly and
    /// park soon after (10 µs, then 50 µs of yielding) rather than holding cores
    /// the engine could use between blocks. That costs a request nothing: it
    /// never waits for a helper that has not started a task (the audio thread
    /// runs tasks from the start, and helpers join as they arrive), though a
    /// helper preempted mid-task delays the request.
    pub(in crate::served) fn thread_pool() -> Option<std::sync::Arc<tutti_graph::WorkerPool>> {
        THREAD_POOL
            .get_or_init(|| {
                let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
                let participants = pool_participants(cores);
                if participants < 2 {
                    return None;
                }
                tutti_graph::WorkerPool::builder()
                    .num_threads(participants)
                    .thread_name(|i| format!("tutti-clap-pool-{i}"))
                    .spin_budget(std::time::Duration::from_micros(10))
                    .idle(std::time::Duration::from_micros(50))
                    .start_handler(|_| {
                        crate::server::request_audio_priority("a clap.thread-pool helper")
                    })
                    .build()
                    .inspect_err(|e| {
                        tracing::warn!(
                            "clap.thread-pool: a {participants}-thread pool was refused, \
                             requests are refused: {e}"
                        );
                    })
                    .ok()
                    .map(std::sync::Arc::new)
            })
            .clone()
    }

    /// Participants of the server's pool on a machine with `cores` cores (see
    /// [`thread_pool`]): half of them, at most 4.
    pub(in crate::served) fn pool_participants(cores: usize) -> usize {
        (cores / 2).min(4)
    }

    /// [`thread_pool`]'s pool, once decided.
    pub(in crate::served) static THREAD_POOL: std::sync::OnceLock<
        Option<std::sync::Arc<tutti_graph::WorkerPool>>,
    > = std::sync::OnceLock::new();

    /// Whether a pending write may be flushed out of band: not when the parameter
    /// requires `process` and the plugin is processing.
    pub(in crate::served) fn flush_out_of_band(requires_process: bool, processing: bool) -> bool {
        !(requires_process && processing)
    }

    impl ServerSide for ClapLoaded {
        fn adopt(&mut self) {
            if self.supports_thread_pool() {
                self.set_thread_pool(thread_pool());
            }
        }

        /// Timers and descriptors are polled once per pass of the host's loop;
        /// only what the plugin raised is taken again.
        fn take_raised(
            lifecycle: &mut Lifecycle<Self>,
            _main: &MainThread,
            out: &mut Vec<Request>,
        ) {
            match lifecycle {
                Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => {
                    plugin.take_raised(out);
                }
                Lifecycle::Active { controller, .. } => controller.loaded_mut().take_raised(out),
                Lifecycle::Poisoned => {}
            }
        }

        /// Each is applied through an out-of-band `params.flush`, except that a
        /// parameter flagged `REQUIRES_PROCESS` is never flushed while the plugin
        /// is processing (CLAP wants it in event order, in a block): it goes back
        /// to the queue. Only what was pending is flushed, at the moment it is
        /// asked for, so a value the plugin or its editor has since moved is not
        /// re-asserted by every write.
        fn settle_edits(lifecycle: &mut Lifecycle<Self>, main: &MainThread) {
            if let Some(control) = lifecycle.control() {
                control.flush_edits(main);
            }
            let mut pending = Vec::new();
            let processing = match lifecycle {
                Lifecycle::Active {
                    seat: Seat::Held(processor),
                    ..
                } => {
                    processor.take_edits(&mut pending);
                    processor.is_processing()
                }
                _ => return,
            };
            for (id, value) in pending {
                let requires = match &*lifecycle {
                    Lifecycle::Active { controller, .. } => {
                        controller.loaded().param_requires_process(main, id.get())
                    }
                    _ => false,
                };
                if flush_out_of_band(requires, processing) {
                    if let Lifecycle::Active { controller, .. } = lifecycle {
                        Control::set_parameter(controller.loaded_mut(), main, id, value);
                    }
                } else if let Some(control) = lifecycle.control() {
                    control.set_parameter(main, id, value);
                }
            }
        }
    }
}

#[cfg(feature = "vst3")]
impl ServerSide for tutti_vst3_host::Vst3Loaded {
    /// Edits, progress and unit news the plugin queues are dropped unread, so
    /// the host's bounded queue holds no backlog. Lock-free and allocation-free.
    fn before_requests(lifecycle: &Lifecycle<Self>) {
        match lifecycle {
            Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => {
                plugin.discard_notifications();
            }
            Lifecycle::Active { controller, .. } => {
                controller.loaded().discard_notifications();
            }
            Lifecycle::Poisoned => {}
        }
    }
}

/// AU differs from the default in nothing.
#[cfg(all(feature = "au", target_os = "macos"))]
impl ServerSide for tutti_au_host::AuLoaded {}
