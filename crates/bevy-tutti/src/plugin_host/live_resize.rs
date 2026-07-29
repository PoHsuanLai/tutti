//! macOS live-resize observer.
//!
//! Bevy's `Update` schedule does not run during AppKit's modal live-
//! resize tracking, so any plugin format that requires explicit
//! `set_size` to reflow (CLAP, AU) lags one or more frames behind the
//! host edge during a drag. We hook `NSWindowDidResizeNotification`
//! directly, which fires on every step inside the tracking loop, and
//! invoke a host-supplied resize callback synchronously.

#![cfg(target_os = "macos")]

use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, Message};
use objc2_app_kit::{NSView, NSWindowDidResizeNotification};
use objc2_foundation::{MainThreadMarker, NSNotification, NSNotificationCenter};

use bevy_ecs::prelude::{NonSendMut, With};

use crate::plugin_host::native_window::native_view_ptr;

/// Closure invoked from inside AppKit's resize tracking loop with the
/// host NSView's current logical content size.
pub(crate) type ResizeCallback = Arc<dyn Fn(u32, u32) + Send + Sync>;

pub(crate) struct ObserverIvars {
    host_view: Retained<NSView>,
    callback: ResizeCallback,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "DawaiPluginLiveResizeObserver"]
    #[ivars = ObserverIvars]
    pub(crate) struct LiveResizeObserver;

    impl LiveResizeObserver {
        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _note: &NSNotification) {
            let ivars = self.ivars();
            let bounds = ivars.host_view.bounds();
            let w = bounds.size.width.round() as u32;
            let h = bounds.size.height.round() as u32;
            (ivars.callback)(w, h);
        }
    }

    unsafe impl NSObjectProtocol for LiveResizeObserver {}
);

/// RAII wrapper around a registered observer; drops it from the
/// notification center on `Drop`.
///
/// **Deliberately not `Send`/`Sync`.** It owns a `Retained<NSView>` and its
/// `Drop` calls `NSNotificationCenter::removeObserver`; AppKit is main-thread
/// only, and an off-main `removeObserver` is a hard crash on macOS. This type
/// previously carried `unsafe impl Send`/`Sync` purely so it could sit inside
/// a plain Bevy `Component` — which put its drop wherever a `Commands` queue
/// happened to be applied (e.g. `plugin_health_poll`, which is *not*
/// main-thread pinned) or wherever the `World` was torn down.
///
/// It now lives in [`LiveResizeRegistry`], a `NonSend` resource, so Bevy
/// itself enforces main-thread access. `Drop` additionally re-checks the
/// thread and leaks rather than crashing if it ever runs off-main.
pub(crate) struct LiveResizeHandle {
    observer: Option<Retained<LiveResizeObserver>>,
}

impl LiveResizeHandle {
    /// Install a live-resize observer on `host`'s NSWindow.
    ///
    /// # Safety
    /// Must be called on the main thread; `host` must be a valid
    /// AppKit window handle.
    pub(crate) unsafe fn install(
        host: raw_window_handle::RawWindowHandle,
        callback: ResizeCallback,
    ) -> Option<Self> {
        let _mtm = MainThreadMarker::new()?;

        let host_view: &NSView = unsafe { &*(native_view_ptr(host)? as *const NSView) };
        let host_window = host_view.window()?;

        let ivars = ObserverIvars {
            host_view: host_view.retain(),
            callback,
        };

        let alloc = LiveResizeObserver::alloc().set_ivars(ivars);
        let observer: Retained<LiveResizeObserver> = unsafe { msg_send![super(alloc), init] };

        let center = NSNotificationCenter::defaultCenter();
        unsafe {
            center.addObserver_selector_name_object(
                &observer,
                sel!(windowDidResize:),
                Some(NSWindowDidResizeNotification),
                Some(host_window.as_ref() as &AnyObject),
            );
        }

        Some(Self {
            observer: Some(observer),
        })
    }
}

impl Drop for LiveResizeHandle {
    fn drop(&mut self) {
        let Some(observer) = self.observer.take() else {
            return;
        };
        // Defence in depth. `LiveResizeRegistry` is `NonSend`, so Bevy should
        // already guarantee we are on the main thread — but a drop is easy to
        // move by accident, and `removeObserver` off-main is a hard crash, not
        // a warning. If we are not on the main thread, deliberately leak the
        // observer: it keeps a retain on an object AppKit still knows about,
        // which is inert, whereas the crash is not recoverable.
        if MainThreadMarker::new().is_none() {
            debug_assert!(
                false,
                "LiveResizeHandle dropped off the main thread; leaking the \
                 AppKit observer rather than calling removeObserver off-main"
            );
            std::mem::forget(observer);
            return;
        }
        let center = NSNotificationCenter::defaultCenter();
        unsafe {
            center.removeObserver(&observer);
        }
    }
}

/// Main-thread-only home for the live-resize observers, keyed by the plugin
/// entity that owns the editor.
///
/// Inserted as a `NonSend` resource by `TuttiHostingPlugin`, so every system
/// that touches it — and therefore every install and every drop — is pinned to
/// the main thread by Bevy's own scheduler. This is what replaces the unsound
/// `unsafe impl Send + Sync` that let the handle ride inside a `Component`.
#[derive(Default)]
pub struct LiveResizeRegistry {
    handles: std::collections::HashMap<bevy_ecs::entity::Entity, LiveResizeHandle>,
}

impl LiveResizeRegistry {
    /// Store the observer for `entity`, replacing (and dropping, on this
    /// thread) any observer it already had.
    pub(crate) fn insert(&mut self, entity: bevy_ecs::entity::Entity, handle: LiveResizeHandle) {
        self.handles.insert(entity, handle);
    }

    /// Drop `entity`'s observer, if any. Must be called from a main-thread
    /// system — which `NonSendMut<LiveResizeRegistry>` guarantees.
    pub(crate) fn remove(&mut self, entity: bevy_ecs::entity::Entity) {
        self.handles.remove(&entity);
    }

    /// Drop every observer whose owning entity no longer has an open editor.
    pub(crate) fn retain_live(&mut self, is_live: impl Fn(bevy_ecs::entity::Entity) -> bool) {
        self.handles.retain(|entity, _| is_live(*entity));
    }
}

/// Reaps observers whose plugin lost its `PluginEditorOpen` without going
/// through `set_editor_visible_observer` — most importantly
/// `plugin_health_poll`, which is *not* main-thread pinned and used to
/// drop the observer wherever its `Commands` queue happened to be applied.
///
/// `NonSendMut` pins this system to the main thread, so the AppKit
/// `removeObserver` in `LiveResizeHandle::drop` always runs where it is legal.
pub fn reap_orphaned_live_resize_observers(
    mut registry: NonSendMut<LiveResizeRegistry>,
    open: bevy_ecs::system::Query<
        bevy_ecs::entity::Entity,
        With<crate::plugin_host::editor::PluginEditorOpen>,
    >,
) {
    use bevy_ecs::entity::EntityHashSet;
    let live: EntityHashSet = open.iter().collect();
    registry.retain_live(|e| live.contains(&e));
}

#[cfg(test)]
mod tests {
    use super::*;

    // `LiveResizeHandle` owns a `Retained<NSView>`
    // and its `Drop` calls AppKit's `removeObserver`, which is a hard crash
    // off the main thread. It previously carried `unsafe impl Send`/`Sync`
    // solely so it could ride inside a `Component` (Bevy requires
    // `Component: Send + Sync`), which put that drop wherever a `Commands`
    // queue was applied or the `World` was torn down. These tests pin the
    // fix: reinstating either impl to squeeze it back into a component makes
    // them fail.
    use std::marker::PhantomData;

    /// Autoref specialization: the inherent `check` (which requires
    /// `T: Send`) shadows the trait `check` on `&Probe<T>` when it applies.
    /// The resolution must happen at a site where `T` is concrete — wrapping
    /// this in a generic `fn is_send<T>()` silently always returns `false`,
    /// which is why `send_probe_actually_discriminates` exists.
    struct Probe<T: ?Sized>(PhantomData<T>);
    trait NotSendFallback {
        fn check(&self) -> bool {
            false
        }
    }
    impl<T: ?Sized> NotSendFallback for &Probe<T> {}
    impl<T: ?Sized + Send> Probe<T> {
        fn check(&self) -> bool {
            true
        }
    }

    macro_rules! is_send {
        ($t:ty) => {
            (&Probe::<$t>(PhantomData)).check()
        };
    }

    /// The probe itself must discriminate, or the assertions below — which
    /// assert a *negative* — would pass no matter what the types are.
    #[test]
    fn send_probe_actually_discriminates() {
        assert!(is_send!(u32), "probe failed to detect a Send type");
        assert!(
            !is_send!(std::rc::Rc<u32>),
            "probe failed to detect a !Send type"
        );
    }

    #[test]
    fn live_resize_handle_is_not_send() {
        assert!(
            !is_send!(LiveResizeHandle),
            "LiveResizeHandle must not be Send: its Drop calls AppKit \
             removeObserver, which is a hard crash off the main thread"
        );
    }

    #[test]
    fn live_resize_registry_is_not_send() {
        assert!(
            !is_send!(LiveResizeRegistry),
            "LiveResizeRegistry must not be Send; it must stay a NonSend \
             resource so Bevy pins every observer drop to the main thread"
        );
    }
}
