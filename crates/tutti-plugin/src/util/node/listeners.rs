//! Main-thread listeners for plugin-originated events.
//!
//! The bridge thread pushes `BridgeEvent`s into these through
//! `PluginClient`'s installed listener; `PluginHandle` users register
//! callbacks through `on_parameter_changed` / `on_refresh` / `on_invalidate`.
//!
//! The plugin→host notifications split **by consequence**, one
//! [`Listener`] each:
//! - [`ParameterChangeSink`]: a single parameter's value was written back
//!   (targeted, cosmetic; carries `(id, value)` inline).
//! - [`RefreshSink`]: a cached *view* is stale ([`PluginRefresh`]); re-read it,
//!   no graph edit.
//! - [`InvalidateSink`]: the plugin changed structurally
//!   ([`PluginInvalidation`], incl. latency); rewire + re-plan PDC.
//!
//! Each holds a single callback (last writer wins). That keeps the API
//! trivial: higher layers that want fan-out can wrap the callback.

use parking_lot::Mutex;
use std::sync::Arc;

use tutti_plugin_ipc::{PluginInvalidation, PluginRefresh};
use tutti_plugin_types::{Normalized, ParamId};

/// A one-slot callback for one kind of plugin notification, `A`.
///
/// Cloneable and shared: the side that hears the plugin holds one end and the
/// host installs the callback through the other. Setting a second callback
/// replaces the first rather than fanning out. (Not called `Sink`:
/// `tutti_types::Sink` is the engine's audio edge.)
pub struct Listener<A> {
    inner: Arc<Mutex<Option<Callback<A>>>>,
}

type Callback<A> = Arc<dyn Fn(A) + Send + Sync>;

/// A plugin-initiated parameter move: `(id, value)`.
pub type ParameterChangeSink = Listener<(ParamId, Normalized)>;
/// **Cosmetic** refresh signals ([`PluginRefresh`]): a cached view (param
/// values / titles) is stale. The callback re-reads from the plugin; the audio
/// graph is untouched.
pub(crate) type RefreshSink = Listener<PluginRefresh>;
/// **Structural** invalidation signals ([`PluginInvalidation`]): the plugin
/// changed latency / bus layout, or reloaded, so the graph plan is stale and
/// PDC must re-run. Latency and IO changes share one listener because they
/// demand the identical host response.
pub(crate) type InvalidateSink = Listener<PluginInvalidation>;

impl<A> Clone for Listener<A> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<A> Default for Listener<A> {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }
}

impl<A> std::fmt::Debug for Listener<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener").finish_non_exhaustive()
    }
}

impl<A> Listener<A> {
    /// An empty listener, which drops every notification until a callback is
    /// installed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs `f`, replacing any callback installed before.
    pub(crate) fn set<F: Fn(A) + Send + Sync + 'static>(&self, f: F) {
        *self.inner.lock() = Some(Arc::new(f));
    }

    /// Removes the callback.
    pub(crate) fn clear(&self) {
        *self.inner.lock() = None;
    }

    /// Calls the callback with `event`, if one is installed. The callback
    /// runs outside the lock (its `Arc` is cloned out), so it may install a
    /// new callback from inside.
    pub(crate) fn fire(&self, event: A) {
        let cb = self.inner.lock().clone();
        if let Some(cb) = cb {
            cb(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn counter(listener: &Listener<u32>, add: u32) -> Arc<AtomicU32> {
        let seen = Arc::new(AtomicU32::new(0));
        let s = Arc::clone(&seen);
        listener.set(move |n| {
            s.fetch_add(n * add, Ordering::Relaxed);
        });
        seen
    }

    // Mutation (run): `set` keeping the first callback (install only when
    // none) fails the second figure; `clear` doing nothing fails the third;
    // a clone with a slot of its own fails the first.
    #[test]
    fn the_last_callback_set_hears_every_clone_until_cleared() {
        let listener = Listener::<u32>::new();
        // Taken before any callback: it must still hear the one set later.
        let other = listener.clone();
        listener.fire(1); // nothing installed: dropped
        let first = counter(&listener, 1);
        other.fire(2);
        assert_eq!(first.load(Ordering::Relaxed), 2, "a clone shares the slot");
        let second = counter(&listener, 10);
        listener.fire(3);
        assert_eq!(
            (
                first.load(Ordering::Relaxed),
                second.load(Ordering::Relaxed)
            ),
            (2, 30),
            "the second callback replaced the first"
        );
        listener.clear();
        listener.fire(4);
        assert_eq!(second.load(Ordering::Relaxed), 30, "cleared");
    }
}
