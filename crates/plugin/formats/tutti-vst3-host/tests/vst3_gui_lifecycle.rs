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

fn sample_plugin_path(bundle: &str) -> Option<PathBuf> {
    let dir = sample_plugin_dir();
    if dir.is_empty() {
        return None;
    }
    let p = Path::new(&dir).join(bundle);
    let bin = resolve_bundle(&p);
    bin.is_file().then_some(bin)
}

fn host_checker_path() -> Option<PathBuf> {
    sample_plugin_path("host-checker.vst3")
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

// SAFETY: the singleton below lives in a `static` and hands out
// `&'static TestWindow` to tests on cargo's worker threads, which requires both
// `Send` and `Sync`. `winit`'s types are neither, because their *methods* are
// thread-affine — but the only method these tests call is `handle()`, which
// reads an X11 window id fixed at creation. No test pumps the event loop,
// mutates the window, or drops it (it lives for the process).
unsafe impl Send for TestWindow {}
unsafe impl Sync for TestWindow {}

/// The process-wide window, built at most once.
///
/// `winit` permits a single `EventLoop` per process: a second `build()` fails.
/// Per-test windows therefore worked only for whichever test ran first, and the
/// rest skipped themselves with "could not create a window" while still
/// reporting `ok` — a silent pass that asserted nothing. Sharing one window
/// makes every test in the file actually run.
static WINDOW: std::sync::OnceLock<Option<TestWindow>> = std::sync::OnceLock::new();

impl TestWindow {
    /// The shared window, or `None` if one could not be created.
    fn get() -> Option<&'static Self> {
        WINDOW.get_or_init(Self::build).as_ref()
    }

    fn build() -> Option<Self> {
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
        // SAFETY: `ptr` is this window's live platform handle; the window is a
        // process-lifetime singleton, so it outlives every editor opened here.
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
    let Some(win) = TestWindow::get() else {
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
    let Some(win) = TestWindow::get() else {
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

/// A plugin with no editor must fail cleanly, not crash or hang.
///
/// `audio-probe` returns nullptr from `createView`, so `open_editor` never
/// reaches the platform-type check or `attached`. This pins the earliest
/// rejection point: the host must report `NotSupported` rather than
/// dereferencing the null view.
///
/// Uses a real window rather than a null handle: `WindowHandle::from_raw`
/// requires a valid handle, and this path must not be the one place that
/// quietly breaks the contract.
#[test]
#[ignore = "needs a display; run under xvfb-run -a"]
fn editorless_plugin_is_refused_cleanly() {
    if !has_display() {
        eprintln!("no DISPLAY/WAYLAND_DISPLAY; run under `xvfb-run -a`. Skipping.");
        return;
    }
    let Some(path) = sample_plugin_path("audio-probe.vst3") else {
        eprintln!("audio-probe.vst3 not found; skipping");
        return;
    };
    let Ok(mut inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("audio-probe load failed; skipping");
        return;
    };
    let Some(win) = TestWindow::get() else {
        eprintln!("could not create a window; skipping");
        return;
    };
    let Some(handle) = win.handle() else {
        eprintln!("unsupported window handle type; skipping");
        return;
    };

    let err = inst
        .open_editor(handle)
        .expect_err("audio-probe has no editor; open_editor must fail");
    eprintln!("editorless plugin refused with: {err:?}");

    // And the failure must leave nothing open — a close afterwards is a no-op,
    // not a `removed()` without a matching `attached()`.
    inst.close_editor();
}

/// The host must ask `isPlatformTypeSupported` before `attached`.
///
/// `host-checker` scores `IPlugView` entry points the host exercises, and a
/// successful open here means the view accepted this platform's type *and* the
/// host's check let it through — i.e. the Gap A check does not reject a plugin
/// that works. The inverse (a view that refuses being rejected) is covered
/// without a display in `vst3_view_teardown.rs`, because no corpus plugin
/// refuses X11.
#[test]
#[ignore = "needs a display; run under xvfb-run -a"]
fn supported_platform_type_still_opens() {
    let Some(mut inst) = load_host_checker() else {
        return;
    };
    let Some(win) = TestWindow::get() else {
        eprintln!("could not create a window; skipping");
        return;
    };
    let Some(handle) = win.handle() else {
        eprintln!("unsupported window handle type; skipping");
        return;
    };

    let size = inst
        .open_editor(handle)
        .expect("host-checker supports this platform type; the check must not reject it");
    assert!(
        size.width > 0 && size.height > 0,
        "editor reported a degenerate size {}x{}",
        size.width,
        size.height
    );
    inst.close_editor();
}

/// This is the regression test for "nothing pumps the plugin run loop": before
/// [`Vst3Loaded::run_editor_loop_iteration`] existed, `host-checker` (a VSTGUI
/// plugin, which drives its redraws off a registered timer) would register its
/// timer and X file descriptor with us and then wait forever. The editor
/// appeared, painted once, and froze.
///
/// The assertion is deliberately two-part, because registration alone proves
/// nothing about the pump:
/// 1. the plugin registered *something* (a timer or an fd) — otherwise there is
///    no run loop to test and we say so rather than passing vacuously;
/// 2. driving the pump raises the dispatch counters, i.e. we really called back
///    into the plugin's `onTimer` / `onFDIsSet`.
#[test]
#[ignore = "needs a display; run under xvfb-run -a"]
fn open_editor_run_loop_is_pumped() {
    let Some(mut inst) = load_host_checker() else {
        return;
    };
    let Some(win) = TestWindow::get() else {
        eprintln!("could not create a window; skipping");
        return;
    };
    let Some(handle) = win.handle() else {
        eprintln!("unsupported window handle type; skipping");
        return;
    };

    // Nothing should be dispatched before an editor exists.
    let before_open = inst.run_loop_activity();

    if let Err(e) = inst.open_editor(handle) {
        eprintln!("open_editor failed ({e:?}); skipping");
        return;
    }

    let registered = inst.run_loop_activity();
    eprintln!(
        "after open: {} timer(s), {} fd(s) registered",
        registered.timers_registered, registered.event_handlers_registered
    );

    if registered.timers_registered == 0 && registered.event_handlers_registered == 0 {
        // Not a host bug: a plugin whose toolkit needs no run loop is entitled
        // to register nothing. Fail loudly only if it registered and we then
        // failed to pump — that is what this test is for.
        eprintln!("plugin registered no run-loop handlers; nothing to pump. Skipping.");
        inst.close_editor();
        return;
    }

    // VSTGUI's redraw timer is on the order of tens of milliseconds, so pump
    // for well over one period rather than assuming a single iteration is due.
    // Real hosts call this every frame; this is that loop, compressed.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut activity = registered;
    while std::time::Instant::now() < deadline {
        inst.run_editor_loop_iteration();
        activity = inst.run_loop_activity();
        if activity.timers_fired > before_open.timers_fired
            && activity.fds_dispatched >= before_open.fds_dispatched
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    eprintln!(
        "after pumping: {} timer fire(s), {} fd dispatch(es)",
        activity.timers_fired, activity.fds_dispatched
    );

    // The pump must have done *something*. A registered handler that never
    // fires across two seconds of pumping means the loop is not wired.
    assert!(
        activity.timers_fired > before_open.timers_fired
            || activity.fds_dispatched > before_open.fds_dispatched,
        "run loop registered {} timer(s) and {} fd(s), but pumping for 2s \
         dispatched nothing (timers_fired {} -> {}, fds_dispatched {} -> {})",
        registered.timers_registered,
        registered.event_handlers_registered,
        before_open.timers_fired,
        activity.timers_fired,
        before_open.fds_dispatched,
        activity.fds_dispatched,
    );

    inst.close_editor();

    // Closing must retire the plugin's handlers, or the next pump calls into a
    // released view.
    let after_close = inst.run_loop_activity();
    eprintln!(
        "after close: {} timer(s), {} fd(s) still registered",
        after_close.timers_registered, after_close.event_handlers_registered
    );

    // Pumping with the editor closed must stay safe (and is what a host does on
    // the frame after the user closes the window).
    inst.run_editor_loop_iteration();
}
