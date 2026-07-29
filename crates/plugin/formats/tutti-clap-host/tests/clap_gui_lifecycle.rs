//! Host-conformance harness for the **editor lifecycle** — the
//! `clap_plugin_gui` half of `src/instance/polling.rs`, driven by a real plugin
//! across the real CLAP FFI.
//!
//! ## The bug this exists to prevent
//!
//! `has_editor()` was `!self.extensions.gui.gui.is_null()` — "is there a gui
//! vtable?" rather than "can an editor actually be embedded?". Those differ for
//! two legal CLAP plugin shapes: a **floating-only** plugin (whose
//! `is_api_supported(api, is_floating=false)` is false) and one whose **`create`
//! is absent** from the vtable. Both have a non-null pointer, so both were
//! reported as having an editor.
//!
//! It mattered because `tutti-plugin-server/src/loaders/clap.rs:166` feeds the
//! answer into `Features::EDITOR`, so the DAW rendered an "open editor" button
//! that could not open one.
//!
//! This had already shipped once, in VST3, in the same shape: `has_editor` there
//! asked "is there a controller?" instead of calling `createView(kEditor)`. The
//! CLAP fix is cheaper than the VST3 one — `is_api_supported` is a documented
//! pure predicate, so unlike `createView` it can be asked without allocating and
//! freeing a real GUI.
//!
//! ## Nothing here opens a window
//!
//! The reference plugin's `clap.gui` is pure bookkeeping: it records what the
//! host called and returns configured answers. `set_parent` is handed a host
//! window handle it never dereferences. Every property under test — does
//! `has_editor` ask the right question, does `open_editor` run the spec's call
//! order, does `resize_editor` honour `adjust_size` — is decided before any
//! pixel would exist, so a real X11/Cocoa window would add a display dependency
//! and a headless skip without adding an assertion. **This suite therefore runs
//! headless on every platform** and has no `#[cfg(target_os)]` gate.
//!
//! ## Determinism and the process-global probe
//!
//! The plugin's GUI mode, capture and command word are process-globals (one
//! dlopen'd image shared by the whole binary), so every test holds
//! [`PROBE_LOCK`] for its entire scenario. The mode in particular is read by the
//! host *once at load*, so it must be set before `load` and must not be changed
//! by a concurrent test mid-scenario.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

mod support;
use support::probe_path::probe_path;

use tutti_clap_host::{ClapLoaded, EditorSize, WindowHandle};
use tutti_clap_test_plugin::{
    GuiCapture, GuiMode, GUI_ASPECT_H, GUI_ASPECT_W, GUI_CALL_ADJUST_SIZE, GUI_CALL_CAN_RESIZE,
    GUI_CALL_CREATE, GUI_CALL_DESTROY, GUI_CALL_GET_RESIZE_HINTS, GUI_CALL_GET_SIZE, GUI_CALL_HIDE,
    GUI_CALL_IS_API_SUPPORTED, GUI_CALL_SET_PARENT, GUI_CALL_SET_SCALE, GUI_CALL_SET_SIZE,
    GUI_CALL_SHOW, GUI_CMD_CLOSED_AND_DESTROYED, GUI_CMD_CLOSED_AND_DESTROYED_FROM_SHOW,
    GUI_CMD_CLOSED_NOT_DESTROYED, GUI_CMD_REQUEST_RESIZE, GUI_HEIGHT, GUI_REQUESTED_RESIZE_H,
    GUI_REQUESTED_RESIZE_W, GUI_SIZE_QUANTUM, GUI_WIDTH,
};

/// Serializes whole scenarios — set mode → reset → load → drive → read — so one
/// test cannot observe another's calls or run against another's selected
/// [`GuiMode`].
static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// A stand-in parent window handle.
///
/// Never dereferenced by anything: the host passes it straight into the
/// `clap_window` it builds, and the probe's `set_parent` records only that a
/// window arrived and what `api` string came with it. A non-null,
/// obviously-fake value is better than null here because it proves the host
/// forwarded *our* handle rather than substituting a zero.
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
    /// The mode is set **before** any `load`, because the host caches the
    /// `clap.gui` extension pointer once during load — `GuiMode::Absent` and
    /// `GuiMode::NoCreate` are literally different vtables (or none), so they
    /// cannot be switched afterwards.
    fn acquire(mode: GuiMode) -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        set_gui_mode(mode as u32);
        gui_reset();
        Probe { _lock: lock }
    }

    /// Load the reference plugin through the real host.
    ///
    /// Deliberately **not** activated: the CLAP editor lifecycle is defined on
    /// a loaded plugin and every method under test lives on `ClapLoaded`, so
    /// activating would add an unrelated audio setup whose failure would be
    /// reported as a GUI failure.
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
    /// waiting for the plugin's next host-driven callback. See
    /// [`GUI_CMD_CLOSED_AND_DESTROYED`].
    fn run_command(&self) {
        assert!(
            run_gui_command(),
            "the probe must have a live host to call back into — did the editor \
             get created?"
        );
    }
}

impl Drop for Probe {
    /// Return the probe to the no-GUI shape every other suite in this crate
    /// was written against. The mode is a process-global that outlives the
    /// test, and a leaked `Embeddable` would silently give the audio and
    /// threading suites a `clap.gui` extension they never asked for.
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
//
// ## Why the handle is leaked instead of dropped per call
//
// The sibling suites open the library, call their symbol, and let the
// `Library` drop — which `dlclose`s it. That is harmless *there* because they
// only ever write probe state while the host already holds the image open, so
// the refcount never reaches zero.
//
// It is fatal here. This suite must set the [`GuiMode`] **before** the host
// loads the plugin, since the host caches the `clap.gui` extension pointer
// once during load. With a per-call handle that store happened in an image
// whose refcount then dropped to zero: the loader unmapped it, the host's
// subsequent load mapped a *fresh* copy with `GUI_MODE` back at its
// `Absent` initializer, and every test failed with "No GUI extension" —
// the plugin faithfully reporting the mode it actually had.
//
// One process-lifetime handle keeps the image mapped across the gap, so the
// mode the test sets is the mode the host loads into.

/// The probe image, opened once and kept mapped for the life of the test
/// binary. See the note above: dropping it between a mode store and the host's
/// load unmaps the global the store just wrote.
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
// has_editor — the bug.
// ===========================================================================

/// The baseline: a plugin that really can embed an editor is reported as having
/// one.
///
/// On its own this passes against the broken `!gui.is_null()` too — it is here
/// so the three negative cases below cannot be satisfied by a `has_editor` that
/// simply returns `false`.
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
/// The second assertion is what makes the first one meaningful: `open_editor`
/// genuinely cannot embed this plugin, so a `has_editor` that says yes is
/// promising something the host cannot deliver.
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
/// This is the CLAP-specific half of the fix. VST3 had no choice but to
/// `createView` and release; CLAP documents `is_api_supported` as a pure
/// predicate ("Returns true if the requested gui api is supported"), while
/// `create` "allocates all resources necessary for the gui". A `has_editor`
/// implemented by create-then-destroy would spin up OpenGL contexts and worker
/// threads on every scanned plugin, so the absence of `create` in the call log
/// is a property worth pinning, not an implementation detail.
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
/// The probe answers `is_api_supported` false for any api string that is not
/// its own platform's, so a host that hardcoded X11 reports "no editor" on
/// macOS and Windows — a silent, total loss of plugin GUIs on two of three
/// platforms, which is precisely the kind of bug that survives a Linux-only CI.
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

/// The spec's embed order, observed from inside a real plugin.
///
/// `polling.rs`'s in-crate test already asserts this order against a hand-written
/// vtable. What it cannot show is that the host reaches that code with a
/// correctly-built `clap_window` — the right platform `api` string and the
/// caller's handle — which is what the `set_parent` assertions below add.
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
        vec![
            GUI_CALL_IS_API_SUPPORTED,
            GUI_CALL_CREATE,
            GUI_CALL_SET_SCALE,
            GUI_CALL_GET_SIZE,
            GUI_CALL_SET_PARENT,
            GUI_CALL_SHOW,
        ],
        "CLAP order: is_api_supported gates create, set_scale precedes get_size \
         so the reported size already accounts for DPI, set_parent follows \
         get_size and precedes show"
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
    assert_eq!(
        cap.last_scale, 1.0,
        "the host passes a scale; 1.0 is today's placeholder until the frontend \
         carries a real backing-scale factor"
    );
}

/// A floating-only plugin is refused at the first gate, before `create`.
///
/// Degrading gracefully matters more than it looks: `create` on a plugin that
/// just said it cannot do embedded is undefined territory, and the plugins that
/// handle it least well are the ones that would crash the host.
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

/// H5: when the plugin destroyed its own editor and told the host so via
/// `gui.closed(was_destroyed = true)`, the host must **not** call `destroy`
/// again.
///
/// A double-destroy is a use-after-free in the plugin, which is why the host
/// keeps an `already_destroyed` latch rather than trusting its own
/// `gui_created` flag alone. The probe's balance is what makes the second
/// destroy visible: it would read -1.
#[test]
fn close_editor_skips_destroy_after_plugin_self_destroyed() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let mut loaded = probe.load();

    loaded.open_editor(fake_parent()).expect("opens");

    // The plugin tears its own editor down and reports it — a user closing the
    // plugin's own window while the editor sits open. Driven out-of-band, i.e.
    // *between* the host's calls; the during-`show` case is its own test below.
    probe.command(GUI_CMD_CLOSED_AND_DESTROYED);
    probe.run_command();

    assert!(
        loaded.poll_gui_closed(),
        "the host must record the plugin's `gui.closed` callback"
    );
    let after_open = probe.capture();
    assert_eq!(
        after_open.create_balance, 0,
        "the plugin destroyed its own editor, so nothing is live"
    );

    loaded.close_editor();

    let cap = probe.capture();
    assert_eq!(
        cap.create_balance, 0,
        "close_editor must skip hide/destroy entirely — calling destroy on an \
         already-destroyed editor is a double-destroy, and would read -1 here"
    );
    assert!(
        !calls(&cap).contains(&GUI_CALL_DESTROY),
        "the host must not have called destroy at all"
    );
}

/// The same H5 hazard, but the plugin self-destroys from **inside `show`** —
/// while the host is still within `open_editor`.
///
/// A plugin does this when it finds during the embed that it cannot present.
/// The host used to clear its `already_destroyed` latch *after*
/// `embed_editor_sequence` returned, so this callback was wiped by the very
/// call that carried it and the host went on to `destroy` an editor that had
/// already torn itself down.
///
/// The out-of-band test above cannot catch it: firing between the host's calls
/// lands after the clear either way. Only a callback raised inside the sequence
/// distinguishes clearing before it from clearing after.
#[test]
fn close_editor_skips_destroy_after_plugin_self_destroyed_during_show() {
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
    assert_eq!(
        after_open.create_balance, 0,
        "the plugin destroyed its own editor from inside show, so nothing is live"
    );

    loaded.close_editor();

    let cap = probe.capture();
    assert_eq!(
        cap.create_balance, 0,
        "close_editor must skip destroy — a second destroy on the editor the \
         plugin already tore down would read -1 here"
    );
    assert!(
        !calls(&cap).contains(&GUI_CALL_DESTROY),
        "the host must not have called destroy at all"
    );
}

/// `gui.closed(was_destroyed = false)` is the *other* half: the plugin's window
/// went away but its resources are intact, so the host still owes it a
/// `hide`/`destroy`.
///
/// Distinguishing the two is the whole reason CLAP passes a flag. A host that
/// treated every `closed` as self-destroyed would leak the plugin's GUI
/// resources for the life of the instance.
#[test]
fn close_editor_still_destroys_when_plugin_was_not_destroyed() {
    let probe = Probe::acquire(GuiMode::Embeddable);
    let mut loaded = probe.load();

    probe.command(GUI_CMD_CLOSED_NOT_DESTROYED);
    loaded.open_editor(fake_parent()).expect("opens");

    assert!(loaded.poll_gui_closed(), "the host records the callback");
    assert_eq!(
        probe.capture().create_balance,
        1,
        "the editor is still allocated — the plugin only reported its window closed"
    );

    loaded.close_editor();

    let cap = probe.capture();
    assert!(
        calls(&cap).contains(&GUI_CALL_DESTROY),
        "was_destroyed = false means the plugin still holds gui resources, so \
         the host must destroy them"
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
/// enforce it — TAL-Reverb-4 prints `[clap-plugin HOST-MISBEHAVING]
/// clap_plugin_gui.can_resize() was called without a prior call to
/// clap_plugin_gui.create()`. The host guarded only on the extension pointer,
/// so it violated that on every pre-create query and the diagnostic went to a
/// log nobody read.
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

/// **The audit finding.** `adjust_size` returning false means the plugin could
/// not compute a usable size — CLAP: "Returns true if the plugin could adjust
/// the given size." The host previously read that as "no snap to apply" and
/// forwarded the *unadjusted* request to `set_size`.
///
/// That inverts the meaning. The one case where the plugin has explicitly said
/// "I cannot give you a working size" was the case where the host pushed the raw
/// size through anyway, and the out-params it forwarded were never promised to
/// have been written.
///
/// The probe's fixed-size mode accepts only its own dimensions, so the old
/// behaviour is observable: it reached `set_size` and was refused there,
/// producing a `set_size refused` error that named the wrong call.
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
/// not get mistaken for permissive ones.
///
/// The safe direction matters: a host that resizes a fixed-size editor corrupts
/// its layout, so an unanswered `get_resize_hints` must not read as
/// "resizable in both axes".
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
///
/// The consume-on-read semantics are the point: a poll that kept returning the
/// last request would make the host resize the editor on every UI frame.
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
/// CLAP requires `gui.destroy()` before `plugin.destroy()`, so this is not
/// merely tidiness: the reverse order hands the plugin's GUI code a destroyed
/// instance. `ClapLoaded::drop` calls `close_editor` for exactly this reason,
/// and nothing else in the suite covers the path where the host never gets an
/// explicit close.
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
