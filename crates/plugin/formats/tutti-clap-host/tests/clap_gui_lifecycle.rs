//! Host-conformance harness for the **editor lifecycle** — the
//! `clap_plugin_gui` half of `src/instance/polling.rs`, driven by a real plugin
//! across the real CLAP FFI.
//!
//! The bug this exists to prevent: `has_editor()` was `!gui.is_null()` — "is
//! there a gui vtable?" rather than "can an editor actually be embedded?".
//! Those differ for two legal plugin shapes, a floating-only plugin and one
//! whose `create` is absent, both of which have a non-null pointer. The answer
//! feeds `Features::EDITOR`, so the DAW rendered an "open editor" button that
//! could not open one.
//!
//! Nothing here opens a window: the probe's `clap.gui` is pure bookkeeping and
//! never dereferences the parent handle, so this suite runs headless on every
//! platform with no `#[cfg(target_os)]` gate.
//!
//! The plugin's GUI mode, capture and command word are process-globals, so
//! every test holds [`PROBE_LOCK`] for its entire scenario. The mode is read by
//! the host once at load, so it must be set before `load`.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

mod support;
use support::probe_path::probe_path;

use tutti_clap_host::{ClapLoaded, EditorSize, WindowHandle};
use tutti_clap_test_plugin::{
    GuiCapture, GuiMode, GUI_ASPECT_H, GUI_ASPECT_W, GUI_CALL_ADJUST_SIZE, GUI_CALL_CAN_RESIZE,
    GUI_CALL_CREATE, GUI_CALL_DESTROY, GUI_CALL_GET_RESIZE_HINTS, GUI_CALL_GET_SIZE, GUI_CALL_HIDE,
    GUI_CALL_IS_API_SUPPORTED, GUI_CALL_SET_PARENT, GUI_CALL_SET_SCALE, GUI_CALL_SET_SIZE,
    GUI_CALL_SET_TRANSIENT, GUI_CALL_SHOW, GUI_CALL_SUGGEST_TITLE, GUI_CMD_CLOSED_AND_DESTROYED,
    GUI_CMD_CLOSED_AND_DESTROYED_FROM_SHOW, GUI_CMD_CLOSED_NOT_DESTROYED, GUI_CMD_REQUEST_RESIZE,
    GUI_HEIGHT, GUI_REQUESTED_RESIZE_H, GUI_REQUESTED_RESIZE_W, GUI_SIZE_QUANTUM, GUI_WIDTH,
};

/// Serializes whole scenarios — set mode → reset → load → drive → read — so one
/// test cannot observe another's calls or run against another's selected
/// [`GuiMode`].
static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// A stand-in parent window handle, never dereferenced. Non-null rather than
/// null so it proves the host forwarded *our* handle instead of a zero.
const FAKE_PARENT: usize = 0xDEAD_BEEF;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A held [`PROBE_LOCK`], a selected [`GuiMode`], and a freshly-reset probe.
struct Probe {
    _lock: MutexGuard<'static, ()>,
}

impl Probe {
    /// Take the lock, select the GUI shape, and clear the recorded calls.
    ///
    /// The mode is set **before** any `load`: the host caches the `clap.gui`
    /// extension pointer once during load, and `Absent`/`NoCreate` are
    /// different vtables (or none), so they cannot be switched afterwards.
    fn acquire(mode: GuiMode) -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_gui_mode(mode as u32);
        gui_reset();
        Probe { _lock: lock }
    }

    /// Load the reference plugin through the real host.
    ///
    /// Deliberately **not** activated: every method under test lives on
    /// `ClapLoaded`, so activating would add an unrelated audio setup whose
    /// failure would be reported as a GUI failure.
    fn load(&self) -> ClapLoaded {
        let path = Path::new(probe_path());
        // Bare dylib: pass it as both bundle and library so the host dlopens it
        // directly, no `.clap` bundle structure needed.
        ClapLoaded::load_with_library(path, Some(path), 48_000.0, 512)
            .expect("reference plugin should load")
    }

    fn capture(&self) -> GuiCapture {
        read_gui_capture()
    }

    fn command(&self, cmd: u32) {
        set_gui_command(cmd);
    }

    /// Fire the latched command immediately, from this thread, rather than
    /// waiting for the plugin's next host-driven callback.
    fn run_command(&self) {
        assert!(
            run_gui_command(),
            "the probe must have a live host to call back into — did the editor \
             get created?"
        );
    }
}

impl Drop for Probe {
    /// Return the probe to the no-GUI shape the other suites were written
    /// against — the mode is a process-global that outlives the test, and a
    /// leaked `Embeddable` would give them a `clap.gui` they never asked for.
    fn drop(&mut self) {
        set_gui_mode(GuiMode::Absent as u32);
    }
}

/// A parent-window handle for `open_editor`.
///
/// # Safety
/// The probe never dereferences it; see [`FAKE_PARENT`].
fn fake_parent() -> WindowHandle {
    unsafe { WindowHandle::from_raw(FAKE_PARENT as *mut std::ffi::c_void) }
}

// --- the exported C symbols, reached across the dlopen seam -----------------
//
// Opening the same path a second time shares the already-loaded image, so these
// see (and drive) exactly the globals the host's calls touched.

/// The probe image, opened once and kept mapped for the life of the test binary.
///
/// Not dropped per call, unlike the sibling suites: this one sets the
/// [`GuiMode`] *before* the host loads, so its handle is the only one. Dropping
/// it `dlclose`s the image, and the host's subsequent load maps a fresh copy
/// with `GUI_MODE` back at its `Absent` initializer.
fn probe_lib() -> &'static libloading::Library {
    static LIB: std::sync::OnceLock<libloading::Library> = std::sync::OnceLock::new();
    LIB.get_or_init(|| unsafe {
        libloading::Library::new(probe_path()).expect("re-open reference plugin")
    })
}

fn read_gui_capture() -> GuiCapture {
    type F = unsafe extern "C" fn(*mut GuiCapture) -> bool;
    let mut cap = GuiCapture::default();
    unsafe {
        let f: libloading::Symbol<F> = probe_lib()
            .get(b"tutti_test_plugin_gui_capture\0")
            .expect("gui capture symbol present");
        assert!(f(&mut cap), "gui capture must succeed");
    }
    cap
}

fn set_gui_mode(mode: u32) {
    type F = unsafe extern "C" fn(u32);
    unsafe {
        let f: libloading::Symbol<F> = probe_lib()
            .get(b"tutti_test_plugin_set_gui_mode\0")
            .expect("gui mode symbol present");
        f(mode);
    }
}

fn set_gui_command(cmd: u32) {
    type F = unsafe extern "C" fn(u32) -> u32;
    unsafe {
        let f: libloading::Symbol<F> = probe_lib()
            .get(b"tutti_test_plugin_gui_command\0")
            .expect("gui command symbol present");
        f(cmd);
    }
}

fn run_gui_command() -> bool {
    type F = unsafe extern "C" fn() -> bool;
    unsafe {
        let f: libloading::Symbol<F> = probe_lib()
            .get(b"tutti_test_plugin_gui_run_command\0")
            .expect("gui run-command symbol present");
        f()
    }
}

fn gui_reset() {
    type F = unsafe extern "C" fn();
    unsafe {
        let f: libloading::Symbol<F> = probe_lib()
            .get(b"tutti_test_plugin_gui_reset\0")
            .expect("gui reset symbol present");
        f();
    }
}

/// The `GUI_CALL_*` ids the probe recorded, in call order.
fn calls(cap: &GuiCapture) -> Vec<u32> {
    let n = cap.call_count as usize;
    assert!(
        n <= cap.calls.len(),
        "probe recorded {n} calls, more than it can store — the assertion below \
         would be reading a truncated log"
    );
    cap.calls[..n].to_vec()
}

// ===========================================================================
// has_editor
// ===========================================================================

/// The baseline: a plugin that really can embed an editor is reported as having
/// one. Here so the three negative cases below cannot be satisfied by a
/// `has_editor` that simply returns `false`.
#[test]
fn has_editor_true_for_embeddable_plugin() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let loaded = probe.load();

    assert!(
        loaded.has_editor(),
        "a plugin whose gui api is supported for embedding and whose `create` \
         exists has an editor"
    );
}

/// A plugin with no `clap.gui` at all. The pointer check gets this right, so
/// this is the case the fix must not regress.
#[test]
fn has_editor_false_when_extension_absent() {
    let probe = Probe::acquire(GuiMode::Absent);
    let loaded = probe.load();

    assert!(
        !loaded.has_editor(),
        "no `clap.gui` extension means no editor"
    );
}

/// **The floating-only case.** `is_api_supported(api, is_floating=false)` is
/// false, so there is no embedded editor to open — but the vtable pointer is
/// non-null, so the old implementation said `true`.
///
/// The `open_editor` assertion is what makes the first one meaningful: the two
/// must not disagree.
#[test]
fn has_editor_false_for_floating_only_plugin() {
    let probe = Probe::acquire(GuiMode::FloatingOnly);
    let mut loaded = probe.load();

    assert!(
        !loaded.has_editor(),
        "a floating-only plugin has no *embeddable* editor: its gui vtable is \
         non-null, which is exactly why the old `!gui.is_null()` check reported \
         an editor the host could never open"
    );

    assert!(
        loaded.open_editor(fake_parent()).is_err(),
        "and open_editor confirms it — the two must not disagree"
    );

    // The other half of the distinction: "cannot embed" is not "has no editor".
    // Without this the test above is satisfied by a host that reports every
    // floating-only plugin as having no UI at all, which is the bug C-12 names.
    assert!(
        loaded.has_floating_editor(),
        "the same plugin *does* have a floating editor — a host that only ever \
         asks the embedded question reports it as editor-less"
    );
}

/// **The absent-`create` case.** Every `clap_plugin_gui` member is an
/// `Option<fn>`; a plugin may omit `create`. The host already modelled this in
/// `EmbedOutcome::did_create`, but `has_editor` did not consult it.
#[test]
fn has_editor_false_when_create_is_absent() {
    let probe = Probe::acquire(GuiMode::NoCreate);
    let loaded = probe.load();

    assert!(
        !loaded.has_editor(),
        "a gui vtable whose `create` is None cannot allocate an editor, however \
         non-null the pointer is"
    );
}

/// `has_editor` must be **cheap and side-effect-free** — it is called once per
/// plugin during a scan, and the DAW scans everything installed.
///
/// CLAP documents `is_api_supported` as a pure predicate, while `create`
/// "allocates all resources necessary for the gui". A `has_editor` implemented
/// by create-then-destroy would spin up OpenGL contexts and worker threads on
/// every scanned plugin, so the absence of `create` in the call log is a
/// property worth pinning.
#[test]
fn has_editor_does_not_create_a_gui() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let loaded = probe.load();

    assert!(loaded.has_editor());

    let cap = probe.capture();
    assert!(
        !cap.created,
        "has_editor must not allocate the plugin's gui — `create` ran"
    );
    assert!(
        !cap.destroyed,
        "and therefore must not destroy one either — `destroy` ran"
    );
    assert_eq!(
        calls(&cap),
        vec![GUI_CALL_IS_API_SUPPORTED],
        "the cheap predicate is the only call a capability query needs"
    );
    assert_eq!(
        cap.is_api_supported_embedded_queries, 1,
        "and it must ask the *embedded* question — asking only the floating one \
         would report a floating-only plugin as embeddable"
    );
    assert_eq!(
        cap.is_api_supported_floating_queries, 0,
        "the host embeds; it has no reason to ask about floating windows"
    );
}

/// The platform API constant must be the *current platform's*, not a hardcoded
/// `"x11"`.
///
/// The probe answers `is_api_supported` false for any api string but its own
/// platform's, so a host that hardcoded X11 reports "no editor" on macOS and
/// Windows — a total loss of plugin GUIs that a Linux-only CI never sees.
#[test]
fn has_editor_asks_about_the_current_platform_api() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let loaded = probe.load();

    assert!(
        loaded.has_editor(),
        "the probe returns false for any api string but its own platform's, so \
         a host asking about the wrong platform lands here"
    );
}

// ===========================================================================
// open_editor — the embed sequence, against a real plugin.
// ===========================================================================

/// Whether the window api this platform embeds with denominates geometry in
/// logical pixels, and therefore must not be sent `set_scale`
/// (`ext/gui.h:56-60`). macOS embeds with `cocoa`; Windows uses `win32` and
/// Linux `x11`, both of which are physical-pixel.
///
/// The host decides this from the api *string*, not from `cfg!`. Mirroring it
/// with a `cfg!` here rather than importing the host's answer is deliberate:
/// a test that asked the host what it does could not disagree with the host.
const PLATFORM_USES_LOGICAL_PIXELS: bool = cfg!(target_os = "macos");

/// The `GUI_CALL_*` ids `open_editor` must produce on this platform.
fn expected_embed_sequence() -> Vec<u32> {
    let mut expected = vec![GUI_CALL_IS_API_SUPPORTED, GUI_CALL_CREATE];
    if !PLATFORM_USES_LOGICAL_PIXELS {
        expected.push(GUI_CALL_SET_SCALE);
    }
    expected.extend([GUI_CALL_GET_SIZE, GUI_CALL_SET_PARENT, GUI_CALL_SHOW]);
    expected
}

/// The spec's embed order, observed from inside a real plugin.
///
/// `polling.rs`'s in-crate test asserts this order against a hand-written
/// vtable; what it cannot show is that the host reaches that code with a
/// correctly-built `clap_window`, which the `set_parent` assertions add.
#[test]
fn open_editor_runs_the_spec_embed_sequence() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let mut loaded = probe.load();

    let size = loaded
        .open_editor(fake_parent())
        .expect("embeddable plugin opens");

    assert_eq!(
        size,
        EditorSize {
            width: GUI_WIDTH,
            height: GUI_HEIGHT
        },
        "the host must report the plugin's own `get_size`, not its 800x600 \
         fallback — the probe deliberately reports neither 800 nor 600"
    );

    let cap = probe.capture();
    assert_eq!(
        calls(&cap),
        expected_embed_sequence(),
        "CLAP order: is_api_supported gates create, set_parent follows get_size \
         and precedes show. `set_scale` appears only on a physical-pixel api, \
         where it must precede get_size so the reported size carries the factor"
    );
    assert!(cap.created, "create ran");
    assert_eq!(cap.create_balance, 1, "exactly one live editor");
    assert!(
        cap.set_parent_window_non_null,
        "the host must hand the plugin a real `clap_window`"
    );
    assert!(
        cap.set_parent_api_matches_platform,
        "…whose `api` names the current platform — a hardcoded \"x11\" fails \
         here on macOS and Windows"
    );
}

/// **C-4.** `ext/gui.h:56-57` on `cocoa`, and `:59-60` on `uikit`: "uses
/// logical size, don't call clap_plugin_gui->set_scale()". `set_scale` itself
/// repeats it at `:141`. A logical-pixel api has already folded the display's
/// backing-scale factor into every coordinate, so a host that sets it too
/// applies the factor twice and a Retina editor opens at 2×.
///
/// The host passes a hardcoded 1.0 today, which is why this never showed as a
/// visible bug — 1.0 is the identity. That makes the *call* the thing to
/// assert, not the size it produced: pinning the size would pass against the
/// bug and only start failing once real DPI is wired, which is the moment this
/// test exists to protect.
///
/// Read `last_scale` alongside the call log, because they answer different
/// questions: `last_scale` is 0.0 both when the host correctly skipped the call
/// and when the probe was never asked anything at all. The `created` assertion
/// rules the second out.
#[test]
fn open_editor_calls_set_scale_only_on_a_physical_pixel_api() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let mut loaded = probe.load();

    loaded
        .open_editor(fake_parent())
        .expect("embeddable plugin opens");

    let cap = probe.capture();
    assert!(
        cap.created,
        "the embed sequence must have run at all, or every assertion below is \
         vacuous"
    );

    let called = calls(&cap).contains(&GUI_CALL_SET_SCALE);
    if PLATFORM_USES_LOGICAL_PIXELS {
        assert!(
            !called,
            "this platform embeds with a logical-pixel api, which the spec says \
             must not be sent set_scale — the factor is already applied"
        );
        assert_eq!(
            cap.last_scale, 0.0,
            "and no scale reached the plugin (0.0 is the probe's never-called \
             sentinel)"
        );
    } else {
        assert!(
            called,
            "this platform embeds with a physical-pixel api, which needs the \
             host's scale — skipping it leaves the editor at 1× on a HiDPI \
             display"
        );
        assert_eq!(
            cap.last_scale, 1.0,
            "the host passes a scale; 1.0 is today's placeholder until the \
             frontend carries a real backing-scale factor"
        );
    }
}

/// A floating-only plugin is refused at the first gate, before `create`.
///
/// `create` on a plugin that just said it cannot do embedded is undefined
/// territory, and the plugins that handle it least well would crash the host.
#[test]
fn open_editor_refuses_floating_only_before_create() {
    let probe = Probe::acquire(GuiMode::FloatingOnly);
    let mut loaded = probe.load();

    let err = loaded
        .open_editor(fake_parent())
        .expect_err("floating-only plugin cannot be embedded");
    let msg = format!("{err}");
    assert!(
        msg.contains("not supported"),
        "the error should name the unsupported embedded api, got: {msg}"
    );

    let cap = probe.capture();
    assert_eq!(
        calls(&cap),
        vec![GUI_CALL_IS_API_SUPPORTED],
        "the sequence must stop at the gate — nothing is created"
    );
    assert!(!cap.created, "no gui resources were allocated");
}

/// Opening twice must not leak an editor: the host closes the first before the
/// second, leaving exactly one live.
///
/// Asserted through the probe's create/destroy balance rather than a flag,
/// because "created twice, destroyed once" and "created once" look identical to
/// any boolean.
#[test]
fn close_editor_destroys_exactly_once() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let mut loaded = probe.load();

    loaded.open_editor(fake_parent()).expect("opens");
    assert_eq!(probe.capture().create_balance, 1, "one live editor");

    loaded.close_editor();

    let cap = probe.capture();
    assert!(cap.destroyed, "destroy ran");
    assert_eq!(
        cap.create_balance, 0,
        "created once, destroyed once — a leak reads positive, a \
         double-destroy negative"
    );
    assert_eq!(
        calls(&cap).iter().filter(|c| **c == GUI_CALL_HIDE).count(),
        1,
        "CLAP wants hide before destroy"
    );

    // Idempotent: a second close is a no-op, not a second destroy. This is the
    // property that makes the `Drop` impl safe after an explicit close.
    loaded.close_editor();
    assert_eq!(
        probe.capture().create_balance,
        0,
        "close_editor is idempotent — a second call must not double-destroy"
    );
}

/// When the plugin's window went away and it told the host so via
/// `gui.closed(was_destroyed = true)`, the host **must** still call `destroy`.
///
/// `ext/gui.h:241-242`: *"If was_destroyed is true, then the host must call
/// clap_plugin_gui->destroy() to acknowledge the gui destruction."* The spec's
/// own lifecycle (`gui.h:20-34`) pairs `destroy()` (step 14) with `create()`
/// (step 2), so it releases the gui resources `create` allocated — not the
/// window that just closed. The host read `was_destroyed` as "the plugin
/// already ran destroy for you" and skipped it, leaking those resources for the
/// instance's lifetime.
///
/// `hide` is the one call that *is* skipped: it acts on a window, and there is
/// no longer one.
#[test]
fn close_editor_destroys_after_plugin_window_was_destroyed() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let mut loaded = probe.load();

    loaded.open_editor(fake_parent()).expect("opens");

    // Driven out-of-band — *between* the host's calls. The during-`show` case
    // is its own test below.
    probe.command(GUI_CMD_CLOSED_AND_DESTROYED);
    probe.run_command();

    assert!(
        loaded.poll_gui_closed(),
        "the host must record the plugin's `gui.closed` callback"
    );
    let after_open = probe.capture();
    assert!(
        after_open.window_destroyed,
        "the probe must have reported was_destroyed = true — without it the \
         assertions below would be testing the ordinary close path"
    );
    assert_eq!(
        after_open.create_balance, 1,
        "the window is gone but the gui object `create` allocated is not; the \
         host still owes it a destroy"
    );

    loaded.close_editor();

    let cap = probe.capture();
    assert!(
        calls(&cap).contains(&GUI_CALL_DESTROY),
        "the spec requires the host acknowledge the destruction with \
         `gui.destroy`; skipping it leaks the plugin's gui resources"
    );
    assert_eq!(
        cap.create_balance, 0,
        "…and exactly once — a leak reads 1 here, a double-destroy -1"
    );
    assert!(
        !calls(&cap).contains(&GUI_CALL_HIDE),
        "`hide` acts on a window, and the plugin just reported it has none"
    );
}

/// The same, but the plugin reports the destruction from **inside `show`** —
/// while the host is still within `open_editor`.
///
/// The host used to clear its window-destroyed latch *after*
/// `embed_editor_sequence` returned, so this callback was wiped by the very
/// call that carried it. Only a callback raised inside the sequence
/// distinguishes clearing before it from clearing after; the out-of-band test
/// above lands after the clear either way.
#[test]
fn close_editor_destroys_after_window_was_destroyed_during_show() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let mut loaded = probe.load();

    // Latched, not run by hand: the probe fires it from `show`, the last call
    // of the host's embed sequence.
    probe.command(GUI_CMD_CLOSED_AND_DESTROYED_FROM_SHOW);
    loaded.open_editor(fake_parent()).expect("opens");

    assert!(
        loaded.poll_gui_closed(),
        "the host must still hold the `gui.closed` the plugin raised during the \
         embed — clearing the latch afterwards would have erased it"
    );

    let after_open = probe.capture();
    assert!(
        after_open.window_destroyed,
        "the probe must have reported was_destroyed = true from inside `show`"
    );
    assert_eq!(
        after_open.create_balance, 1,
        "the gui object is still allocated — only the window went away"
    );

    loaded.close_editor();

    let cap = probe.capture();
    assert!(
        calls(&cap).contains(&GUI_CALL_DESTROY),
        "a destruction reported mid-embed carries the same obligation as one \
         reported between calls"
    );
    assert_eq!(cap.create_balance, 0, "…and exactly once");
    assert!(
        !calls(&cap).contains(&GUI_CALL_HIDE),
        "`hide` is still skipped — the latch survived the embed sequence"
    );
}

/// `gui.closed(was_destroyed = false)` is the *other* half: the plugin's window
/// is still there and it merely lost the connection to its gui, so the host owes
/// it the full `hide` **and** `destroy`.
///
/// The `hide` assertion is what keeps the two halves apart: `destroy` alone is
/// now common to both, so a host that skipped the `was_destroyed` branch
/// entirely would satisfy every other assertion here.
#[test]
fn close_editor_hides_and_destroys_when_window_was_not_destroyed() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let mut loaded = probe.load();

    probe.command(GUI_CMD_CLOSED_NOT_DESTROYED);
    loaded.open_editor(fake_parent()).expect("opens");

    assert!(loaded.poll_gui_closed(), "the host records the callback");
    let after_open = probe.capture();
    assert!(
        !after_open.window_destroyed,
        "this path must report was_destroyed = false"
    );
    assert_eq!(
        after_open.create_balance, 1,
        "the editor is still allocated — the plugin only reported its window closed"
    );

    loaded.close_editor();

    let cap = probe.capture();
    assert!(
        calls(&cap).contains(&GUI_CALL_HIDE),
        "the window is still up, so CLAP wants it hidden before destroy"
    );
    assert!(
        calls(&cap).contains(&GUI_CALL_DESTROY),
        "and the host must destroy the gui resources it asked `create` for"
    );
    assert_eq!(cap.create_balance, 0, "…exactly once");
}

// ===========================================================================
// editor_capabilities
// ===========================================================================

/// Capabilities read off a created editor reach the plugin and come back
/// refined by its resize hints.
#[test]
fn editor_capabilities_reports_plugin_hints() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let mut loaded = probe.load();
    loaded.open_editor(fake_parent()).expect("opens");
    gui_reset();

    let caps = loaded.editor_capabilities();

    assert!(caps.resize.resizable, "the probe's can_resize is true");
    assert!(caps.resize.can_resize_horizontally);
    assert!(caps.resize.can_resize_vertically);
    assert!(caps.aspect.preserve, "the probe preserves aspect ratio");
    assert_eq!(
        caps.aspect.ratio,
        Some((GUI_ASPECT_W, GUI_ASPECT_H)),
        "the ratio must arrive unreduced and unswapped"
    );

    assert_eq!(
        calls(&probe.capture()),
        vec![GUI_CALL_CAN_RESIZE, GUI_CALL_GET_RESIZE_HINTS],
        "hints refine can_resize rather than replacing it"
    );
}

/// Before `create`, the query must not touch the plugin at all.
///
/// CLAP orders every `clap_plugin_gui` call after `create()`, and plugins
/// enforce it with a `HOST-MISBEHAVING` diagnostic. The host guarded only on
/// the extension pointer, so it violated that on every pre-create query.
#[test]
fn editor_capabilities_touches_nothing_before_create() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let loaded = probe.load();

    let caps = loaded.editor_capabilities();

    assert!(
        !caps.resize.resizable,
        "defaults are the honest answer before create: nothing has been asked, \
         so nothing is claimed"
    );
    assert!(
        calls(&probe.capture()).is_empty(),
        "and the plugin must not have been called — that is the HOST-MISBEHAVING \
         diagnostic this gate exists to stop"
    );
}

// ===========================================================================
// resize_editor — and the adjust_size contract.
// ===========================================================================

/// The host must forward the size the plugin **snapped to**, not the size the
/// user dragged to.
///
/// The probe snaps down to a 10px grid and then refuses any `set_size` that is
/// off-grid, so a host that skipped `adjust_size` or discarded its out-params
/// fails on the plugin's own refusal rather than on a cosmetic mismatch.
#[test]
fn resize_editor_forwards_the_adjusted_size() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let mut loaded = probe.load();
    loaded.open_editor(fake_parent()).expect("opens");
    gui_reset();

    // Deliberately off-grid in both axes.
    let requested = EditorSize {
        width: GUI_SIZE_QUANTUM * 33 + 7,
        height: GUI_SIZE_QUANTUM * 21 + 3,
    };
    let applied = loaded.resize_editor(requested).expect("resize succeeds");

    assert_eq!(
        applied,
        EditorSize {
            width: requested.width / GUI_SIZE_QUANTUM * GUI_SIZE_QUANTUM,
            height: requested.height / GUI_SIZE_QUANTUM * GUI_SIZE_QUANTUM,
        },
        "the returned size is the plugin's snap, not the raw request"
    );

    let cap = probe.capture();
    assert_eq!(
        calls(&cap),
        vec![GUI_CALL_ADJUST_SIZE, GUI_CALL_SET_SIZE],
        "adjust_size must precede set_size — that is the only order in which \
         the snap can be applied"
    );
    assert_eq!(
        (cap.last_adjust_in_w, cap.last_adjust_in_h),
        (requested.width, requested.height),
        "the raw request reaches adjust_size unmodified"
    );
    assert_eq!(
        (cap.last_set_size_w, cap.last_set_size_h),
        (applied.width, applied.height),
        "and the *adjusted* size is what reaches set_size"
    );
}

/// `adjust_size` returning false means the plugin could not compute a usable
/// size ("Returns true if the plugin could adjust the given size"). The host
/// read that as "no snap to apply" and forwarded the *unadjusted* request to
/// `set_size` — pushing the raw size through in the one case where the plugin
/// said it cannot give a working size, with out-params never promised to have
/// been written.
///
/// The probe's fixed-size mode accepts only its own dimensions, so the old
/// behaviour is observable: it reached `set_size`, was refused there, and
/// produced an error naming the wrong call.
#[test]
fn resize_editor_fails_when_plugin_cannot_adjust() {
    let probe = Probe::acquire(GuiMode::FixedSize);
    let mut loaded = probe.load();
    loaded.open_editor(fake_parent()).expect("opens");
    gui_reset();

    let err = loaded
        .resize_editor(EditorSize {
            width: 1234,
            height: 567,
        })
        .expect_err("a fixed-size editor cannot honour an arbitrary size");

    let msg = format!("{err}");
    assert!(
        msg.contains("adjust_size"),
        "the error must name `adjust_size` — blaming `set_size` points the \
         reader at the wrong call, got: {msg}"
    );

    let cap = probe.capture();
    assert_eq!(
        calls(&cap),
        vec![GUI_CALL_ADJUST_SIZE],
        "the host must stop at the refusal: forwarding an unadjusted size to \
         set_size is exactly the bug"
    );
    assert_eq!(
        (cap.last_set_size_w, cap.last_set_size_h),
        (0, 0),
        "set_size must not have been reached at all"
    );
}

/// A fixed-size editor reports itself as non-resizable, and its absent hints do
/// not get mistaken for permissive ones — a host that resizes a fixed-size
/// editor corrupts its layout, so an unanswered `get_resize_hints` must not
/// read as "resizable in both axes".
#[test]
fn editor_capabilities_reports_fixed_size_as_not_resizable() {
    let probe = Probe::acquire(GuiMode::FixedSize);
    let mut loaded = probe.load();
    loaded.open_editor(fake_parent()).expect("opens");

    let caps = loaded.editor_capabilities();

    assert!(!caps.resize.resizable, "the probe's can_resize is false");
    assert!(!caps.resize.can_resize_horizontally);
    assert!(!caps.resize.can_resize_vertically);
    assert!(
        !caps.aspect.preserve,
        "a `get_resize_hints` that returned false leaves the out-param unread"
    );
    assert!(caps.aspect.ratio.is_none());
}

// ===========================================================================
// poll_editor_resize_request
// ===========================================================================

/// A plugin-initiated `request_resize` reaches the host and is delivered once.
/// A poll that kept returning the last request would make the host resize the
/// editor on every UI frame.
#[test]
fn poll_editor_resize_request_delivers_once() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let mut loaded = probe.load();

    assert!(
        loaded.poll_editor_resize_request().is_none(),
        "nothing pending before the plugin asks"
    );

    // The probe calls host.gui.request_resize from inside `show`, i.e. while
    // the host still considers the editor live.
    probe.command(GUI_CMD_REQUEST_RESIZE);
    loaded.open_editor(fake_parent()).expect("opens");

    assert_eq!(
        loaded.poll_editor_resize_request(),
        Some(EditorSize {
            width: GUI_REQUESTED_RESIZE_W,
            height: GUI_REQUESTED_RESIZE_H,
        }),
        "the host must deliver the plugin's requested size — the probe asks for \
         a size unrelated to its own get_size, so echoing the initial size fails"
    );

    assert!(
        loaded.poll_editor_resize_request().is_none(),
        "and consume it: a re-delivered request would resize on every frame"
    );
}

// ===========================================================================
// Drop
// ===========================================================================

/// Dropping a `ClapLoaded` with a live editor destroys it, in that order.
///
/// CLAP requires `gui.destroy()` before `plugin.destroy()`; the reverse hands
/// the plugin's GUI code a destroyed instance. Nothing else in the suite covers
/// the path where the host never gets an explicit close.
#[test]
fn drop_destroys_a_live_editor() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    {
        let mut loaded = probe.load();
        loaded.open_editor(fake_parent()).expect("opens");
        assert_eq!(probe.capture().create_balance, 1, "one live editor");
    }

    let cap = probe.capture();
    assert!(
        cap.destroyed,
        "dropping the instance must destroy the editor — CLAP requires \
         gui.destroy() before plugin.destroy()"
    );
    assert_eq!(
        cap.create_balance, 0,
        "and leave nothing allocated behind it"
    );
}

// ---------------------------------------------------------------------------
// Floating windows — the plugin owns the window, the host only hints
// ---------------------------------------------------------------------------

/// The title the host suggests in these scenarios.
const TEST_TITLE: &std::ffi::CStr = c"Tutti Test Editor";

/// Read the probe's captured title back as a `&str`, up to its NUL.
fn suggested_title(cap: &GuiCapture) -> String {
    let end = cap
        .suggested_title
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(cap.suggested_title.len());
    String::from_utf8_lossy(&cap.suggested_title[..end]).into_owned()
}

/// A floating-only plugin opens, in the order `ext/gui.h:20-27` gives.
///
/// The whole point of C-12: before this, the only path into a CLAP editor was
/// the embed sequence, which such a plugin refuses at its first gate. It could
/// therefore never show a UI, and `Features::EDITOR` said it had none.
#[test]
fn open_floating_editor_runs_the_spec_sequence() {
    let probe = Probe::acquire(GuiMode::FloatingOnly);
    let mut loaded = probe.load();

    loaded
        .open_floating_editor(Some(fake_parent()), TEST_TITLE)
        .expect("a floating-only plugin must open in floating mode");

    let cap = probe.capture();
    assert_eq!(
        calls(&cap),
        vec![
            GUI_CALL_IS_API_SUPPORTED,
            GUI_CALL_CREATE,
            GUI_CALL_SET_TRANSIENT,
            GUI_CALL_SUGGEST_TITLE,
            GUI_CALL_SHOW,
        ],
        "the floating sequence is is_api_supported → create → set_transient → \
         suggest_title → show"
    );
    assert!(
        cap.created_floating,
        "`create` must be called with is_floating = true — an embedded create \
         on a floating-only plugin is the call it already refuses"
    );
}

/// The floating path asks the *floating* question, and only that one.
///
/// The counterpart to `open_editor_refuses_floating_only_before_create`, which
/// pins that the embed path asks only the embedded question. Keeping both means
/// neither path can quietly start asking the other's.
#[test]
fn the_floating_path_asks_only_the_floating_question() {
    let probe = Probe::acquire(GuiMode::FloatingOnly);
    let mut loaded = probe.load();

    loaded
        .open_floating_editor(Some(fake_parent()), TEST_TITLE)
        .expect("floating open should succeed");

    let cap = probe.capture();
    assert_eq!(
        cap.is_api_supported_floating_queries, 1,
        "the floating path must ask the floating question"
    );
    assert_eq!(
        cap.is_api_supported_embedded_queries, 0,
        "and must not ask the embedded one — a host that asks both and requires \
         both would refuse the plugin it just opened"
    );
}

/// No geometry call is made on a floating window.
///
/// `set_scale`, `set_parent`, `can_resize`, `adjust_size` and `set_size` are all
/// marked `[main-thread & !floating]` in `ext/gui.h`. A host that calls them
/// anyway is misbehaving against a window it does not own, and plugins with a
/// validation layer print exactly that.
///
/// `get_size` is legal while floating but is not asked either — see
/// `open_floating_editor`'s doc for why reporting a size the host cannot apply
/// is worse than reporting none.
#[test]
fn a_floating_editor_is_asked_for_no_geometry() {
    let probe = Probe::acquire(GuiMode::FloatingOnly);
    let mut loaded = probe.load();

    loaded
        .open_floating_editor(Some(fake_parent()), TEST_TITLE)
        .expect("floating open should succeed");

    let observed = calls(&probe.capture());
    for (id, name) in [
        (GUI_CALL_SET_SCALE, "set_scale"),
        (GUI_CALL_SET_PARENT, "set_parent"),
        (GUI_CALL_GET_SIZE, "get_size"),
        (GUI_CALL_CAN_RESIZE, "can_resize"),
        (GUI_CALL_ADJUST_SIZE, "adjust_size"),
        (GUI_CALL_SET_SIZE, "set_size"),
    ] {
        assert!(
            !observed.contains(&id),
            "{name} must not be called on a floating window — the header marks \
             it !floating, or the answer describes a window the host cannot lay \
             out"
        );
    }
}

/// The suggested title reaches the plugin verbatim.
///
/// Asserted on content, not on the call having happened: a host that called
/// `suggest_title(NULL)` or sent an empty string would satisfy a call-order
/// check while telling the plugin nothing.
#[test]
fn the_host_suggests_a_window_title() {
    let probe = Probe::acquire(GuiMode::FloatingOnly);
    let mut loaded = probe.load();

    loaded
        .open_floating_editor(Some(fake_parent()), TEST_TITLE)
        .expect("floating open should succeed");

    let cap = probe.capture();
    assert_eq!(
        suggested_title(&cap),
        TEST_TITLE.to_str().unwrap(),
        "the plugin must receive the title the host passed, not a truncation \
         or an empty string"
    );
}

/// A `None` transient skips the hint rather than passing null through.
///
/// "Stay above nothing" and "no opinion about stacking" are different requests,
/// and only the second is what a host with no window to parent to means.
#[test]
fn no_transient_parent_skips_the_hint() {
    let probe = Probe::acquire(GuiMode::FloatingOnly);
    let mut loaded = probe.load();

    loaded
        .open_floating_editor(None, TEST_TITLE)
        .expect("floating open should succeed without a transient parent");

    let cap = probe.capture();
    assert!(
        !calls(&cap).contains(&GUI_CALL_SET_TRANSIENT),
        "with no parent window there is nothing to stay above, so the hint is \
         skipped, not sent as null"
    );
    assert!(
        !cap.set_transient_window_non_null,
        "and the plugin saw no transient window at all"
    );
}

/// A floating editor tears down through the same `close_editor`.
///
/// `hide` and `destroy` are the two GUI calls the header does not mark
/// `!floating`, so one teardown serves both modes. Pinned because the
/// alternative — a second close path — is the kind of duplication that leaks a
/// window when only one of the two is called.
#[test]
fn close_editor_tears_down_a_floating_editor() {
    let probe = Probe::acquire(GuiMode::FloatingOnly);
    let mut loaded = probe.load();

    loaded
        .open_floating_editor(Some(fake_parent()), TEST_TITLE)
        .expect("floating open should succeed");
    loaded.close_editor();

    let cap = probe.capture();
    assert!(
        cap.destroyed,
        "close_editor must destroy the plugin's window"
    );
    assert_eq!(
        cap.create_balance, 0,
        "and leave nothing allocated behind it — a floating window the host \
         forgot to destroy outlives the plugin's editor state"
    );
}

/// A plugin with no `create` has no floating editor either.
///
/// The negative that keeps [`has_floating_editor`] honest: it must consult the
/// vtable rather than answer from the extension pointer, which is the same
/// mistake `has_editor` used to make in the other direction.
#[test]
fn a_plugin_without_create_has_no_floating_editor() {
    let probe = Probe::acquire(GuiMode::NoCreate);
    let loaded = probe.load();

    assert!(
        !loaded.has_floating_editor(),
        "a plugin with no `create` fn cannot present any window, floating \
         included — the gui pointer being non-null says nothing about that"
    );
}

/// `get_preferred_api` is reported, and `Some(false)` is distinct from `None`.
///
/// Two probes rather than one, because the interesting property is that the
/// answer *tracks the plugin*. A `prefers_floating` hardcoded either way passes
/// half of this.
#[test]
fn a_plugin_states_its_preferred_mode() {
    {
        let probe = Probe::acquire(GuiMode::FloatingOnly);
        let loaded = probe.load();
        assert_eq!(
            loaded.prefers_floating(),
            Some(true),
            "a floating-only probe prefers floating"
        );
    }

    let probe = Probe::acquire(GuiMode::Embeddable);
    let loaded = probe.load();
    assert_eq!(
        loaded.prefers_floating(),
        Some(false),
        "an embeddable probe prefers embedded — `Some(false)` and `None` must \
         not collapse, or a host cannot tell a stated preference from silence"
    );
}
