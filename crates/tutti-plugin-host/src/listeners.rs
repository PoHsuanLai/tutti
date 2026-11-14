//! Main-thread listeners for plugin-originated events.
//!
//! The bridge thread pushes `BridgeEvent`s into these through
//! `PluginClient`'s installed listener; `PluginHandle` users register
//! callbacks through [`PluginHandle::subscribe`](crate::PluginHandle::subscribe).
//!
//! The plugin→host notifications split **by consequence**, one
//! [`Listener`] each:
//! - [`ParameterChangeSink`]: the plugin edited one of its own parameters
//!   (targeted, cosmetic; carries the [`ParamEdit`], with its gesture, inline).
//! - [`RefreshSink`]: a cached *view* is stale ([`PluginRefresh`]); re-read it,
//!   no graph edit.
//! - [`InvalidateSink`]: the plugin changed structurally
//!   ([`PluginInvalidation`], incl. latency); rewire + re-plan PDC.
//! - [`TransportSink`]: the plugin asked the transport to do something
//!   ([`TransportRequest`]); the application decides, and may do nothing.
//!
//! Each fans out to its subscribers, each ended by dropping its
//! [`Subscription`]; there is no slot to replace and nothing to clear.

use parking_lot::Mutex;
use std::sync::Arc;

use tutti_plugin_types::ParamEdit;
use tutti_plugin_types::{PluginInvalidation, PluginRefresh, TransportRequest};

/// A fan-out of callbacks for one kind of plugin notification, `A`.
///
/// Cloneable and shared: the side that hears the plugin holds one end and
/// feeds it with [`fire`](Self::fire), and every subscriber hears each
/// notification until its [`Subscription`] drops. (Not called `Sink`:
/// `tutti_buffers::Sink` is the engine's audio edge.)
pub struct Listener<A> {
    inner: Arc<Mutex<Subscribers<A>>>,
}

type Callback<A> = Arc<dyn Fn(A) + Send + Sync>;

struct Subscribers<A> {
    next: u64,
    list: Vec<(u64, Callback<A>)>,
}

/// A callback's registration with a [`Listener`]: it hears notifications until
/// this drops.
#[must_use = "a dropped subscription unsubscribes"]
pub struct Subscription {
    // Type-erased: removes the callback from the listener it came from.
    unsubscribe: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Subscription {
    /// One subscription that ends all of `parts` when it drops.
    pub(crate) fn all(parts: impl IntoIterator<Item = Subscription>) -> Self {
        let parts: Vec<Subscription> = parts.into_iter().collect();
        Self {
            unsubscribe: Some(Box::new(move || drop(parts))),
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(f) = self.unsubscribe.take() {
            f();
        }
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription").finish_non_exhaustive()
    }
}

/// What a plugin reported, through whichever of the three listeners heard it
/// ([`PluginHandle::subscribe`](crate::PluginHandle::subscribe)).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Notice {
    /// The plugin edited one of its own parameters.
    Param(ParamEdit),
    /// A cached view of the plugin is stale (re-read it).
    Refresh(PluginRefresh),
    /// The plugin changed structurally (latency, tail, buses, a reload).
    Invalidate(PluginInvalidation),
    /// The plugin asked the host's transport to do something. The host owns
    /// the transport and may refuse.
    Transport(TransportRequest),
}

/// A plugin-initiated parameter edit: a [`ParamEdit`] (the parameter, its value
/// and where in a gesture it fell).
pub type ParameterChangeSink = Listener<ParamEdit>;
/// **Cosmetic** refresh signals ([`PluginRefresh`]): a cached view (param
/// values / titles) is stale. The callback re-reads from the plugin; the audio
/// graph is untouched.
pub type RefreshSink = Listener<PluginRefresh>;
/// **Structural** invalidation signals ([`PluginInvalidation`]): the plugin
/// changed latency / bus layout, or reloaded, so the graph plan is stale and
/// PDC must re-run. Latency and IO changes share one listener because they
/// demand the identical host response.
pub type InvalidateSink = Listener<PluginInvalidation>;
/// **Transport requests** ([`TransportRequest`]): the plugin asked the host to
/// start, stop, jump or loop. Each is delivered, in the order made.
pub type TransportSink = Listener<TransportRequest>;

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
            inner: Arc::new(Mutex::new(Subscribers {
                next: 0,
                list: Vec::new(),
            })),
        }
    }
}

impl<A> std::fmt::Debug for Listener<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener").finish_non_exhaustive()
    }
}

impl<A: Clone> Listener<A> {
    /// An empty listener, which drops every notification until a callback is
    /// installed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `f`, which hears every notification fired until the returned
    /// [`Subscription`] drops. Any number may be registered at once.
    pub(crate) fn subscribe<F: Fn(A) + Send + Sync + 'static>(&self, f: F) -> Subscription
    where
        A: 'static,
    {
        let mut inner = self.inner.lock();
        let id = inner.next;
        inner.next += 1;
        inner.list.push((id, Arc::new(f)));
        let shared = Arc::clone(&self.inner);
        Subscription {
            unsubscribe: Some(Box::new(move || {
                shared.lock().list.retain(|(n, _)| *n != id);
            })),
        }
    }

    /// Calls every subscriber with `event`. The callbacks run outside the lock
    /// (their `Arc`s are cloned out), so one may subscribe or unsubscribe from
    /// inside.
    ///
    /// Public for `tutti-plugin`'s bridge thread, which feeds a served plugin's notifications in.
    pub fn fire(&self, event: A) {
        let callbacks: Vec<Callback<A>> = self
            .inner
            .lock()
            .list
            .iter()
            .map(|(_, cb)| Arc::clone(cb))
            .collect();
        for cb in callbacks {
            cb(event.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn counter(listener: &Listener<u32>, add: u32) -> (Arc<AtomicU32>, Subscription) {
        let seen = Arc::new(AtomicU32::new(0));
        let s = Arc::clone(&seen);
        let sub = listener.subscribe(move |n| {
            s.fetch_add(n * add, Ordering::Relaxed);
        });
        (seen, sub)
    }

    /// Every subscriber hears each notification, through every clone of the
    /// listener, until its own subscription drops; a second subscriber does
    /// not replace the first.
    #[test]
    fn every_subscriber_hears_every_clone_until_its_subscription_drops() {
        let listener = Listener::<u32>::new();
        let other = listener.clone();
        listener.fire(1); // nobody subscribed: dropped
        let (first, first_sub) = counter(&listener, 1);
        other.fire(2);
        assert_eq!(first.load(Ordering::Relaxed), 2, "a clone shares the list");
        let (second, second_sub) = counter(&listener, 10);
        listener.fire(3);
        assert_eq!(
            (
                first.load(Ordering::Relaxed),
                second.load(Ordering::Relaxed)
            ),
            (5, 30),
            "both hear it"
        );
        drop(first_sub);
        listener.fire(4);
        assert_eq!(
            (
                first.load(Ordering::Relaxed),
                second.load(Ordering::Relaxed)
            ),
            (5, 70),
            "the dropped one stopped, the other did not"
        );
        drop(second_sub);
        listener.fire(5);
        assert_eq!(second.load(Ordering::Relaxed), 70);
    }

    /// **A callback may drop its own subscription and subscribe again from
    /// inside the call.** `fire` calls a snapshot of the callbacks cloned out of
    /// the lock; one that held the lock across the calls would deadlock here,
    /// which the join deadline turns into a failure.
    #[test]
    fn a_callback_may_unsubscribe_and_subscribe_from_inside_a_fire() {
        use std::sync::mpsc;
        use std::time::Duration;
        let listener = Listener::<u32>::new();
        let late = Arc::new(AtomicU32::new(0));
        let own: Arc<Mutex<Option<Subscription>>> = Arc::default();
        let again: Arc<Mutex<Vec<Subscription>>> = Arc::default();
        let sub = {
            let (listener, own, again, late) =
                (listener.clone(), own.clone(), again.clone(), late.clone());
            listener.clone().subscribe(move |_| {
                // Drop this callback's own subscription, then add another.
                drop(own.lock().take());
                let late = late.clone();
                again.lock().push(listener.subscribe(move |n| {
                    late.fetch_add(n, Ordering::Relaxed);
                }));
            })
        };
        *own.lock() = Some(sub);
        let (done, finished) = mpsc::channel();
        let fired = listener.clone();
        std::thread::spawn(move || {
            fired.fire(1);
            fired.fire(2);
            let _ = done.send(());
        });
        finished
            .recv_timeout(Duration::from_secs(5))
            .expect("fire deadlocked on a callback that changed its own subscribers");
        assert_eq!(
            late.load(Ordering::Relaxed),
            2,
            "the callback added inside the first fire hears the second; the first one, unsubscribed, ran once"
        );
    }
}
