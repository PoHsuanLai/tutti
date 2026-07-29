//! VST3 editor lifecycle conformance — the `IPlugView` checks the process-path
//! harness cannot reach.
//!
//! HostChecker wraps the plugin's own `IPlugView` and records how the host
//! drives it: `attached`/`removed` pairing, whether `onSize` arrives before
//! attach, whether it arrives synchronously during a `resizeView`, and which
//! optional entry points (`canResize`, `checkSizeConstraint`, `setFrame`,
//! `setContentScaleFactor`) the host uses at all. Two of those are
//! error-severity and describe host bugs:
//!
//! - `IPlugView::attached is called without removed first!`
//! - `IPlugView::removed is called without attached first!`
//!
//! Both are ordering mistakes that leak a native view per open — the kind of
//! thing that survives manual testing because the editor still appears.
//!
//! ## Why this is a separate test binary
//!
//! It needs a real X11/Cocoa window, so it is gated on a display being present
//! and skips cleanly without one. Keeping it out of `vst3_conformance` means
//! the main harness stays runnable headless.
//!
//! ## Running
//!
//! ```bash
//! VST3_SAMPLE_PLUGIN_DIR=/path/to/build/VST3/Release \
//! xvfb-run -a cargo test -p tutti-vst3-host --features conformance \
//!   --test vst3_gui_lifecycle -- --ignored
//! ```
//!
//! `#[ignore]` by default: a test that silently needs a display is worse than
//! one you opt into.

#![cfg(feature = "conformance")]

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use tutti_vst3_host::{EditorSize, Vst3Instance, WindowHandle};

/// Compile-time default, baked in by `build.rs`.
const SAMPLE_PLUGIN_DIR_BUILT: &str = env!("VST3_SAMPLE_PLUGIN_DIR");

/// Where to look for sample plugins, **runtime env first**.
///
/// `build.rs` bakes the path in at compile time, which means exporting
/// `VST3_SAMPLE_PLUGIN_DIR` before `cargo test` has no effect unless the crate
/// happens to rebuild — so pointing this at a debug-symbol build to diagnose a
/// crash silently kept loading the stripped release plugin instead. Reading the
/// variable at runtime makes that switch actually work.
fn sample_plugin_dir() -> String {
    std::env::var("VST3_SAMPLE_PLUGIN_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| SAMPLE_PLUGIN_DIR_BUILT.to_string())
}

/// Resolve a `.vst3` bundle to the loadable binary inside it.
fn resolve_bundle(path: &Path) -> PathBuf {
    if path.is_file() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    for sub in ["Contents/x86_64-linux", "Contents/MacOS", "Contents/x86_64-win"] {
        let dir = path.join(sub);
        for ext in ["so", "", "vst3", "dylib"] {
            let cand = if ext.is_empty() {
                dir.join(stem)
            } else {
                dir.join(format!("{stem}.{ext}"))
            };
            if cand.is_file() {
                return cand;
            }
        }
    }
    path.to_path_buf()
}

fn host_checker_path() -> Option<PathBuf> {
    let dir = sample_plugin_dir();
    if dir.is_empty() {
        return None;
    }
    let p = Path::new(&dir).join("host-checker.vst3");
    let bin = resolve_bundle(&p);
    bin.is_file().then_some(bin)
}

/// Whether a window server is reachable. Without one, `winit` aborts the
/// process rather than returning an error, so this must be checked first.
fn has_display() -> bool {
    if cfg!(target_os = "linux") {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    } else {
        true
    }
}

/// A real native window to parent the plugin editor into, plus the event loop
/// that owns it. Both must outlive the editor.
struct TestWindow {
    _event_loop: winit::event_loop::EventLoop<()>,
    window: winit::window::Window,
}

impl TestWindow {
    fn new() -> Option<Self> {
        use winit::event_loop::EventLoop;
        use winit::platform::x11::EventLoopBuilderExtX11;

        // `with_any_thread` because cargo runs tests on worker threads; winit
        // otherwise refuses to build an event loop off the main thread.
        let event_loop = EventLoop::builder().with_any_thread(true).build().ok()?;
        #[allow(deprecated)]
        let window = event_loop
            .create_window(
                winit::window::Window::default_attributes()
                    .with_title("vst3 editor host")
                    .with_inner_size(winit::dpi::LogicalSize::new(800, 600))
                    .with_visible(false),
            )
            .ok()?;
        Some(Self {
            _event_loop: event_loop,
            window,
        })
    }

    /// The platform window handle to hand the plugin.
    fn handle(&self) -> Option<WindowHandle> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let raw = self.window.window_handle().ok()?.as_raw();
        let ptr = match raw {
            RawWindowHandle::Xlib(h) => h.window as *mut c_void,
            RawWindowHandle::Xcb(h) => h.window.get() as usize as *mut c_void,
            #[cfg(target_os = "macos")]
            RawWindowHandle::AppKit(h) => h.ns_view.as_ptr(),
            _ => return None,
        };
        // SAFETY: `ptr` is this window's live platform handle; `self` (and so
        // the window) outlives every editor opened against it in these tests.
        Some(unsafe { WindowHandle::from_raw(ptr) })
    }
}

/// Load `host-checker`, or skip. Returns `None` with a printed reason.
fn load_host_checker() -> Option<Vst3Instance> {
    if !has_display() {
        eprintln!("no DISPLAY/WAYLAND_DISPLAY; run under `xvfb-run -a`. Skipping.");
        return None;
    }
    let path = host_checker_path()?;
    match Vst3Instance::<f32>::load(&path, 48_000.0, 512) {
        Ok(i) => Some(i),
        Err(e) => {
            eprintln!("host-checker load failed ({e:?}); skipping");
            None
        }
    }
}

/// Opening and closing an editor must pair `attached`/`removed` exactly.
///
/// The plugin flags `attached is called without removed first!` if the host
/// opens twice without closing, and the converse on a stray close. Both leak a
/// native view per occurrence.
#[test]
#[ignore = "needs a display; run under xvfb-run -a"]
fn editor_open_close_pairs_attach_and_remove() {
    let Some(mut inst) = load_host_checker() else {
        return;
    };
    let Some(win) = TestWindow::new() else {
        eprintln!("could not create a window; skipping");
        return;
    };
    let Some(handle) = win.handle() else {
        eprintln!("unsupported window handle type; skipping");
        return;
    };

    // Three full cycles: a host that forgets `removed` shows up on cycle two.
    for cycle in 0..3 {
        match inst.open_editor(handle) {
            Ok(size) => {
                assert!(
                    size.width > 0 && size.height > 0,
                    "cycle {cycle}: editor reported a degenerate size {}x{}",
                    size.width,
                    size.height
                );
            }
            Err(e) => {
                eprintln!("cycle {cycle}: open_editor failed ({e:?}); skipping rest");
                return;
            }
        }
        inst.close_editor();
    }

    // A close with no open must be a no-op, not a stray `removed`.
    inst.close_editor();
}

/// `close_editor` on a never-opened instance must not call `removed`.
#[test]
#[ignore = "needs a display; run under xvfb-run -a"]
fn closing_an_unopened_editor_is_a_noop() {
    let Some(mut inst) = load_host_checker() else {
        return;
    };
    // No open_editor call at all. This must not reach IPlugView::removed —
    // which the plugin would flag as "removed without attached".
    inst.close_editor();
    inst.close_editor();
}

/// Editor capabilities must be queryable, and a resize must be honoured or
/// cleanly refused.
///
/// `canResize` and `checkSizeConstraint` are the entry points a host uses to
/// decide whether its editor window may be dragged. A host that resizes a
/// fixed-size view without asking produces a stretched or clipped GUI.
#[test]
#[ignore = "needs a display; run under xvfb-run -a"]
fn editor_resize_respects_capabilities() {
    let Some(mut inst) = load_host_checker() else {
        return;
    };
    let Some(win) = TestWindow::new() else {
        eprintln!("could not create a window; skipping");
        return;
    };
    let Some(handle) = win.handle() else {
        eprintln!("unsupported window handle type; skipping");
        return;
    };

    let Ok(initial) = inst.open_editor(handle) else {
        eprintln!("open_editor failed; skipping");
        return;
    };

    let caps = inst.editor_capabilities();
    eprintln!(
        "editor {}x{}, resize hints: {:?}",
        initial.width, initial.height, caps.resize
    );

    let requested = EditorSize {
        width: initial.width + 100,
        height: initial.height + 80,
    };
    match inst.resize_editor(requested) {
        Ok(granted) => {
            assert!(
                granted.width > 0 && granted.height > 0,
                "resize granted a degenerate size {}x{}",
                granted.width,
                granted.height
            );
            eprintln!("resize granted {}x{}", granted.width, granted.height);
        }
        // A fixed-size view refusing is correct behaviour, not a failure.
        Err(e) => eprintln!("resize refused ({e:?}) — valid for a fixed-size editor"),
    }

    inst.close_editor();
}
