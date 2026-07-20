//! Platform helpers for native window handles.
//!
//! Thin wrappers around OS APIs for plugin editor window management. Bevy-free:
//! all functions speak [`raw_window_handle::RawWindowHandle`] (the platform-neutral
//! trait Bevy, winit, and wgpu all implement), so any host — Bevy or not — can
//! reuse them. The Bevy call sites unwrap `bevy_window::RawHandleWrapper`
//! (`.get_window_handle()`) before calling in.

/// Extract a u64 native handle pointer from a [`raw_window_handle::RawWindowHandle`].
///
/// Returns the NSView pointer (macOS), HWND (Windows), or X11/Wayland
/// window/surface ID.
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

/// Attach a child window to a parent so they move together.
///
/// - **macOS**: `addChildWindow:ordered:` — child follows parent.
/// - **Windows**: `SetWindowLongPtrW(GWL_HWNDPARENT)` — owned window.
/// - **Linux**: No-op — X11/Wayland don't support toplevel parent-child.
pub fn attach_child_window(
    child: raw_window_handle::RawWindowHandle,
    parent: raw_window_handle::RawWindowHandle,
) {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::{NSView, NSWindowOrderingMode};

        unsafe {
            let child_view: &NSView =
                &*(native_view_ptr(child).unwrap() as *const NSView);
            let parent_view: &NSView =
                &*(native_view_ptr(parent).unwrap() as *const NSView);

            let child_window = child_view.window().expect("child must be in a window");
            let parent_window = parent_view.window().expect("parent must be in a window");

            parent_window.addChildWindow_ordered(&child_window, NSWindowOrderingMode::Above);
        }
    }

    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::RawWindowHandle;
        if let (RawWindowHandle::Win32(child_h), RawWindowHandle::Win32(parent_h)) =
            (child, parent)
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

/// Make every existing subview of `host`'s NSView resize with its
/// parent. Plugins like Surge XT attach their content as a subview
/// of the parent NSView passed to `IPlugView::attached`; without an
/// autoresizing mask they stay fixed during a host edge-drag, which
/// produces a visible flash as the plugin briefly pokes outside (or
/// is clipped by) the new host bounds.
pub fn enable_subview_autoresize(host: raw_window_handle::RawWindowHandle) {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::{NSAutoresizingMaskOptions, NSView};

        unsafe {
            let host_view: &NSView =
                &*(native_view_ptr(host).unwrap() as *const NSView);
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
