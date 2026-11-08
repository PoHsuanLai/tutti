//! `clap.gui`: the editor entries, as the sequences a host runs them in.

use std::ffi::{CStr, c_char};
use std::fmt;

use clap_sys::ext::gui::{
    CLAP_WINDOW_API_COCOA, clap_gui_resize_hints, clap_plugin_gui, clap_window, clap_window_handle,
};
use clap_sys::plugin::clap_plugin;
use tutti_plugin_types::{MainThread, WindowHandle};

use crate::plugin::Plugin;

/// A `clap.gui` step that did not succeed. `Display` is the text a host shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuiError {
    /// The plugin has no `clap.gui`.
    NoExtension,
    /// The plugin does not support this host's window API (embedded).
    EmbeddedUnsupported,
    /// The plugin does not support this host's window API (floating).
    FloatingUnsupported,
    /// `create` answered `false` (embedded).
    CreateFailed,
    /// `create` answered `false` (floating).
    CreateFloatingFailed,
    /// `set_parent` answered `false`.
    SetParentFailed,
    /// `adjust_size` found no usable size.
    AdjustRefused,
    /// The plugin has no `set_size`.
    SetSizeMissing,
    /// `set_size` answered `false`.
    SetSizeRefused,
    /// An entry that CLAP orders after `create` was asked for before it (or
    /// after `destroy`); the plugin was not called.
    NotCreated,
    /// `create` while the previous editor is still created; the plugin was not
    /// called.
    AlreadyCreated,
}

impl fmt::Display for GuiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoExtension => "No GUI extension",
            Self::EmbeddedUnsupported => "GUI embedded window API not supported",
            Self::FloatingUnsupported => "GUI floating window API not supported",
            Self::CreateFailed => "GUI create failed",
            Self::CreateFloatingFailed => "GUI create (floating) failed",
            Self::SetParentFailed => "Set parent failed",
            Self::AdjustRefused => "adjust_size: no usable size fits the request",
            Self::SetSizeMissing => "set_size unsupported",
            Self::SetSizeRefused => "set_size refused",
            Self::NotCreated => "the editor has not been created",
            Self::AlreadyCreated => "the editor is already created",
        })
    }
}

impl std::error::Error for GuiError {}

/// What opening an embedded editor produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbedOutcome {
    /// The editor's width.
    pub width: u32,
    /// The editor's height.
    pub height: u32,
    /// Whether the plugin has a `create` entry and it ran, so the editor
    /// exists and must be destroyed later.
    pub did_create: bool,
}

/// What the plugin said about resizing its editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuiHints {
    /// `can_resize`.
    pub resizable: bool,
    /// Whether the width can change.
    pub can_resize_horizontally: bool,
    /// Whether the height can change.
    pub can_resize_vertically: bool,
    /// Whether the aspect ratio is to be kept.
    pub preserve_aspect_ratio: bool,
    /// The aspect ratio (width, height) when the plugin gave a usable one.
    pub aspect_ratio: Option<(u32, u32)>,
}

#[cfg(target_os = "macos")]
fn platform_window_handle(parent: *mut std::ffi::c_void) -> (*const c_char, clap_window_handle) {
    (
        CLAP_WINDOW_API_COCOA.as_ptr(),
        clap_window_handle { cocoa: parent },
    )
}

#[cfg(target_os = "windows")]
fn platform_window_handle(parent: *mut std::ffi::c_void) -> (*const c_char, clap_window_handle) {
    use clap_sys::ext::gui::CLAP_WINDOW_API_WIN32;
    (
        CLAP_WINDOW_API_WIN32.as_ptr(),
        clap_window_handle { win32: parent },
    )
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_window_handle(parent: *mut std::ffi::c_void) -> (*const c_char, clap_window_handle) {
    use clap_sys::ext::gui::CLAP_WINDOW_API_X11;
    (
        CLAP_WINDOW_API_X11.as_ptr(),
        clap_window_handle { x11: parent as u64 },
    )
}

/// Whether `api` names a window API whose sizes are in logical pixels, where
/// the plugin scales itself (`gui.h`: Cocoa and UIKit).
fn api_uses_logical_pixels(api: *const c_char) -> bool {
    if api.is_null() {
        return false;
    }
    // SAFETY: `api` is one of this module's `CLAP_WINDOW_API_*` constants, a
    // `'static` nul-terminated string.
    let api = unsafe { CStr::from_ptr(api) };
    api == CLAP_WINDOW_API_COCOA || api.to_bytes() == b"uikit"
}

/// Creates, scales, sizes, parents and shows an embedded editor.
///
/// # Safety
/// `plugin` is a live instance `gui` is the `clap.gui` of, called on its main
/// thread; `window` is a valid parent for `api`.
unsafe fn embed_editor_sequence(
    gui: &clap_plugin_gui,
    plugin: *const clap_plugin,
    api: *const c_char,
    window: clap_window_handle,
    scale: f64,
) -> Result<EmbedOutcome, GuiError> {
    // SAFETY (every call below): the plugin's own `clap.gui` entries, with the
    // instance, `api` and values built here; `[main-thread]` entries (the
    // contract).
    if let Some(f) = gui.is_api_supported
        // SAFETY: the plugin's own `clap.gui` entry, with the instance and values built here, under this function's contract (a live instance, main thread).
        && !unsafe { f(plugin, api, false) }
    {
        return Err(GuiError::EmbeddedUnsupported);
    }
    let did_create = match gui.create {
        Some(create) => {
            // SAFETY: the plugin's own `clap.gui` entry, with the instance and values built here, under this function's contract (a live instance, main thread).
            if !unsafe { create(plugin, api, false) } {
                return Err(GuiError::CreateFailed);
            }
            true
        }
        None => false,
    };
    // A logical-pixel API (Cocoa, UIKit) scales itself; `set_scale` there
    // would double-scale (`ext/gui.h`).
    let set_scale = if api_uses_logical_pixels(api) {
        None
    } else {
        gui.set_scale
    };
    if let Some(f) = set_scale {
        // SAFETY: the plugin's own `clap.gui` entry, with the instance and values built here, under this function's contract (a live instance, main thread).
        unsafe { f(plugin, scale) };
    }
    let (width, height) = match gui.get_size {
        Some(f) => {
            let (mut w, mut h) = (0_u32, 0_u32);
            // SAFETY: the plugin's own `clap.gui` entry, with the instance and values built here, under this function's contract (a live instance, main thread).
            if unsafe { f(plugin, &mut w, &mut h) } {
                (w, h)
            } else {
                (800, 600)
            }
        }
        None => (800, 600),
    };
    if let Some(f) = gui.set_parent {
        let window = clap_window {
            api,
            specific: window,
        };
        // SAFETY: the plugin's own `clap.gui` entry, with the instance and values built here, under this function's contract (a live instance, main thread).
        if !unsafe { f(plugin, &window) } {
            return Err(GuiError::SetParentFailed);
        }
    }
    if let Some(f) = gui.show {
        // SAFETY: the plugin's own `clap.gui` entry, with the instance and values built here, under this function's contract (a live instance, main thread).
        unsafe { f(plugin) };
    }
    Ok(EmbedOutcome {
        width,
        height,
        did_create,
    })
}

/// Creates a floating editor, hints a transient parent and a title, shows it.
///
/// # Safety
/// As [`embed_editor_sequence`]; `title` is nul-terminated.
unsafe fn floating_editor_sequence(
    gui: &clap_plugin_gui,
    plugin: *const clap_plugin,
    api: *const c_char,
    transient: Option<clap_window_handle>,
    title: &CStr,
) -> Result<bool, GuiError> {
    // SAFETY (every call below): as `embed_editor_sequence`.
    if let Some(f) = gui.is_api_supported
        // SAFETY: the plugin's own `clap.gui` entry, with the instance and values built here, under this function's contract (a live instance, main thread).
        && !unsafe { f(plugin, api, true) }
    {
        return Err(GuiError::FloatingUnsupported);
    }
    let did_create = match gui.create {
        Some(create) => {
            // SAFETY: the plugin's own `clap.gui` entry, with the instance and values built here, under this function's contract (a live instance, main thread).
            if !unsafe { create(plugin, api, true) } {
                return Err(GuiError::CreateFloatingFailed);
            }
            true
        }
        None => false,
    };
    if let (Some(f), Some(handle)) = (gui.set_transient, transient) {
        let window = clap_window {
            api,
            specific: handle,
        };
        // SAFETY: the plugin's own `clap.gui` entry, with the instance and values built here, under this function's contract (a live instance, main thread).
        unsafe { f(plugin, &window) };
    }
    if let Some(f) = gui.suggest_title {
        // SAFETY: the plugin's own `clap.gui` entry, with the instance and values built here, under this function's contract (a live instance, main thread).
        unsafe { f(plugin, title.as_ptr()) };
    }
    if let Some(f) = gui.show {
        // SAFETY: the plugin's own `clap.gui` entry, with the instance and values built here, under this function's contract (a live instance, main thread).
        unsafe { f(plugin) };
    }
    Ok(did_create)
}

/// `can_resize` and `get_resize_hints`.
///
/// # Safety
/// As [`embed_editor_sequence`], after the editor was created.
unsafe fn query_editor_capabilities(gui: &clap_plugin_gui, plugin: *const clap_plugin) -> GuiHints {
    // SAFETY: the plugin's own entries, as above.
    let resizable = gui.can_resize.is_some_and(|f| unsafe { f(plugin) });
    let mut hints = GuiHints {
        resizable,
        can_resize_horizontally: resizable,
        can_resize_vertically: resizable,
        preserve_aspect_ratio: false,
        aspect_ratio: None,
    };
    if let Some(f) = gui.get_resize_hints {
        let mut raw = clap_gui_resize_hints {
            can_resize_horizontally: false,
            can_resize_vertically: false,
            preserve_aspect_ratio: false,
            aspect_ratio_width: 0,
            aspect_ratio_height: 0,
        };
        // SAFETY: the plugin's own entry with a writable out-struct.
        if unsafe { f(plugin, &mut raw) } {
            hints.can_resize_horizontally = raw.can_resize_horizontally;
            hints.can_resize_vertically = raw.can_resize_vertically;
            hints.preserve_aspect_ratio = raw.preserve_aspect_ratio;
            if raw.preserve_aspect_ratio
                && raw.aspect_ratio_width > 0
                && raw.aspect_ratio_height > 0
            {
                hints.aspect_ratio = Some((raw.aspect_ratio_width, raw.aspect_ratio_height));
            }
        }
    }
    hints
}

impl Plugin {
    fn gui(&self) -> Result<&clap_plugin_gui, GuiError> {
        self.gui_ext().ok_or(GuiError::NoExtension)
    }

    /// Whether the plugin can show an embedded editor on this platform's
    /// window API.
    #[must_use]
    pub fn has_editor(&self, _main: &MainThread) -> bool {
        self.has_gui_api(false)
    }

    /// Whether the plugin can show a floating editor on this platform's
    /// window API.
    #[must_use]
    pub fn has_floating_editor(&self, _main: &MainThread) -> bool {
        self.has_gui_api(true)
    }

    fn has_gui_api(&self, floating: bool) -> bool {
        let Some(gui) = self.gui_ext() else {
            return false;
        };
        if gui.create.is_none() {
            return false;
        }
        let (api, _) = platform_window_handle(std::ptr::null_mut());
        match gui.is_api_supported {
            // SAFETY: the plugin's own `is_api_supported` with the instance
            // and this platform's API constant.
            Some(f) => unsafe { f(self.raw(), api, floating) },
            None => true,
        }
    }

    /// `gui.get_preferred_api`: whether the plugin prefers floating, if it
    /// states a preference.
    #[must_use]
    pub fn prefers_floating(&self, _main: &MainThread) -> Option<bool> {
        let get = self.gui_ext()?.get_preferred_api?;
        let mut api: *const c_char = std::ptr::null();
        let mut floating = false;
        // SAFETY: the plugin's own entry with writable out-parameters; `api`
        // is not read.
        let stated = unsafe { get(self.raw(), &mut api, &mut floating) };
        stated.then_some(floating)
    }

    /// Creates, scales, sizes, parents and shows an embedded editor in
    /// `parent`. `parent`'s validity is what its unsafe constructor claimed.
    ///
    /// # Errors
    /// [`GuiError::AlreadyCreated`] over a live editor, else the first step
    /// the plugin refuses. An editor `create`d before a refused `set_parent`
    /// is destroyed before the error returns.
    pub fn open_embedded(
        &mut self,
        _main: &MainThread,
        parent: &WindowHandle,
        scale: f64,
    ) -> Result<EmbedOutcome, GuiError> {
        let gui = self.gui()?;
        if self.gui_created {
            return Err(GuiError::AlreadyCreated);
        }
        let (api, handle) = platform_window_handle(parent.as_ptr());
        // SAFETY: `&mut self` is the main-thread half of a live instance, `gui`
        // is its `clap.gui`, and `parent` is a valid window by its constructor.
        let outcome = unsafe { embed_editor_sequence(gui, self.raw(), api, handle, scale) };
        match outcome {
            Ok(done) => self.gui_created = done.did_create,
            // `create` ran and `set_parent` refused: the editor exists but
            // will not be shown, so it is destroyed here rather than left for
            // a caller to know.
            Err(GuiError::SetParentFailed) => {
                if let Some(destroy) = gui.destroy {
                    // SAFETY: the plugin's own `destroy` after its `create`.
                    unsafe { destroy(self.raw()) };
                }
            }
            Err(_) => {}
        }
        outcome
    }

    /// Creates a floating editor, hints a transient parent and a title, and
    /// shows it. Returns whether `create` ran.
    ///
    /// # Errors
    /// [`GuiError::AlreadyCreated`] over a live editor, else the first step
    /// the plugin refuses.
    pub fn open_floating(
        &mut self,
        _main: &MainThread,
        transient: Option<&WindowHandle>,
        title: &CStr,
    ) -> Result<bool, GuiError> {
        let gui = self.gui()?;
        if self.gui_created {
            return Err(GuiError::AlreadyCreated);
        }
        let (api, _) = platform_window_handle(std::ptr::null_mut());
        let transient = transient.map(|w| platform_window_handle(w.as_ptr()).1);
        // SAFETY: as `open_embedded`; `title` is nul-terminated.
        let created = unsafe { floating_editor_sequence(gui, self.raw(), api, transient, title) }?;
        self.gui_created = created;
        Ok(created)
    }

    /// `can_resize` and `get_resize_hints`, `None` without a `clap.gui` or
    /// before `create` (CLAP orders every `clap.gui` method after it).
    #[must_use]
    pub fn gui_hints(&self, _main: &MainThread) -> Option<GuiHints> {
        let gui = self.gui_ext().filter(|_| self.gui_created)?;
        // SAFETY: the instance is live and `gui` is its `clap.gui`; a
        // `[main-thread]` call on the main-thread half.
        Some(unsafe { query_editor_capabilities(gui, self.raw()) })
    }

    /// Asks the plugin to adjust `width` by `height` to a size it can take,
    /// then sets it. Returns the size set.
    ///
    /// # Errors
    /// No `clap.gui`, no editor created, no usable adjusted size, no
    /// `set_size`, or a refused `set_size`.
    pub fn resize_gui(
        &mut self,
        _main: &MainThread,
        width: u32,
        height: u32,
    ) -> Result<(u32, u32), GuiError> {
        let gui = self.gui()?;
        if !self.gui_created {
            return Err(GuiError::NotCreated);
        }
        let plugin = self.raw();
        let (mut w, mut h) = (width, height);
        // SAFETY: the plugin's own entries with writable in/out sizes.
        if let Some(adjust) = gui.adjust_size
            // SAFETY: the plugin's own `clap.gui` entry, with the instance and values built here, under this function's contract (a live instance, main thread).
            && !unsafe { adjust(plugin, &mut w, &mut h) }
        {
            return Err(GuiError::AdjustRefused);
        }
        let set_size = gui.set_size.ok_or(GuiError::SetSizeMissing)?;
        // SAFETY: as above.
        if !unsafe { set_size(plugin, w, h) } {
            return Err(GuiError::SetSizeRefused);
        }
        Ok((w, h))
    }

    /// Hides the editor (unless its window is already gone) and destroys it. A
    /// no-op before `create` and after `destroy`, so `destroy` runs once.
    pub fn close_gui(&mut self, _main: &MainThread, window_destroyed: bool) {
        self.destroy_gui(window_destroyed);
    }

    /// [`close_gui`](Self::close_gui) without a token, for `Drop`, which checks
    /// the thread itself.
    pub(crate) fn destroy_gui(&mut self, window_destroyed: bool) {
        if !self.gui_created {
            return;
        }
        self.gui_created = false;
        let Some(gui) = self.gui_ext() else { return };
        let plugin = self.raw();
        if let (false, Some(hide)) = (window_destroyed, gui.hide) {
            // SAFETY: the plugin's own `hide`, as `resize_gui`.
            unsafe { hide(plugin) };
        }
        if let Some(destroy) = gui.destroy {
            // SAFETY: the plugin's own `destroy`, as above.
            unsafe { destroy(plugin) };
        }
    }
}

#[cfg(test)]
mod embed_sequence_tests {
    use super::*;

    // The stub gui vtable's fns log their own name into a `Vec<&str>` reached
    // through the stub plugin's `plugin_data`, so the test can assert the
    // CLAP-mandated call order.
    unsafe fn log(plugin: *const clap_plugin, name: &'static str) {
        // SAFETY: `plugin` is the `clap_plugin` pointer the stub was called with, live for this
        // call.
        let vec = unsafe { &mut *((*plugin).plugin_data as *mut Vec<&'static str>) };
        vec.push(name);
    }

    unsafe extern "C" fn stub_is_api_supported(
        plugin: *const clap_plugin,
        _api: *const c_char,
        _is_floating: bool,
    ) -> bool {
        // SAFETY: `plugin` is the test's own `clap_plugin`, handed back unchanged by the gui
        // call under test; its `plugin_data` is the test's call log.
        unsafe { log(plugin, "is_api_supported") };
        true
    }

    unsafe extern "C" fn stub_create(
        plugin: *const clap_plugin,
        _api: *const c_char,
        _is_floating: bool,
    ) -> bool {
        // SAFETY: `plugin` is the test's own `clap_plugin`, handed back unchanged by the gui
        // call under test; its `plugin_data` is the test's call log.
        unsafe { log(plugin, "create") };
        true
    }

    unsafe extern "C" fn stub_set_scale(plugin: *const clap_plugin, _scale: f64) -> bool {
        // SAFETY: `plugin` is the test's own `clap_plugin`, handed back unchanged by the gui
        // call under test; its `plugin_data` is the test's call log.
        unsafe { log(plugin, "set_scale") };
        true
    }

    unsafe extern "C" fn stub_get_size(
        plugin: *const clap_plugin,
        width: *mut u32,
        height: *mut u32,
    ) -> bool {
        // SAFETY: `plugin` is the test's own `clap_plugin`, handed back unchanged by the gui
        // call under test; its `plugin_data` is the test's call log.
        unsafe { log(plugin, "get_size") };
        // SAFETY: the gui call under test passes out-pointers to live `u32`s.
        unsafe {
            *width = 640;
            *height = 480;
        }
        true
    }

    unsafe extern "C" fn stub_set_parent(
        plugin: *const clap_plugin,
        _window: *const clap_window,
    ) -> bool {
        // SAFETY: `plugin` is the test's own `clap_plugin`, handed back unchanged by the gui
        // call under test; its `plugin_data` is the test's call log.
        unsafe { log(plugin, "set_parent") };
        true
    }

    unsafe extern "C" fn stub_show(plugin: *const clap_plugin) -> bool {
        // SAFETY: `plugin` is the test's own `clap_plugin`, handed back unchanged by the gui
        // call under test; its `plugin_data` is the test's call log.
        unsafe { log(plugin, "show") };
        true
    }

    unsafe extern "C" fn stub_is_api_supported_false(
        plugin: *const clap_plugin,
        _api: *const c_char,
        _is_floating: bool,
    ) -> bool {
        // SAFETY: `plugin` is the test's own `clap_plugin`, handed back unchanged by the gui
        // call under test; its `plugin_data` is the test's call log.
        unsafe { log(plugin, "is_api_supported") };
        false
    }

    unsafe extern "C" fn stub_can_resize(plugin: *const clap_plugin) -> bool {
        // SAFETY: `plugin` is the test's own `clap_plugin`, handed back unchanged by the gui
        // call under test; its `plugin_data` is the test's call log.
        unsafe { log(plugin, "can_resize") };
        true
    }

    unsafe extern "C" fn stub_get_resize_hints(
        plugin: *const clap_plugin,
        hints: *mut clap_sys::ext::gui::clap_gui_resize_hints,
    ) -> bool {
        // SAFETY: `plugin` is the test's own `clap_plugin`, handed back unchanged by the gui
        // call under test; its `plugin_data` is the test's call log.
        unsafe { log(plugin, "get_resize_hints") };
        if !hints.is_null() {
            // SAFETY: `hints` is non-null and points to a live `clap_gui_resize_hints`.
            unsafe {
                (*hints).can_resize_horizontally = true;
                (*hints).can_resize_vertically = true;
            }
        }
        true
    }

    fn stub_gui() -> clap_plugin_gui {
        // SAFETY: clap_plugin_gui is all Option<fn ptr> fields; zeroed = None.
        let mut gui: clap_plugin_gui = unsafe { std::mem::zeroed() };
        gui.is_api_supported = Some(stub_is_api_supported);
        gui.create = Some(stub_create);
        gui.set_scale = Some(stub_set_scale);
        gui.get_size = Some(stub_get_size);
        gui.set_parent = Some(stub_set_parent);
        gui.show = Some(stub_show);
        gui.can_resize = Some(stub_can_resize);
        gui.get_resize_hints = Some(stub_get_resize_hints);
        gui
    }

    fn stub_plugin(log: &mut Vec<&'static str>) -> clap_plugin {
        // SAFETY: clap_plugin is POD (pointers + Option<fn>); zeroed is a valid
        // all-null/None instance. We only ever read `plugin_data`.
        let mut plugin: clap_plugin = unsafe { std::mem::zeroed() };
        plugin.plugin_data = log as *mut Vec<&'static str> as *mut std::ffi::c_void;
        plugin
    }

    /// Drives the embed sequence against `api` and return the call log.
    fn run_embed(api: &std::ffi::CStr) -> Vec<&'static str> {
        let mut order: Vec<&'static str> = Vec::new();
        let plugin = stub_plugin(&mut order);
        let gui = stub_gui();
        let handle = clap_window_handle {
            ptr: std::ptr::null_mut(),
        };

        // SAFETY: the stub entries under test; `plugin` is the stub below.
        let outcome = unsafe {
            embed_editor_sequence(
                &gui,
                &plugin as *const clap_plugin,
                api.as_ptr(),
                handle,
                2.0,
            )
        }
        .expect("embed sequence succeeds");

        assert!(outcome.did_create, "create ran");
        assert_eq!(outcome.width, 640);
        assert_eq!(outcome.height, 480);
        order
    }

    #[test]
    fn embed_sequence_calls_in_spec_order() {
        // win32 is a physical-pixel api, so this is the shape that includes
        // `set_scale`; the logical-pixel apis are covered below.
        let order = run_embed(clap_sys::ext::gui::CLAP_WINDOW_API_WIN32);

        // The exact CLAP embed order: is_api_supported → create → set_scale →
        // get_size → set_parent → show.
        assert_eq!(
            order,
            vec![
                "is_api_supported",
                "create",
                "set_scale",
                "get_size",
                "set_parent",
                "show",
            ]
        );
    }

    /// `ext/gui.h:56-57` on `cocoa`: "uses logical size, don't call
    /// clap_plugin_gui->set_scale()". The api has already applied the display's
    /// backing-scale factor to every coordinate, so a host that also sets it
    /// makes a Retina editor open at twice its size.
    #[test]
    fn embed_sequence_skips_set_scale_on_cocoa() {
        let order = run_embed(clap_sys::ext::gui::CLAP_WINDOW_API_COCOA);

        assert!(
            !order.contains(&"set_scale"),
            "cocoa reports logical pixels; setting a scale on top of it applies \
             the factor twice. Sequence was {order:?}"
        );
        assert_eq!(
            order,
            vec![
                "is_api_supported",
                "create",
                "get_size",
                "set_parent",
                "show",
            ],
            "and the rest of the sequence is unchanged — only the one call drops"
        );
    }

    /// `uikit` carries the identical annotation (`ext/gui.h:59-60`) and has no
    /// clap-sys constant, so it is matched by literal. Asserted separately
    /// because a check written against the `CLAP_WINDOW_API_COCOA` constant
    /// alone leaves it out.
    #[test]
    fn embed_sequence_skips_set_scale_on_uikit() {
        let order = run_embed(c"uikit");

        assert!(
            !order.contains(&"set_scale"),
            "uikit reports logical pixels, same as cocoa. Sequence was {order:?}"
        );
    }

    /// An api string the host cannot classify takes the `set_scale` path. Both
    /// logical-pixel apis are named explicitly, so anything else is a
    /// physical-pixel one — and the alternative reading would silently drop the
    /// call on win32/x11, where the plugin needs it.
    #[test]
    fn embed_sequence_sets_scale_for_an_unrecognized_api() {
        let order = run_embed(c"some-future-windowing-api");

        assert!(
            order.contains(&"set_scale"),
            "an unclassifiable api defaults to the physical-pixel path. \
             Sequence was {order:?}"
        );
    }

    #[test]
    fn embed_sequence_rejects_unsupported_api() {
        // is_api_supported == false must short-circuit before create — a
        // floating-only plugin degrades gracefully instead of being embedded.
        let mut order: Vec<&'static str> = Vec::new();
        let plugin = stub_plugin(&mut order);
        let mut gui = stub_gui();
        gui.is_api_supported = Some(stub_is_api_supported_false);
        let handle = clap_window_handle {
            ptr: std::ptr::null_mut(),
        };

        // SAFETY: as above.
        let result = unsafe {
            embed_editor_sequence(
                &gui,
                &plugin as *const clap_plugin,
                std::ptr::null(),
                handle,
                1.0,
            )
        };

        assert!(result.is_err(), "unsupported api errors");
        assert_eq!(order, vec!["is_api_supported"], "stops before create");
    }

    /// Querying capabilities on a created editor calls the vtable; the
    /// `gui_created` gate in `editor_capabilities` is what keeps it from happening
    /// before that.
    ///
    /// The hazard is a missing precondition, not a wrong call sequence: the
    /// vtable calls below are correct *once `create()` has run*. CLAP orders
    /// every `clap_plugin_gui` method after `create`, and plugins check —
    /// TAL-Reverb-4 prints `[clap-plugin HOST-MISBEHAVING]
    /// clap_plugin_gui.can_resize() was called without a prior call to
    /// clap_plugin_gui.create()` if the guard tests only the extension pointer.
    #[test]
    fn capability_query_reads_both_gui_fns() {
        let mut order: Vec<&'static str> = Vec::new();
        let plugin = stub_plugin(&mut order);
        let gui = stub_gui();

        // SAFETY: as above: the stub gui and plugin.
        let caps = unsafe { query_editor_capabilities(&gui, &plugin as *const clap_plugin) };

        assert_eq!(
            order,
            vec!["can_resize", "get_resize_hints"],
            "hints must refine can_resize, not replace it"
        );
        assert!(caps.resizable);
        assert!(caps.can_resize_horizontally);
        assert!(caps.can_resize_vertically);
    }

    /// A plugin advertising `gui` but exposing neither resize fn is reported as
    /// non-resizable rather than defaulting to resizable — the safe direction, since
    /// a host that resizes a fixed-size editor corrupts its layout.
    #[test]
    fn capability_query_defaults_to_not_resizable() {
        let mut order: Vec<&'static str> = Vec::new();
        let plugin = stub_plugin(&mut order);
        // SAFETY: all-Option fields; zeroed = every fn absent.
        let gui: clap_plugin_gui = unsafe { std::mem::zeroed() };

        // SAFETY: as above.
        let caps = unsafe { query_editor_capabilities(&gui, &plugin as *const clap_plugin) };

        assert!(order.is_empty(), "nothing to call");
        assert!(!caps.resizable);
        assert!(!caps.preserve_aspect_ratio);
        assert!(caps.aspect_ratio.is_none());
    }
}
