//! Main-thread listener sinks for plugin-originated events.
//!
//! The bridge thread pushes `BridgeEvent`s into these sinks via
//! `PluginClient`'s installed listener; `PluginHandle` users register
//! callbacks through `on_latency_changed` / `on_parameter_changed`.
//!
//! Each sink holds a single listener (last writer wins). That keeps the
//! API trivial — higher layers that want fan-out can wrap the callback.

use parking_lot::Mutex;
use std::sync::Arc;

type LatencyCb = Arc<dyn Fn(usize) + Send + Sync>;
type ParamCb = Arc<dyn Fn(u32, f32) + Send + Sync>;

#[derive(Clone, Default)]
pub(crate) struct LatencyChangeSink {
    inner: Arc<Mutex<Option<LatencyCb>>>,
}

impl LatencyChangeSink {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set<F: Fn(usize) + Send + Sync + 'static>(&self, f: F) {
        *self.inner.lock() = Some(Arc::new(f));
    }

    pub(crate) fn clear(&self) {
        *self.inner.lock() = None;
    }

    pub(crate) fn fire(&self, samples: usize) {
        // Clone the Arc out of the lock so the callback runs unlocked —
        // callers are free to re-enter (e.g. install a new callback from
        // inside the current one).
        let cb = self.inner.lock().clone();
        if let Some(cb) = cb {
            cb(samples);
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct ParameterChangeSink {
    inner: Arc<Mutex<Option<ParamCb>>>,
}

impl ParameterChangeSink {
    pub(crate) fn new() -> Self {
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
