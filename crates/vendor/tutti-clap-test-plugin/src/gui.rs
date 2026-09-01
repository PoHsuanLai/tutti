//! GUI probe — the plugin side of the host's editor lifecycle.
//!
//! Its own module so it owns an independent capture channel and switches, and
//! cannot perturb the other oracles. `lib.rs` holds only the `get_extension`
//! hook.
//!
//! **This opens no windows.** Every fn in the `clap_plugin_gui` vtable below is
//! pure bookkeeping: it records that the host called it, returns the answer the
//! test configured, and touches nothing native. `set_parent` is handed a host
//! window handle the probe never dereferences.
//!
//! The answers are process-global switches because the interesting `has_editor`
//! cases are *plugin shapes*, not runtime states. A vtable is a `static` and the
//! host caches the extension pointer once at load, so the test selects the shape
//! with [`crate::gui::tutti_test_plugin_set_gui_mode`] before loading.

use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use clap_sys::ext::gui::{
    clap_gui_resize_hints, clap_host_gui, clap_plugin_gui, clap_window, CLAP_EXT_GUI,
};
use clap_sys::host::clap_host;
use clap_sys::plugin::clap_plugin;

// ---------------------------------------------------------------------------
// Configuration — the plugin *shape* the probe presents.
// ---------------------------------------------------------------------------

/// Which `clap.gui` shape the probe presents to the host.
///
/// The host reads the vtable **once at load time** and caches the pointer, so a
/// test picks the shape via [`crate::gui::tutti_test_plugin_set_gui_mode`] before calling
/// `ClapLoaded::load`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuiMode {
    /// No `clap.gui` extension at all — `get_extension("clap.gui")` returns
    /// null. The one shape a host can rule out on the pointer alone.
    Absent = 0,
    /// A normal embeddable editor: `is_api_supported(api, is_floating=false)`
    /// is true and `create` is present.
    Embeddable = 1,
    /// Floating-only: the extension is present and `create` exists, but
    /// `is_api_supported` answers true **only** for `is_floating == true`.
    ///
    /// The vtable is non-null, so a `has_editor` written as `!gui.is_null()`
    /// says yes and `open_editor` then fails at the embed gate.
    FloatingOnly = 2,
    /// The extension is present and the API is supported, but `create` is
    /// absent from the vtable.
    ///
    /// Legal CLAP — every `clap_plugin_gui` member is an `Option<fn>` — and
    /// equally invisible to a null-pointer check.
    NoCreate = 3,
    /// Embeddable, but fixed-size: `can_resize` is false and `adjust_size`
    /// returns false (it cannot produce a usable size). Pins the
    /// `resize_editor` contract.
    FixedSize = 4,
}

impl GuiMode {
    fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Embeddable,
            2 => Self::FloatingOnly,
            3 => Self::NoCreate,
            4 => Self::FixedSize,
            _ => Self::Absent,
        }
    }
}

/// The size `get_size` reports. Not 800x600: that is the host's *fallback*
/// when `get_size` is absent or fails, so a probe reporting it would make
/// "the host read our size" and "the host gave up and guessed"
/// indistinguishable.
pub const GUI_WIDTH: u32 = 442;
/// See [`GUI_WIDTH`].
pub const GUI_HEIGHT: u32 = 337;

/// The aspect ratio `get_resize_hints` reports when resizable. Coprime, so a
/// host that reduces or swaps the pair is caught.
pub const GUI_ASPECT_W: u32 = 16;
/// See [`GUI_ASPECT_W`].
pub const GUI_ASPECT_H: u32 = 9;

/// Granularity `adjust_size` snaps to in the resizable modes. A host that
/// forwards the *unadjusted* request to `set_size` reports a size that is not
/// a multiple of this, which is the whole point of the snap.
pub const GUI_SIZE_QUANTUM: u32 = 10;

static GUI_MODE: AtomicU32 = AtomicU32::new(GuiMode::Absent as u32);

fn gui_mode() -> GuiMode {
    GuiMode::from_u32(GUI_MODE.load(Ordering::SeqCst))
}

/// Select the `clap.gui` shape the probe presents. Call **before** the host
/// loads the plugin — the extension pointer is cached once during load. Takes
/// the [`GuiMode`] discriminant as a bare `u32` (a C ABI boundary).
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_gui_mode(mode: u32) {
    GUI_MODE.store(mode, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Capture — what the host called, in what order.
// ---------------------------------------------------------------------------

/// Maximum vtable calls recorded. The lifecycle under test is a handful of
/// calls; extras beyond this bump `call_count` without being stored, so an
/// overflow reads as a failed length assertion rather than a silent truncation.
pub const MAX_GUI_CALLS: usize = 32;

/// Identifiers for the `clap_plugin_gui` fns the probe records, in
/// [`GuiCapture::calls`]. Part of the `#[repr(C)]` contract with the test.
pub const GUI_CALL_IS_API_SUPPORTED: u32 = 1;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_CREATE: u32 = 2;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_DESTROY: u32 = 3;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_SET_SCALE: u32 = 4;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_GET_SIZE: u32 = 5;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_CAN_RESIZE: u32 = 6;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_GET_RESIZE_HINTS: u32 = 7;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_ADJUST_SIZE: u32 = 8;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_SET_SIZE: u32 = 9;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_SET_PARENT: u32 = 10;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_SHOW: u32 = 11;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_HIDE: u32 = 12;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_SET_TRANSIENT: u32 = 13;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_SUGGEST_TITLE: u32 = 14;
/// See [`GUI_CALL_IS_API_SUPPORTED`].
pub const GUI_CALL_GET_PREFERRED_API: u32 = 15;

/// The GUI snapshot the conformance test reads across the dlopen seam.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct GuiCapture {
    /// The `GUI_CALL_*` ids the host invoked, in call order.
    pub calls: [u32; MAX_GUI_CALLS],
    /// How many calls happened since the last reset. May exceed
    /// [`MAX_GUI_CALLS`], in which case `calls` holds only the first that many.
    pub call_count: u32,
    /// How many times `is_api_supported` was asked with `is_floating == false`.
    ///
    /// Split from the floating count because that distinction *is* the bug: a
    /// host asking only the floating question would conclude a floating-only
    /// plugin is embeddable.
    pub is_api_supported_embedded_queries: u32,
    /// How many times `is_api_supported` was asked with `is_floating == true`.
    pub is_api_supported_floating_queries: u32,
    /// Whether `create` has ever run since the last reset.
    pub created: bool,
    /// Whether `destroy` has run since the last reset.
    pub destroyed: bool,
    /// Net `create` minus `destroy` count. Positive at teardown means the host
    /// leaked the gui resources `create` allocated; negative means it destroyed
    /// more times than it created.
    ///
    /// A window closing (`GUI_CMD_CLOSED_AND_DESTROYED`) does **not** move it —
    /// see [`window_destroyed`](GuiCapture::window_destroyed) — so the host's
    /// obligation to acknowledge with `destroy()` stays visible here.
    pub create_balance: i32,
    /// Whether the probe reported `gui.closed(was_destroyed = true)` since the
    /// last reset, i.e. its window went away on its own.
    pub window_destroyed: bool,
    /// The `scale` the host passed to `set_scale`, or 0.0 if never called.
    pub last_scale: f64,
    /// The width/height the host last passed to `set_size`.
    pub last_set_size_w: u32,
    /// See [`GuiCapture::last_set_size_w`].
    pub last_set_size_h: u32,
    /// The width/height the host last passed *into* `adjust_size`, before the
    /// probe snapped them. Lets the test prove the host forwarded the snapped
    /// value rather than the raw request.
    pub last_adjust_in_w: u32,
    /// See [`GuiCapture::last_adjust_in_w`].
    pub last_adjust_in_h: u32,
    /// Whether the host ever passed a non-null `clap_window` to `set_parent`.
    pub set_parent_window_non_null: bool,
    /// Whether that `clap_window`'s `api` string was the current platform's
    /// CLAP constant — the "do not hardcode X11" assertion.
    pub set_parent_api_matches_platform: bool,
    /// Whether the host ever passed a non-null `clap_window` to `set_transient`.
    ///
    /// The floating counterpart to
    /// [`set_parent_window_non_null`](GuiCapture::set_parent_window_non_null).
    /// A host that passes null here has told the plugin to stay above nothing,
    /// which is not the same request as skipping the hint.
    pub set_transient_window_non_null: bool,
    /// Whether `create` was last called with `is_floating == true`.
    ///
    /// Not a count: what matters is which *mode* the window that now exists was
    /// built in, and a host that created embedded then floating has one window,
    /// not two.
    pub created_floating: bool,
    /// The title the host passed to `suggest_title`, NUL-terminated and
    /// truncated to fit. Empty if never called.
    ///
    /// Captured as bytes rather than a flag so a host sending an empty or
    /// garbage title is distinguishable from one sending nothing.
    pub suggested_title: [u8; MAX_TITLE_LEN],
}

/// Bound on [`GuiCapture::suggested_title`]. Fixed-size because the capture
/// crosses the dlopen seam as `#[repr(C)]` — a `String` could not.
pub const MAX_TITLE_LEN: usize = 64;

impl Default for GuiCapture {
    fn default() -> Self {
        Self {
            calls: [0; MAX_GUI_CALLS],
            call_count: 0,
            is_api_supported_embedded_queries: 0,
            is_api_supported_floating_queries: 0,
            created: false,
            destroyed: false,
            create_balance: 0,
            window_destroyed: false,
            last_scale: 0.0,
            last_set_size_w: 0,
            last_set_size_h: 0,
            last_adjust_in_w: 0,
            last_adjust_in_h: 0,
            set_parent_window_non_null: false,
            set_parent_api_matches_platform: false,
            set_transient_window_non_null: false,
            created_floating: false,
            suggested_title: [0; MAX_TITLE_LEN],
        }
    }
}

// ---------------------------------------------------------------------------
// Backing storage. Atomics rather than a mutex: the call log is an
// append-and-index, which a `fetch_add` expresses without a poisoning path in
// the middle of an FFI callback.
// ---------------------------------------------------------------------------

struct GuiGlobals {
    calls: [AtomicU32; MAX_GUI_CALLS],
    call_count: AtomicU32,
    embedded_queries: AtomicU32,
    floating_queries: AtomicU32,
    created: AtomicBool,
    destroyed: AtomicBool,
    /// Net create-minus-destroy, kept as the two's-complement `u32` of an
    /// `i32` so a host that destroys more than it created reads back negative
    /// rather than wrapping to a huge positive.
    create_balance: AtomicU32,
    /// Whether the probe told the host its window was destroyed.
    window_destroyed: AtomicBool,
    /// The scale as an `f32` bit pattern — there is no `AtomicF64`, and the
    /// values in play (1.0, 2.0) are exact in f32.
    last_scale_bits: AtomicU32,
    last_set_size_w: AtomicU32,
    last_set_size_h: AtomicU32,
    last_adjust_in_w: AtomicU32,
    last_adjust_in_h: AtomicU32,
    set_parent_window_non_null: AtomicBool,
    set_parent_api_matches_platform: AtomicBool,
    set_transient_window_non_null: AtomicBool,
    created_floating: AtomicBool,
    /// The suggested title, one byte per cell. An array of atomics rather than
    /// a `Mutex<String>` for the reason at the top of this block: these are
    /// written from inside an FFI callback, and a title is short enough that
    /// per-byte stores cost nothing.
    suggested_title: [AtomicU32; MAX_TITLE_LEN],
    /// Latched command; see [`tutti_test_plugin_gui_command`].
    command: AtomicU32,
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO_U32: AtomicU32 = AtomicU32::new(0);

static GUI: GuiGlobals = GuiGlobals {
    calls: [ZERO_U32; MAX_GUI_CALLS],
    call_count: AtomicU32::new(0),
    embedded_queries: AtomicU32::new(0),
    floating_queries: AtomicU32::new(0),
    created: AtomicBool::new(false),
    destroyed: AtomicBool::new(false),
    create_balance: AtomicU32::new(0),
    window_destroyed: AtomicBool::new(false),
    last_scale_bits: AtomicU32::new(0),
    last_set_size_w: AtomicU32::new(0),
    last_set_size_h: AtomicU32::new(0),
    last_adjust_in_w: AtomicU32::new(0),
    last_adjust_in_h: AtomicU32::new(0),
    set_parent_window_non_null: AtomicBool::new(false),
    set_parent_api_matches_platform: AtomicBool::new(false),
    set_transient_window_non_null: AtomicBool::new(false),
    created_floating: AtomicBool::new(false),
    suggested_title: [ZERO_U32; MAX_TITLE_LEN],
    command: AtomicU32::new(GUI_CMD_NONE),
};

/// Append `id` to the call log. Over-long logs bump the count without storing,
/// so the test's length assertion fails rather than the record silently lying.
fn record(id: u32) {
    let idx = GUI.call_count.fetch_add(1, Ordering::AcqRel) as usize;
    if idx < MAX_GUI_CALLS {
        GUI.calls[idx].store(id, Ordering::Release);
    }
}

/// Copy the GUI snapshot out to `out`.
///
/// # Safety
/// `out` must point to a valid, writable [`GuiCapture`].
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_gui_capture(out: *mut GuiCapture) -> bool {
    if out.is_null() {
        return false;
    }
    let mut cap = GuiCapture {
        call_count: GUI.call_count.load(Ordering::Acquire),
        is_api_supported_embedded_queries: GUI.embedded_queries.load(Ordering::Acquire),
        is_api_supported_floating_queries: GUI.floating_queries.load(Ordering::Acquire),
        created: GUI.created.load(Ordering::Acquire),
        destroyed: GUI.destroyed.load(Ordering::Acquire),
        create_balance: GUI.create_balance.load(Ordering::Acquire) as i32,
        window_destroyed: GUI.window_destroyed.load(Ordering::Acquire),
        // Widened from the f32 bit pattern the store side keeps: the scale
        // slot is an `AtomicU32` because the host passes small exact values
        // (1.0, 2.0), for which f32 is lossless, and the test only needs to
        // tell "the host set a scale" from "it did not".
        last_scale: f32::from_bits(GUI.last_scale_bits.load(Ordering::Acquire)) as f64,
        last_set_size_w: GUI.last_set_size_w.load(Ordering::Acquire),
        last_set_size_h: GUI.last_set_size_h.load(Ordering::Acquire),
        last_adjust_in_w: GUI.last_adjust_in_w.load(Ordering::Acquire),
        last_adjust_in_h: GUI.last_adjust_in_h.load(Ordering::Acquire),
        set_parent_window_non_null: GUI.set_parent_window_non_null.load(Ordering::Acquire),
        set_parent_api_matches_platform: GUI
            .set_parent_api_matches_platform
            .load(Ordering::Acquire),
        set_transient_window_non_null: GUI.set_transient_window_non_null.load(Ordering::Acquire),
        created_floating: GUI.created_floating.load(Ordering::Acquire),
        ..GuiCapture::default()
    };
    for (dst, src) in cap.calls.iter_mut().zip(GUI.calls.iter()) {
        *dst = src.load(Ordering::Acquire);
    }
    for (dst, src) in cap
        .suggested_title
        .iter_mut()
        .zip(GUI.suggested_title.iter())
    {
        *dst = src.load(Ordering::Acquire) as u8;
    }
    *out = cap;
    true
}

/// Clear every recorded call and counter. The test calls this before each
/// scenario, because the whole test binary shares one loaded image and
/// therefore one global.
///
/// Does **not** clear the selected [`GuiMode`]: the mode is chosen before load
/// and must survive a mid-scenario reset.
#[no_mangle]
pub extern "C" fn tutti_test_plugin_gui_reset() {
    for slot in GUI.calls.iter() {
        slot.store(0, Ordering::Release);
    }
    GUI.call_count.store(0, Ordering::Release);
    GUI.embedded_queries.store(0, Ordering::Release);
    GUI.floating_queries.store(0, Ordering::Release);
    GUI.created.store(false, Ordering::Release);
    GUI.destroyed.store(false, Ordering::Release);
    GUI.create_balance.store(0, Ordering::Release);
    GUI.window_destroyed.store(false, Ordering::Release);
    GUI.last_scale_bits.store(0, Ordering::Release);
    GUI.last_set_size_w.store(0, Ordering::Release);
    GUI.last_set_size_h.store(0, Ordering::Release);
    GUI.last_adjust_in_w.store(0, Ordering::Release);
    GUI.last_adjust_in_h.store(0, Ordering::Release);
    GUI.set_parent_window_non_null
        .store(false, Ordering::Release);
    GUI.set_parent_api_matches_platform
        .store(false, Ordering::Release);
    GUI.set_transient_window_non_null
        .store(false, Ordering::Release);
    GUI.created_floating.store(false, Ordering::Release);
    // Cleared, not left: a title from a previous scenario would make the next
    // one's "the host sent a title" assertion pass without the host sending
    // anything — green in one test order and red in another.
    for slot in GUI.suggested_title.iter() {
        slot.store(0, Ordering::Release);
    }
    GUI.command.store(GUI_CMD_NONE, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Commands — behaviours that only exist if the plugin initiates them.
// ---------------------------------------------------------------------------

/// No pending command.
pub const GUI_CMD_NONE: u32 = 0;
/// Call `host.gui.request_resize(w, h)` from the next `show`.
///
/// `show` is the site because it is the last call in the host's embed sequence,
/// so a resize requested there lands while the host still considers the editor
/// live — which is when a real plugin discovers its content does not fit.
pub const GUI_CMD_REQUEST_RESIZE: u32 = 1;
/// Call `host.gui.closed(was_destroyed = false)` from the next `show`. The
/// plugin's window went away but its resources are intact, so the host should
/// still run `hide`/`destroy`.
pub const GUI_CMD_CLOSED_NOT_DESTROYED: u32 = 2;
/// Call `host.gui.closed(was_destroyed = true)`: the probe's window went away.
/// Its gui object stays allocated, so a host that never calls `gui.destroy`
/// leaks it and shows up as a `create_balance` still at 1.
///
/// **Not** run from `show`: the test drives it via
/// [`tutti_test_plugin_gui_run_command`] after `open_editor` returns, modelling
/// a user closing the plugin's own window while the editor sits open.
pub const GUI_CMD_CLOSED_AND_DESTROYED: u32 = 3;

/// The same, but **from inside `show`** — while the host is still within
/// `open_editor`. A plugin does this when it discovers during the embed that it
/// cannot present (no display, a failed GL context).
///
/// Distinguishes clearing the host's window-destroyed latch *before* the embed
/// sequence from clearing it after; clearing after wipes the callback the
/// sequence just carried.
pub const GUI_CMD_CLOSED_AND_DESTROYED_FROM_SHOW: u32 = 4;

/// The size the plugin asks the host for via [`GUI_CMD_REQUEST_RESIZE`].
/// Deliberately unrelated to [`GUI_WIDTH`]/[`GUI_HEIGHT`] so a host echoing the
/// initial size cannot pass.
pub const GUI_REQUESTED_RESIZE_W: u32 = 1024;
/// See [`GUI_REQUESTED_RESIZE_W`].
pub const GUI_REQUESTED_RESIZE_H: u32 = 768;

/// Latch a command for the plugin to run at its next legal call site, returning
/// the previous one. [`GUI_CMD_NONE`] clears.
///
/// # Safety
/// Safe to call from any thread; a single swap. `extern "C"` only so the test
/// can reach it across the dlopen seam.
#[no_mangle]
pub extern "C" fn tutti_test_plugin_gui_command(cmd: u32) -> u32 {
    GUI.command.swap(cmd, Ordering::AcqRel)
}

/// The `clap_host` of the most recent `gui.create`. A `clap_plugin_gui` fn
/// receives only the `clap_plugin`, so this is the probe's only route back to
/// the host from a test-initiated call.
static LAST_HOST: std::sync::atomic::AtomicPtr<clap_host> =
    std::sync::atomic::AtomicPtr::new(ptr::null_mut());

/// Run the latched command **now**, from the caller's thread, rather than
/// waiting for the next host-driven callback — [`GUI_CMD_CLOSED_AND_DESTROYED`]
/// must land *between* `open_editor` and `close_editor`, not inside either.
///
/// Returns false if no editor has been created (so no host pointer is known) or
/// nothing was latched.
///
/// # Safety
/// The host recorded at `create` must still be alive — i.e. the `ClapLoaded`
/// that opened the editor must not have been dropped.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_gui_run_command() -> bool {
    let host = LAST_HOST.load(Ordering::Acquire);
    if host.is_null() || GUI.command.load(Ordering::Acquire) == GUI_CMD_NONE {
        return false;
    }
    run_command_against(host);
    true
}

/// Fetch a host extension vtable by id, or `None` if the host does not offer it.
///
/// # Safety
/// `host` must be null or valid, and `T` must be the vtable type CLAP defines
/// for `id`.
unsafe fn host_ext<'a, T>(host: *const clap_host, id: &CStr) -> Option<&'a T> {
    if host.is_null() {
        return None;
    }
    let get_ext = (*host).get_extension?;
    let ptr = get_ext(host, id.as_ptr());
    if ptr.is_null() {
        None
    } else {
        Some(&*(ptr as *const T))
    }
}

/// Run whatever command the test latched, from inside a host-driven callback.
///
/// [`GUI_CMD_CLOSED_AND_DESTROYED`] is excluded: it must land *between* the
/// host's calls. [`GUI_CMD_CLOSED_AND_DESTROYED_FROM_SHOW`] exists precisely to
/// fire from here, so it is not.
///
/// # Safety
/// `plugin` must be a `clap_plugin` this crate's factory produced.
unsafe fn run_pending_command(plugin: *const clap_plugin) {
    if GUI.command.load(Ordering::Acquire) == GUI_CMD_CLOSED_AND_DESTROYED {
        return;
    }
    run_command_against(crate::plugin_host(plugin));
}

/// Claim the latched command and run it against `host`. Consumes it so it
/// fires once.
///
/// # Safety
/// `host` must be null or a valid, live `clap_host`.
unsafe fn run_command_against(host: *const clap_host) {
    let cmd = GUI.command.load(Ordering::Acquire);
    if cmd == GUI_CMD_NONE {
        return;
    }
    // Claim before running so a re-entrant visit cannot run it twice.
    if GUI
        .command
        .compare_exchange(cmd, GUI_CMD_NONE, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let Some(gui_host) = host_ext::<clap_host_gui>(host, CLAP_EXT_GUI) else {
        return;
    };
    match cmd {
        GUI_CMD_REQUEST_RESIZE => {
            if let Some(f) = gui_host.request_resize {
                f(host, GUI_REQUESTED_RESIZE_W, GUI_REQUESTED_RESIZE_H);
            }
        }
        GUI_CMD_CLOSED_NOT_DESTROYED => {
            if let Some(f) = gui_host.closed {
                f(host, false);
            }
        }
        // Both window-destroyed variants behave identically here; they differ
        // only in *when* they are dispatched — see `run_pending_command`.
        GUI_CMD_CLOSED_AND_DESTROYED | GUI_CMD_CLOSED_AND_DESTROYED_FROM_SHOW => {
            // The *window* is gone; the gui object `create` allocated is not.
            // `ext/gui.h` obliges the host to call `destroy()` to acknowledge
            // this, which is what releases that object — so `create_balance` is
            // untouched here and stays positive until the host does its part.
            // Marking it torn down here would make a host that leaks the
            // resources look correct.
            GUI.window_destroyed.store(true, Ordering::Release);
            if let Some(f) = gui_host.closed {
                f(host, true);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// The vtable. Pure bookkeeping — see the module docs.
// ---------------------------------------------------------------------------

/// The CLAP window api constant for the platform this probe was built for.
/// `set_parent` compares the host's `clap_window.api` against it, which makes
/// "the host hardcoded x11" a failure on macOS and Windows.
fn platform_api() -> &'static CStr {
    #[cfg(target_os = "macos")]
    {
        clap_sys::ext::gui::CLAP_WINDOW_API_COCOA
    }
    #[cfg(target_os = "windows")]
    {
        clap_sys::ext::gui::CLAP_WINDOW_API_WIN32
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        clap_sys::ext::gui::CLAP_WINDOW_API_X11
    }
}

unsafe extern "C" fn gui_is_api_supported(
    _plugin: *const clap_plugin,
    api: *const std::ffi::c_char,
    is_floating: bool,
) -> bool {
    record(GUI_CALL_IS_API_SUPPORTED);
    if is_floating {
        GUI.floating_queries.fetch_add(1, Ordering::AcqRel);
    } else {
        GUI.embedded_queries.fetch_add(1, Ordering::AcqRel);
    }
    // An unrecognised api string is a "no" regardless of mode — a host that
    // sends the wrong platform's constant must not be told yes.
    if api.is_null() || CStr::from_ptr(api) != platform_api() {
        return false;
    }
    match gui_mode() {
        // The whole point of this mode: embedded is refused, floating is not.
        GuiMode::FloatingOnly => is_floating,
        GuiMode::Absent => false,
        _ => true,
    }
}

unsafe extern "C" fn gui_create(
    plugin: *const clap_plugin,
    api: *const std::ffi::c_char,
    is_floating: bool,
) -> bool {
    record(GUI_CALL_CREATE);
    if api.is_null() || CStr::from_ptr(api) != platform_api() {
        return false;
    }
    // Refuse the same combination `is_api_supported` refuses. A host that
    // skipped the query and called `create` blind gets a clean failure rather
    // than a half-built editor.
    if gui_mode() == GuiMode::FloatingOnly && !is_floating {
        return false;
    }
    // Remember the host so a test can drive a callback out-of-band; see
    // `tutti_test_plugin_gui_run_command`.
    LAST_HOST.store(
        crate::plugin_host(plugin) as *mut clap_host,
        Ordering::Release,
    );
    GUI.created.store(true, Ordering::Release);
    GUI.created_floating.store(is_floating, Ordering::Release);
    GUI.create_balance.fetch_add(1, Ordering::AcqRel);
    true
}

unsafe extern "C" fn gui_destroy(_plugin: *const clap_plugin) {
    record(GUI_CALL_DESTROY);
    GUI.destroyed.store(true, Ordering::Release);
    GUI.create_balance.fetch_sub(1, Ordering::AcqRel);
}

unsafe extern "C" fn gui_set_scale(_plugin: *const clap_plugin, scale: f64) -> bool {
    record(GUI_CALL_SET_SCALE);
    GUI.last_scale_bits
        .store((scale as f32).to_bits(), Ordering::Release);
    true
}

unsafe extern "C" fn gui_get_size(
    _plugin: *const clap_plugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    record(GUI_CALL_GET_SIZE);
    if width.is_null() || height.is_null() {
        return false;
    }
    *width = GUI_WIDTH;
    *height = GUI_HEIGHT;
    true
}

unsafe extern "C" fn gui_can_resize(_plugin: *const clap_plugin) -> bool {
    record(GUI_CALL_CAN_RESIZE);
    gui_mode() != GuiMode::FixedSize
}

unsafe extern "C" fn gui_get_resize_hints(
    _plugin: *const clap_plugin,
    hints: *mut clap_gui_resize_hints,
) -> bool {
    record(GUI_CALL_GET_RESIZE_HINTS);
    if hints.is_null() {
        return false;
    }
    // A fixed-size editor has no hints to give; CLAP says the return value is
    // "can the plugin provide hints", so false is the honest answer and the
    // host must not read the out-param.
    if gui_mode() == GuiMode::FixedSize {
        return false;
    }
    (*hints).can_resize_horizontally = true;
    (*hints).can_resize_vertically = true;
    (*hints).preserve_aspect_ratio = true;
    (*hints).aspect_ratio_width = GUI_ASPECT_W;
    (*hints).aspect_ratio_height = GUI_ASPECT_H;
    true
}

unsafe extern "C" fn gui_adjust_size(
    _plugin: *const clap_plugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    record(GUI_CALL_ADJUST_SIZE);
    if width.is_null() || height.is_null() {
        return false;
    }
    GUI.last_adjust_in_w.store(*width, Ordering::Release);
    GUI.last_adjust_in_h.store(*height, Ordering::Release);
    // CLAP: "Returns true if the plugin could adjust the given size." A
    // fixed-size editor cannot, and leaves the out-params untouched — exactly
    // the situation where forwarding them to `set_size` would be wrong.
    if gui_mode() == GuiMode::FixedSize {
        return false;
    }
    // Snap down to the quantum — "fits in the given size" is a floor, not a
    // round. Clamp to one quantum so a tiny request cannot snap to zero.
    *width = (*width / GUI_SIZE_QUANTUM).max(1) * GUI_SIZE_QUANTUM;
    *height = (*height / GUI_SIZE_QUANTUM).max(1) * GUI_SIZE_QUANTUM;
    true
}

unsafe extern "C" fn gui_set_size(_plugin: *const clap_plugin, width: u32, height: u32) -> bool {
    record(GUI_CALL_SET_SIZE);
    GUI.last_set_size_w.store(width, Ordering::Release);
    GUI.last_set_size_h.store(height, Ordering::Release);
    match gui_mode() {
        // A fixed-size editor accepts only its own size. This is what makes
        // "the host forwarded an unadjusted size" a *failure* rather than a
        // silently-accepted wrong answer.
        GuiMode::FixedSize => width == GUI_WIDTH && height == GUI_HEIGHT,
        // Elsewhere, accept anything on the snap grid and refuse anything off
        // it — the same distinction, applied to the resizable case.
        _ => width.is_multiple_of(GUI_SIZE_QUANTUM) && height.is_multiple_of(GUI_SIZE_QUANTUM),
    }
}

unsafe extern "C" fn gui_set_parent(
    _plugin: *const clap_plugin,
    window: *const clap_window,
) -> bool {
    record(GUI_CALL_SET_PARENT);
    if window.is_null() {
        return false;
    }
    GUI.set_parent_window_non_null
        .store(true, Ordering::Release);
    let api = (*window).api;
    let matches = !api.is_null() && CStr::from_ptr(api) == platform_api();
    GUI.set_parent_api_matches_platform
        .store(matches, Ordering::Release);
    // The handle itself is never dereferenced — see the module docs. Recording
    // that we were handed one is the whole job.
    matches
}

/// The floating counterpart to [`gui_set_parent`].
///
/// Returns `true` even for a mismatched api: `set_transient` is a *hint* the
/// plugin may act on or ignore, unlike `set_parent`, whose failure means the
/// window was never embedded. Answering `false` here would tell a host its
/// floating window is broken when it is merely unparented.
unsafe extern "C" fn gui_set_transient(
    _plugin: *const clap_plugin,
    window: *const clap_window,
) -> bool {
    record(GUI_CALL_SET_TRANSIENT);
    if window.is_null() {
        return false;
    }
    GUI.set_transient_window_non_null
        .store(true, Ordering::Release);
    true
}

/// Record the host's suggested window title.
///
/// Stored rather than counted so a test can tell an empty or garbage title from
/// no title at all — the same reason `set_parent` records the api string.
unsafe extern "C" fn gui_suggest_title(
    _plugin: *const clap_plugin,
    title: *const std::ffi::c_char,
) {
    record(GUI_CALL_SUGGEST_TITLE);
    if title.is_null() {
        return;
    }
    let bytes = CStr::from_ptr(title).to_bytes();
    // Leave the last cell as the NUL terminator, so a title at or over the
    // bound reads back truncated rather than unterminated.
    let n = bytes.len().min(MAX_TITLE_LEN - 1);
    for (slot, byte) in GUI.suggested_title.iter().zip(&bytes[..n]) {
        slot.store(*byte as u32, Ordering::Release);
    }
    GUI.suggested_title[n].store(0, Ordering::Release);
}

/// State a preference for the platform api, in whichever mode this probe
/// supports.
///
/// A [`GuiMode::FloatingOnly`] probe prefers floating and everything else
/// prefers embedded, so the preference always agrees with what
/// `is_api_supported` will answer. A probe that preferred a mode it then
/// refused would be testing the host against a plugin no host should humour.
unsafe extern "C" fn gui_get_preferred_api(
    _plugin: *const clap_plugin,
    api: *mut *const std::ffi::c_char,
    is_floating: *mut bool,
) -> bool {
    record(GUI_CALL_GET_PREFERRED_API);
    if api.is_null() || is_floating.is_null() {
        return false;
    }
    // Assigned as a pointer to the constant, never strcopied — `ext/gui.h:114`
    // requires exactly this, and a host is entitled to compare it by address.
    *api = platform_api().as_ptr();
    *is_floating = gui_mode() == GuiMode::FloatingOnly;
    true
}

unsafe extern "C" fn gui_show(plugin: *const clap_plugin) -> bool {
    record(GUI_CALL_SHOW);
    // The latched command runs here: `show` is the last call of the host's
    // embed sequence, so a plugin-initiated resize or close lands while the
    // host still believes the editor is live.
    run_pending_command(plugin);
    true
}

unsafe extern "C" fn gui_hide(_plugin: *const clap_plugin) -> bool {
    record(GUI_CALL_HIDE);
    true
}

/// The embeddable vtable — every fn present.
static GUI_FULL: clap_plugin_gui = clap_plugin_gui {
    is_api_supported: Some(gui_is_api_supported),
    get_preferred_api: Some(gui_get_preferred_api),
    create: Some(gui_create),
    destroy: Some(gui_destroy),
    set_scale: Some(gui_set_scale),
    get_size: Some(gui_get_size),
    can_resize: Some(gui_can_resize),
    get_resize_hints: Some(gui_get_resize_hints),
    adjust_size: Some(gui_adjust_size),
    set_size: Some(gui_set_size),
    set_parent: Some(gui_set_parent),
    set_transient: Some(gui_set_transient),
    suggest_title: Some(gui_suggest_title),
    show: Some(gui_show),
    hide: Some(gui_hide),
};

/// The [`GuiMode::NoCreate`] vtable — identical but for the absent `create`.
///
/// A separate `static` rather than a runtime branch, because the shape under
/// test is precisely "the fn pointer is `None`". A `create` that exists and
/// returns false is a *different* plugin.
static GUI_NO_CREATE: clap_plugin_gui = clap_plugin_gui {
    create: None,
    ..GUI_FULL
};

/// The GUI extension this probe implements, for `plugin_get_extension` to fall
/// through to. Returns null for anything else — and for [`GuiMode::Absent`],
/// which is how the probe presents a plugin with no editor at all.
///
/// # Safety
/// `id` must be a valid nul-terminated C string.
pub unsafe fn get_extension(id: &CStr) -> *const c_void {
    if id != CLAP_EXT_GUI {
        return ptr::null();
    }
    match gui_mode() {
        GuiMode::Absent => ptr::null(),
        GuiMode::NoCreate => &GUI_NO_CREATE as *const _ as *const c_void,
        _ => &GUI_FULL as *const _ as *const c_void,
    }
}
