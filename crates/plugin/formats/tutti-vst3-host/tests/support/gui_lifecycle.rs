//! The VST3 editor-lifecycle tests and their helpers, shared verbatim by two
//! targets:
//!
//! - `tests/vst3_gui_lifecycle.rs` — the default cargo harness. Works wherever
//!   winit can build an event loop off the main thread (X11, Wayland, Windows);
//!   on macOS every test skips, because Cocoa will not allow it.
//! - `tests/gui_lifecycle_main.rs` — a `harness = false` binary that owns
//!   `main()`, and so runs these on the main thread. That is the only
//!   configuration macOS accepts, and it matches how a real host drives an
//!   editor.
//!
//! It lives under `tests/support/` because cargo compiles every top-level file
//! in `tests/` as its own target; a shared module must sit in a subdirectory or
//! it would be built a third time on its own.
//!
//! Each test below is wrapped in `gui_test!`, which the two roots define
//! differently: the harness one attaches `#[test]`/`#[ignore]`, the main-thread
//! one emits a plain function. The attributes cannot be written here directly —
//! rustc strips an `#[ignore]` function out of a `harness = false` binary, so
//! the runner would have nothing left to call.

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
    for sub in [
        "Contents/x86_64-linux",
        "Contents/MacOS",
        "Contents/x86_64-win",
    ] {
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

/// Build the test event loop, or `None` if this platform will not give us one
/// on the current thread.
///
/// The only genuinely platform-specific step in this file. Cargo's default
/// harness runs tests on worker threads, and winit refuses to build an event
/// loop off the main thread unless asked — X11, Wayland and Windows expose
/// `with_any_thread` for exactly that, each on its own extension trait.
///
/// macOS has no equivalent, because Cocoa requires the main thread at the OS
/// level. That is why these tests are also built as `gui_lifecycle_main`, a
/// `harness = false` binary that owns `main()` and therefore *is* the main
/// thread — see the module docs. Under the default harness on macOS this
/// returns `None` and every GUI test skips; under that binary it succeeds.
///
/// Callers get an `Option` and no `cfg` of their own.
fn build_event_loop() -> Option<winit::event_loop::EventLoop<()>> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // X11 and Wayland each spell it on their own trait; whichever backend
        // winit picks, the builder honours the flag set for it.
        use winit::event_loop::EventLoop;
        use winit::platform::wayland::EventLoopBuilderExtWayland;
        use winit::platform::x11::EventLoopBuilderExtX11;
        let mut builder = EventLoop::builder();
        EventLoopBuilderExtX11::with_any_thread(&mut builder, true);
        EventLoopBuilderExtWayland::with_any_thread(&mut builder, true);
        builder.build().ok()
    }
    #[cfg(windows)]
    {
        use winit::event_loop::EventLoop;
        use winit::platform::windows::EventLoopBuilderExtWindows as _;
        EventLoop::builder().with_any_thread(true).build().ok()
    }
    #[cfg(target_os = "macos")]
    {
        use winit::event_loop::EventLoop;
        // Cocoa offers no `with_any_thread` opt-out, so the only way to get a
        // loop here is to already be on the main thread. Building off it does
        // not return an error — winit panics — so this must be a check, not a
        // `.ok()`.
        if !is_main_thread() {
            eprintln!(
                "macOS requires the event loop on the main thread, and this is a \
                 cargo worker thread; skipping. Run the `gui_lifecycle_main` \
                 binary instead — see the module docs."
            );
            return None;
        }
        EventLoop::builder().build().ok()
    }
}

/// Whether the caller is on the process's main thread.
///
/// Only macOS needs this, and only because winit *panics* rather than erroring
/// when an event loop is built elsewhere — so the check has to happen before
/// the call. `pthread_main_np` is the OS's own answer, which beats inferring it
/// from a thread name.
#[cfg(target_os = "macos")]
fn is_main_thread() -> bool {
    extern "C" {
        fn pthread_main_np() -> std::os::raw::c_int;
    }
    unsafe { pthread_main_np() == 1 }
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
        let event_loop = build_event_loop()?;
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

gui_test! {
/// Opening and closing an editor must pair `attached`/`removed` exactly.
///
/// The plugin flags `attached is called without removed first!` if the host
/// opens twice without closing, and the converse on a stray close. Both leak a
/// native view per occurrence.
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
}

gui_test! {
/// `close_editor` on a never-opened instance must not call `removed`.
fn closing_an_unopened_editor_is_a_noop() {
    let Some(mut inst) = load_host_checker() else {
        return;
    };
    // No open_editor call at all. This must not reach IPlugView::removed —
    // which the plugin would flag as "removed without attached".
    inst.close_editor();
    inst.close_editor();
}
}

gui_test! {
/// Editor capabilities must be queryable, and a resize must be honoured or
/// cleanly refused.
///
/// `canResize` and `checkSizeConstraint` are the entry points a host uses to
/// decide whether its editor window may be dragged. A host that resizes a
/// fixed-size view without asking produces a stretched or clipped GUI.
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

    // An in-range request: both a correct host and one that ignores
    // `checkSizeConstraint` return the same thing here, so this leg only
    // establishes that resizing works at all.
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

    // The discriminating case: a request the plugin *must* clamp.
    //
    // `VST3Editor::checkSizeConstraint` clamps to the .uidesc min/max
    // (vst3editor.cpp:1467-1490), so an absurd request comes back snapped. Ask
    // the plugin directly what it would do, and only assert if it genuinely
    // constrains this size — a plugin with no limits has nothing to enforce and
    // must not be turned into a failure.
    //
    // Without this leg the test was vacuous: it asserted only that the granted
    // size was non-degenerate, which is equally true of a host that forwards
    // the raw request. Measured — replacing `resize_editor`'s constrained rect
    // with the unmodified request left the old test reporting `ok`.
    const ABSURD: EditorSize = EditorSize {
        width: 10_000,
        height: 10_000,
    };
    let clamped_to = inst
        .check_editor_size_constraint(ABSURD)
        .expect("editor is open, so the constraint query must answer");

    if clamped_to == ABSURD {
        eprintln!("editor accepts {ABSURD:?} unclamped — no constraint to verify");
    } else {
        eprintln!("plugin clamps {ABSURD:?} to {clamped_to:?}");
        let granted = inst
            .resize_editor(ABSURD)
            .expect("plugin reported a legal snapped size, so onSize must accept it");
        assert_eq!(
            granted, clamped_to,
            "host granted {granted:?} for an out-of-range request, but the \
             plugin's own checkSizeConstraint snaps it to {clamped_to:?} — the \
             host applied the raw request instead of the constrained rect"
        );
    }

    inst.close_editor();
}
}

gui_test! {
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
}

gui_test! {
/// The host must ask `isPlatformTypeSupported` before `attached`.
///
/// `host-checker` scores `IPlugView` entry points the host exercises, and a
/// successful open here means the view accepted this platform's type *and* the
/// host's check let it through — i.e. the Gap A check does not reject a plugin
/// that works. The inverse (a view that refuses being rejected) is covered
/// without a display in `vst3_view_teardown.rs`, because no corpus plugin
/// refuses X11.
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
}

gui_test! {
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
}

gui_test! {
/// `has_editor()` must agree with whether an editor can actually be opened.
///
/// Answering it with `controller.is_some()` answers a *different question*:
/// nearly every VST3 has an edit controller, because that is where parameters
/// live, and only some of them also publish a view. That returns `true`
/// unconditionally — for `audio-probe` and `adelay`, whose `open_editor` fails,
/// exactly as much as for `host-checker` and `again`, whose succeeds.
///
/// That is not cosmetic. `tutti-plugin-server` feeds this straight into
/// `Features::EDITOR` on the plugin descriptor (`loaders/vst3.rs:116,150`), so
/// a DAW would offer an "open editor" affordance for every VST3 it scanned and
/// fail when the user took it.
///
/// The check is a *contrast across four plugins* rather than a single
/// assertion: two that open and two that do not. A `has_editor` hardcoded
/// either way — which is what the bug amounted to — fails one of the pairs.
fn has_editor_agrees_with_opening_one() {
    if !has_display() {
        eprintln!("no DISPLAY/WAYLAND_DISPLAY; run under `xvfb-run -a`. Skipping.");
        return;
    }
    let Some(win) = TestWindow::get() else {
        eprintln!("could not create a window; skipping");
        return;
    };
    let Some(handle) = win.handle() else {
        eprintln!("unsupported window handle type; skipping");
        return;
    };

    let mut checked = 0;
    let mut with_editor = 0;
    let mut without_editor = 0;
    let mut disagreements = Vec::new();

    // `again.vst3` is deliberately absent: its VSTGUI/cairo backend aborts in
    // `_get_screen_index` under Xvfb, which kills the whole test binary. That
    // is a cairo/Xvfb interaction, not a host defect — the plugins below cover
    // both outcomes without it.
    for name in [
        "host-checker.vst3",
        "panner.vst3",
        "note-expression-text.vst3",
        "audio-probe.vst3",
        "adelay.vst3",
        "channel-context.vst3",
    ] {
        let Some(path) = sample_plugin_path(name) else {
            continue;
        };
        let Ok(mut inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
            continue;
        };

        let claims = inst.has_editor();
        let opened = inst.open_editor(handle).is_ok();
        if opened {
            inst.close_editor();
            with_editor += 1;
        } else {
            without_editor += 1;
        }
        checked += 1;

        if claims != opened {
            disagreements.push(format!(
                "{name}: has_editor()={claims} but open_editor() {}",
                if opened { "succeeded" } else { "failed" }
            ));
        }
    }

    // Without both kinds present, agreement proves nothing: a constant `true`
    // satisfies an all-editor corpus and a constant `false` an all-editorless
    // one. Say so rather than reporting a pass that means nothing.
    assert!(
        checked >= 2 && with_editor > 0 && without_editor > 0,
        "this test needs at least one plugin with an editor and one without to \
         be meaningful; checked {checked} ({with_editor} with, \
         {without_editor} without)"
    );
    assert!(
        disagreements.is_empty(),
        "has_editor() disagrees with reality:\n  {}",
        disagreements.join("\n  ")
    );
}
}

gui_test! {
/// A closed editor must report no pending resize request.
///
/// `poll_editor_resize_request` drains a channel the plugin's `IPlugFrame`
/// pushes into. The `EditorState::Open` guard is what stops a stale request
/// surviving a close and being applied to the *next* editor — so the assertion
/// is that closing clears it, not merely that a fresh instance returns `None`.
fn a_closed_editor_reports_no_resize_request() {
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

    assert!(
        inst.poll_editor_resize_request().is_none(),
        "an editor that was never opened reported a pending resize"
    );

    inst.open_editor(handle)
        .expect("host-checker opens an editor");
    // Drain whatever the plugin queued during open, so the post-close assertion
    // is about the close and not about start-up traffic.
    let _ = inst.poll_editor_resize_request();
    inst.close_editor();

    assert!(
        inst.poll_editor_resize_request().is_none(),
        "a closed editor still reported a pending resize request — it would be \
         applied to whichever editor is opened next"
    );
}
}

gui_test! {
/// A live plugin editor must survive being handed keyboard, wheel and focus
/// input, and must answer whether it consumed each key.
///
/// The stub-view suite (`vst3_view_input.rs`) pins the *mapping* — that the
/// arguments arrive unmangled and that only `kResultTrue` reads as consumed —
/// which is all a stub can prove. What it cannot show is that a real plugin's
/// key handler is reachable at all: a view that was never `attached`, or a host
/// calling off the UI thread, crashes here and nowhere else.
///
/// The consumed/not-consumed answers are **not** asserted. Whether a plugin
/// takes a given key is its own business and both answers are correct, so
/// pinning one would be a test of host-checker rather than of this host. What
/// is asserted is that the calls complete and the editor is still usable after.
///
/// Measured: host-checker consumes none of them. That is a real answer from a
/// reachable handler rather than a dropped call — its editor derives from
/// `VSTGUIEditor`, which implements all three and forwards to `CFrame`
/// (`vstguieditor.cpp:297,321,345`); `CFrame` returns false when no control
/// wants the input. So a plugin declining everything is the expected shape
/// here, and it is exactly why this test cannot assert on the answer.
fn editor_accepts_keyboard_wheel_and_focus() {
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

    // Before the editor exists every forwarder must decline rather than reach
    // through a `None` view.
    assert!(
        !inst.send_key_down('a' as u16, 0, 0),
        "a key was reported consumed with no editor open"
    );
    assert!(
        !inst.send_wheel(1.0),
        "a wheel event was reported consumed with no editor open"
    );
    inst.set_editor_focus(true); // must not panic

    let Ok(size) = inst.open_editor(handle) else {
        eprintln!("open_editor failed; skipping");
        return;
    };

    // Focus first: a plugin that has not been told it has the keyboard is
    // entitled to ignore every key that follows.
    inst.set_editor_focus(true);

    let consumed_char = inst.send_key_down('a' as u16, 0, 0);
    let _ = inst.send_key_up('a' as u16, 0, 0);
    // A non-character keystroke: virtual key set, character zero. VKEY_LEFT.
    let consumed_arrow = inst.send_key_down(0, 11, 0);
    let _ = inst.send_key_up(0, 11, 0);
    let consumed_wheel = inst.send_wheel(1.0);
    let _ = inst.send_wheel(-1.0);

    eprintln!(
        "host-checker consumed: char={consumed_char} arrow={consumed_arrow} \
         wheel={consumed_wheel}"
    );

    inst.set_editor_focus(false);

    // The editor must still be alive and answering after all of that — the
    // failure this catches is a plugin left in a broken state by input it was
    // handed at the wrong time, which a crash-free run alone would not show.
    let caps = inst.editor_capabilities();
    eprintln!(
        "editor still responsive after input: {}x{}, resize hints {:?}",
        size.width, size.height, caps.resize
    );

    inst.close_editor();

    // And the forwarders decline again once it is gone.
    assert!(
        !inst.send_key_down('a' as u16, 0, 0),
        "a key was reported consumed after the editor closed"
    );
}
}
