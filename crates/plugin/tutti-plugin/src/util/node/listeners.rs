//! Main-thread listener sinks for plugin-originated events.
//!
//! The bridge thread pushes `BridgeEvent`s into these sinks via
//! `PluginClient`'s installed listener; `PluginHandle` users register
//! callbacks through `on_parameter_changed` / `on_refresh` / `on_invalidate`.
//!
//! The plugin→host notifications split **by consequence**:
//! - [`ParameterChangeSink`] — a single parameter's value was written back
//!   (targeted, cosmetic; carries `(id, value)` inline).
//! - [`RefreshSink`] — a cached *view* is stale ([`PluginRefresh`]); re-read it,
//!   no graph edit.
//! - [`InvalidateSink`] — the plugin changed structurally
//!   ([`PluginInvalidation`], incl. latency); rewire + re-plan PDC.
//!
//! Each sink holds a single listener (last writer wins). That keeps the
//! API trivial — higher layers that want fan-out can wrap the callback.

use parking_lot::Mutex;
use std::sync::Arc;

use crate::host::ipc_client::audio::{PluginInvalidation, PluginRefresh};

type ParamCb = Arc<dyn Fn(u32, f32) + Send + Sync>;
type RefreshCb = Arc<dyn Fn(PluginRefresh) + Send + Sync>;
type InvalidateCb = Arc<dyn Fn(PluginInvalidation) + Send + Sync>;

#[derive(Clone, Default)]
pub struct ParameterChangeSink {
    inner: Arc<Mutex<Option<ParamCb>>>,
}

impl ParameterChangeSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set<F: Fn(u32, f32) + Send + Sync + 'static>(&self, f: F) {
        *self.inner.lock() = Some(Arc::new(f));
    }

    pub(crate) fn clear(&self) {
        *self.inner.lock() = None;
    }

    pub(crate) fn fire(&self, param_id: u32, value: f32) {
        let cb = self.inner.lock().clone();
        if let Some(cb) = cb {
            cb(param_id, value);
        }
    }
}

/// Sink for **cosmetic** refresh signals ([`PluginRefresh`]): a cached view
/// (param values / titles) is stale. The callback re-reads from the plugin; the
/// audio graph is untouched.
#[derive(Clone, Default)]
pub(crate) struct RefreshSink {
    inner: Arc<Mutex<Option<RefreshCb>>>,
}

impl RefreshSink {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set<F: Fn(PluginRefresh) + Send + Sync + 'static>(&self, f: F) {
        *self.inner.lock() = Some(Arc::new(f));
    }

    pub(crate) fn clear(&self) {
        *self.inner.lock() = None;
    }

    pub(crate) fn fire(&self, refresh: PluginRefresh) {
        // Clone the Arc out of the lock so the callback runs unlocked —
        // callers are free to re-enter (e.g. install a new callback from
        // inside the current one).
        let cb = self.inner.lock().clone();
        if let Some(cb) = cb {
            cb(refresh);
        }
    }
}

/// Sink for **structural** invalidation signals ([`PluginInvalidation`]): the
/// plugin changed latency / bus layout, or reloaded, so the graph plan is stale
/// and PDC must re-run. Absorbs what used to be the separate latency-changed
/// callback — latency and IO changes demand the identical host response.
#[derive(Clone, Default)]
pub(crate) struct InvalidateSink {
    inner: Arc<Mutex<Option<InvalidateCb>>>,
}

impl InvalidateSink {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set<F: Fn(PluginInvalidation) + Send + Sync + 'static>(&self, f: F) {
        *self.inner.lock() = Some(Arc::new(f));
    }

    pub(crate) fn clear(&self) {
        *self.inner.lock() = None;
    }

    pub(crate) fn fire(&self, invalidation: PluginInvalidation) {
        let cb = self.inner.lock().clone();
        if let Some(cb) = cb {
            cb(invalidation);
        }
    }
}
