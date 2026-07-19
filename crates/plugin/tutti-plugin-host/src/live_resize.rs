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

use crate::native_window::native_view_ptr;

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
pub(crate) struct LiveResizeHandle {
    observer: Retained<LiveResizeObserver>,
}

unsafe impl Send for LiveResizeHandle {}
unsafe impl Sync for LiveResizeHandle {}

impl LiveResizeHandle {
    /// Install a live-resize observer on `host`'s NSWindow.
    ///
    /// # Safety
    /// Must be called on the main thread; `host` must be a valid
    /// AppKit window handle.
    pub(crate) unsafe fn install(
        host: &bevy_window::RawHandleWrapper,
        callback: ResizeCallback,
    ) -> Option<Self> {
        let _mtm = MainThreadMarker::new()?;

        let host_view: &NSView =
            unsafe { &*(native_view_ptr(host.get_window_handle())? as *const NSView) };
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

        Some(Self { observer })
    }
}

impl Drop for LiveResizeHandle {
    fn drop(&mut self) {
        let center = NSNotificationCenter::defaultCenter();
        unsafe {
            center.removeObserver(&self.observer);
        }
    }
}
