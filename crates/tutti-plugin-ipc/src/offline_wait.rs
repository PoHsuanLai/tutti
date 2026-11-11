//! [`OfflineWait`] — what an offline fork's batcher waits on, and what it
//! latches when the wait fails.

use crate::ProcessGuard;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;
use tutti_types::SampleRate;

/// An offline fork's wait for the server: the per-block budget, and the
/// latches the batcher sets when the wait fails (a missed budget, a dead
/// server, a plugin that refused its rate).
///
/// The batcher records the failure here and whoever probes the fork's health
/// reads it back ([`gave_up`](Self::gave_up), [`death`](Self::death),
/// [`refusal`](Self::refusal)): a render that ends before the next process
/// poll drops the bridge with the executor, so the latch is what outlives it.
pub struct OfflineWait {
    /// The fork's server process. Weak: a probe the editor keeps must not keep
    /// the fork alive after its node is gone.
    server: Weak<ProcessGuard>,
    /// Latched when a block first misses `budget`.
    gave_up: AtomicBool,
    /// Latched, with its exit status, when the server was found dead.
    died: Mutex<Option<String>>,
    /// Latched when the fork's plugin was found refusing its rate.
    refusal: Mutex<Option<(SampleRate, String)>>,
    budget: Duration,
}

impl std::fmt::Debug for OfflineWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineWait")
            .field("budget", &self.budget)
            .field("gave_up", &self.gave_up)
            .finish_non_exhaustive()
    }
}

impl OfflineWait {
    /// A wait of `budget` per chunk on the server behind `server`, with
    /// nothing latched.
    pub fn new(budget: Duration, server: Weak<ProcessGuard>) -> Self {
        Self {
            server,
            gave_up: AtomicBool::new(false),
            died: Mutex::new(None),
            refusal: Mutex::new(None),
            budget,
        }
    }

    /// The per-block budget (`BridgeConfig::timeout_ms`).
    pub fn budget(&self) -> Duration {
        self.budget
    }

    /// Whether the wait should not wait at all any more: a block already
    /// missed its budget, or the server is dead.
    pub fn stopped_waiting(&self) -> bool {
        self.gave_up() || lock(&self.died).is_some()
    }

    /// Whether a block missed its budget.
    pub fn gave_up(&self) -> bool {
        self.gave_up.load(Ordering::Acquire)
    }

    /// Latch a missed budget.
    pub fn give_up(&self) {
        self.gave_up.store(true, Ordering::Release);
    }

    /// The latched death of the server (a cause or its exit status), if any.
    pub fn death(&self) -> Option<String> {
        lock(&self.died).clone()
    }

    /// Latch a crash the bridge reported, unless a death is latched already.
    pub fn latch_crash(&self, cause: Option<String>) {
        let mut died = lock(&self.died);
        if died.is_none() {
            *died = Some(cause.unwrap_or_else(|| "no cause latched".to_string()));
        }
    }

    /// The latched rate refusal, if any.
    pub fn refusal(&self) -> Option<(SampleRate, String)> {
        lock(&self.refusal).clone()
    }

    /// Latch a rate refusal: the fork renders silence, at once, for the rest
    /// of the render.
    pub fn latch_refusal(&self, refusal: (SampleRate, String)) {
        let mut slot = lock(&self.refusal);
        if slot.is_none() {
            *slot = Some(refusal);
        }
    }

    /// Ask the process whether it has exited, and latch it if so.
    pub fn server_died(&self) -> bool {
        let mut died = lock(&self.died);
        if died.is_some() {
            return true;
        }
        let status = self.server.upgrade().and_then(|s| s.exited());
        if let Some(status) = status {
            *died = Some(format!("plugin-server exited: {status}"));
        }
        died.is_some()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
