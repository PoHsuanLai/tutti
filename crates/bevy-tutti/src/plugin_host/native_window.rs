//! Platform helpers for native window handles.
//!
//! Thin wrappers around OS APIs for plugin editor window management. Bevy-free:
//! all functions speak [`raw_window_handle::RawWindowHandle`] (the platform-neutral
//! trait Bevy, winit, and wgpu all implement), so any host — Bevy or not — can
//! reuse them. The Bevy call sites unwrap `bevy_window::RawHandleWrapper`
//! (`.get_window_handle()`) before calling in.

/// Flattens a [`raw_window_handle::RawWindowHandle`] to the one native integer
/// the OS APIs below take.
///
/// That is an `NSView*` on macOS, an `HWND` on Windows, and an X11 window or
/// Wayland surface id on Linux — different things widened to a common `u64`, so
/// a caller must already know which platform it is on before dereferencing one.
///
/// `None` for any handle variant this crate does not host plugin editors on
/// (Android, iOS, web).
pub fn native_view_ptr(raw: raw_window_handle::RawWindowHandle) -> Option<u64> {
    use raw_window_handle::RawWindowHandle;
    match raw {
        RawWindowHandle::AppKit(h) => Some(h.ns_view.as_ptr() as u64),
        RawWindowHandle::Win32(h) => Some(isize::from(h.hwnd) as u64),
        RawWindowHandle::Xlib(h) => Some(h.window),
        RawWindowHandle::Xcb(h) => Some(h.window.get() as u64),
        RawWindowHandle::Wayland(h) => Some(h.surface.as_ptr() as u64),
        _ => None,
    }
}

/// Attach a child window to a parent so the two move together.
///
/// - **macOS**: `addChildWindow:ordered:` — the child follows the parent.
/// - **Windows**: `SetWindowLongPtrW(GWL_HWNDPARENT)` — an owned window.
/// - **Linux**: a no-op. X11 and Wayland have no toplevel parent-child
///   relationship, so plugin windows float independently.
///
/// **Main thread only.** Both the AppKit and Win32 calls are window operations,
/// which those APIs require on the thread that owns the window.
///
/// # Panics
///
/// On macOS, if either handle is not an AppKit handle, or if either view is not
/// yet installed in an `NSWindow`. Call this only once Bevy has created the
/// window and its native handle is available.
pub fn attach_child_window(
    child: raw_window_handle::RawWindowHandle,
    parent: raw_window_handle::RawWindowHandle,
) {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::{NSView, NSWindowOrderingMode};

        unsafe {
            let child_view: &NSView = &*(native_view_ptr(child).unwrap() as *const NSView);
            let parent_view: &NSView = &*(native_view_ptr(parent).unwrap() as *const NSView);

            let child_window = child_view.window().expect("child must be in a window");
            let parent_window = parent_view.window().expect("parent must be in a window");

            parent_window.addChildWindow_ordered(&child_window, NSWindowOrderingMode::Above);
        }
    }

    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::RawWindowHandle;
        if let (RawWindowHandle::Win32(child_h), RawWindowHandle::Win32(parent_h)) = (child, parent)
        {
            unsafe {
                #[cfg(target_pointer_width = "64")]
                type Lparam = isize;
                #[cfg(target_pointer_width = "32")]
                type Lparam = i32;

                const GWL_HWNDPARENT: i32 = -8;

                extern "system" {
                    fn SetWindowLongPtrW(hwnd: isize, index: i32, new_long: Lparam) -> Lparam;
                }

                let child_hwnd = isize::from(child_h.hwnd);
                let parent_hwnd = isize::from(parent_h.hwnd);
                SetWindowLongPtrW(child_hwnd, GWL_HWNDPARENT, parent_hwnd as Lparam);
            }
        }
    }

    #[cfg(all(
        unix,
        not(target_os = "macos"),
        not(target_os = "android"),
        not(target_os = "ios"),
    ))]
    {
        // X11/Wayland: plugin windows float independently (same as Zrythm).
        let _ = (child, parent);
    }
}

/// Make every existing subview of `host`'s `NSView` resize with its parent.
///
/// Plugins like Surge XT attach their content as a subview of the parent
/// `NSView` passed to `IPlugView::attached`; without an autoresizing mask they
/// stay fixed during a host edge-drag, which produces a visible flash as the
/// plugin briefly pokes outside — or is clipped by — the new host bounds.
///
/// The AppKit-friendly half of live resize. A plugin whose format needs an
/// explicit `set_size` instead gets `live_resize`'s notification observer.
///
/// **Main thread only**, and a no-op off macOS. Only subviews present *now* are
/// masked, so call this after the plugin has attached its content.
///
/// # Panics
///
/// If `host` is not an AppKit handle.
pub fn enable_subview_autoresize(host: raw_window_handle::RawWindowHandle) {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::{NSAutoresizingMaskOptions, NSView};

        unsafe {
            let host_view: &NSView = &*(native_view_ptr(host).unwrap() as *const NSView);
            host_view.setAutoresizesSubviews(true);
            let mask = NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable;
            for subview in host_view.subviews().iter() {
                subview.setAutoresizingMask(mask);
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = host;
    }
}
