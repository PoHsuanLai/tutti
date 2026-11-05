//! The plugin's editor surface.

use super::*;

impl Vst3Loaded {
    /// Returns `true` if the plugin actually publishes an editor view.
    ///
    /// **Asks `createView`, not `controller.is_some()`.** Those are different
    /// questions: nearly every VST3 has an edit controller — that is where
    /// parameters live — while only some also publish a UI. A host that shows
    /// an "open editor" button from this answer needs the stronger question.
    ///
    /// The view is created and immediately released — the same
    /// `createView(kEditor)` the SDK's own `editorhost` uses to decide there is
    /// a UI (`editorhost.cpp:207`). That costs a plugin-side allocation per
    /// call, so callers needing it per-frame should cache it; the DAW asks
    /// once, at scan time.
    ///
    /// A plugin with no controller at all answers `false`.
    #[must_use]
    pub fn has_editor(&self) -> bool {
        self.plugin.has_editor()
    }

    /// Creates the plugin editor, attach it to `parent`, and return its initial
    /// pixel size. Only one editor may be open at a time per instance —
    /// opening a second replaces the first.
    ///
    /// `scale` is `parent`'s backing-store scale. On a physical-pixel window
    /// system (Win32, X11) it is sent through
    /// `IPlugViewContentScaleSupport::setContentScaleFactor` before
    /// `attached`, for a view that implements it; on macOS the view reads it
    /// from its own `NSView`, and it is not sent (see [`ContentScale`]).
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::EditorError`] if
    /// the plugin has no controller, refuses to create a view, or the view
    /// rejects this platform's window type, and
    /// [`Vst3Error::PluginError`] if
    /// `IPlugView::attached` fails.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn open_editor(&mut self, parent: WindowHandle, scale: ContentScale) -> Result<EditorSize> {
        tutti_plugin_types::assert_main_thread();
        let size = self
            .plugin
            .open_editor(parent, scale)
            .map_err(|error| match error {
                EditorError::NoController => {
                    Vst3Error::EditorError("Plugin has no editor controller".to_string())
                }
                EditorError::NoView => {
                    Vst3Error::EditorError("Failed to create plugin view".to_string())
                }
                EditorError::PlatformRefused(name) => Vst3Error::EditorError(format!(
                    "Plugin view does not support platform type {name}"
                )),
                EditorError::Attach(e) => Vst3Error::PluginError {
                    stage: LoadStage::Initialization,
                    code: e.code(),
                },
                _ => Vst3Error::EditorError("Failed to open the plugin editor".to_string()),
            })?;

        let (width, height) = size
            .map(|size| (size.width, size.height))
            .unwrap_or(DEFAULT_EDITOR_SIZE);
        Ok(EditorSize { width, height })
    }

    /// Closes the editor if open, calling `IPlugView::removed`. No-op otherwise.
    /// Called automatically on `Drop`.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn close_editor(&mut self) {
        tutti_plugin_types::assert_main_thread();
        self.plugin.close_editor();
    }

    /// What the currently open editor supports, queried live from the view.
    ///
    /// Returns the all-`false` default when no editor is open — a closed editor
    /// reports no capabilities rather than the ones it had while open.
    pub fn editor_capabilities(&self) -> EditorCapabilities {
        self.plugin.editor_capabilities()
    }

    /// Runs one iteration of the event loop this host lends the plugin
    /// (`Linux::IRunLoop`): fire any timers that came due and dispatch any
    /// plugin file descriptor that became readable.
    ///
    /// **The embedder must call this from its UI thread, every frame, for as
    /// long as any editor of this plugin is open.** X11 has no ambient run loop
    /// the way Cocoa and Win32 do, so `iplugview.h` makes the loop the *host's*
    /// job: "the host has to call the event handler when the file descriptor is
    /// marked readable", and a registered timer "will be called repeatedly until
    /// it is unregistered". Nothing else drives them. Skip this and the editor
    /// opens, paints once, and then freezes — no redraws, no animation, no
    /// response to input.
    ///
    /// Cheap and non-blocking: the `poll` uses a zero timeout, so calling it on
    /// a frame where nothing is ready costs one syscall and returns. That is why
    /// it is safe to call unconditionally from a render loop.
    ///
    /// # Why this is not folded into `poll_plugin_notifications`
    ///
    /// The two look alike but run on different clocks, and merging them would
    /// break one or the other.
    /// [`poll_plugin_notifications`](Self::poll_plugin_notifications) drains
    /// queues that this host filled; the work is already done and arriving late
    /// only delays a UI update. This call *is* the plugin's event loop — its
    /// cadence sets the editor's frame rate, and a host that polls
    /// notifications a few times a second (perfectly adequate for parameter
    /// echoes) would render such an editor unusable. Equally, a host with no
    /// editor open should not be forced to pump a loop with nothing in it.
    /// Keeping them separate lets each be called at the rate it actually needs.
    ///
    /// # Why the host and not a timer thread
    ///
    /// A background thread ticking this would be simpler for the embedder and
    /// is wrong: the handlers are plugin GUI code reaching into its X
    /// connection, and the spec's whole premise is that the host donates *its
    /// UI thread*. Calling them from anywhere else is the same data race as
    /// touching any other toolkit off-thread. So the obligation is the
    /// embedder's, and this method is the seam — it deliberately cannot be
    /// automated away from inside a library that owns no event loop.
    ///
    /// No-op on non-Linux targets, where the OS provides the run loop, so
    /// calling it unconditionally is portable.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn run_editor_loop_iteration(&mut self) {
        tutti_plugin_types::assert_main_thread();
        self.plugin.run_editor_loop_iteration();
    }

    /// What the plugin has registered with this host's run loop, and how much
    /// the host has dispatched. Test-only observation seam behind the
    /// `conformance` feature — see [`RunLoopActivity`](crate::RunLoopActivity).
    ///
    /// All-zero off Linux, where the OS owns the run loop and plugins register
    /// nothing with the host, so callers need no `cfg` of their own.
    #[cfg(feature = "conformance")]
    pub fn run_loop_activity(&self) -> crate::RunLoopActivity {
        self.plugin.run_loop_activity()
    }

    /// The latest `IPlugFrame::resizeView` request since the last poll, if
    /// any. Earlier requests in between are superseded, not queued: only the
    /// size the plugin wants now matters.
    pub fn poll_editor_resize_request(&mut self) -> Option<EditorSize> {
        self.plugin.take_editor_resize_request()
    }

    /// Asks the plugin what it would snap `requested` to, without applying it.
    ///
    /// A read-only probe of `IPlugView::checkSizeConstraint`: the plugin clamps
    /// the rect to the nearest size it accepts, and this reports that size
    /// without the `onSize` that [`resize_editor`](Self::resize_editor) would
    /// follow with. Returns `None` when no editor is open.
    ///
    /// Exposed for the conformance suite, which uses it to check that a size
    /// the host granted is genuinely one the plugin accepts.
    #[cfg(feature = "conformance")]
    pub fn check_editor_size_constraint(&self, requested: EditorSize) -> Option<EditorSize> {
        self.plugin.editor_size_constraint(requested)
    }

    /// Offer a key press to the open editor. Returns `true` if the plugin
    /// **consumed** it, in which case the host must not act on it too.
    ///
    /// # The two key codes
    ///
    /// VST3 carries a character *and* a virtual key, and a caller supplies one
    /// while zeroing the other. `key` is the UTF-16 character the keystroke
    /// produced; `virtual_key` is a `VirtualKeyCodes` value for keystrokes that
    /// produce no character (arrows, function keys). Sending both, or neither,
    /// is what plugins disagree about handling — pick one.
    ///
    /// `modifiers` is an OR of `KeyModifier` bits. Note the SDK's own
    /// convention is platform-dependent: `kCommandKey` is Cmd on macOS and
    /// Ctrl on Windows and Linux, so a caller mapping from a neutral modifier
    /// set has to know which platform it is on.
    ///
    /// # Arbitration
    ///
    /// Honour the return value rather than filtering keys before the call. A
    /// host that reserves keys for itself — space for transport, say — takes
    /// them from every plugin that legitimately uses them, and cannot know
    /// which those are. The spec's answer is this bool, and it applies only
    /// while the editor holds focus.
    ///
    /// Returns `false` if no editor is open. Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    #[must_use]
    pub fn send_key_down(&self, key: u16, virtual_key: i16, modifiers: i16) -> bool {
        tutti_plugin_types::assert_main_thread();
        self.plugin.editor_key_down(key, virtual_key, modifiers)
    }

    /// Offer a key release to the open editor. See
    /// [`send_key_down`](Self::send_key_down) — identical contract, and a host
    /// that forwards one without the other leaves plugins holding keys down.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    #[must_use]
    pub fn send_key_up(&self, key: u16, virtual_key: i16, modifiers: i16) -> bool {
        tutti_plugin_types::assert_main_thread();
        self.plugin.editor_key_up(key, virtual_key, modifiers)
    }

    /// Offer a mouse-wheel movement to the open editor, `distance` in wheel
    /// notches. Returns `true` if the plugin consumed it.
    ///
    /// `IPlugView::onWheel` takes a single scalar, so horizontal scroll and
    /// modifier-qualified scroll have nowhere to go — a caller with either must
    /// handle it host-side. Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    #[must_use]
    pub fn send_wheel(&self, distance: f32) -> bool {
        tutti_plugin_types::assert_main_thread();
        self.plugin.editor_wheel(distance)
    }

    /// Tells the open editor whether it now has keyboard focus.
    ///
    /// Plugins use this to draw a focus ring and to arm their own key
    /// handling; one that never hears it may ignore keys the host forwards.
    /// A notification, not a request — there is no consumed/refused answer.
    /// No-op if no editor is open. Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn set_editor_focus(&self, focused: bool) {
        tutti_plugin_types::assert_main_thread();
        self.plugin.set_editor_focus(focused);
    }

    /// Resizes the open editor and returns the size the plugin applied.
    ///
    /// The request is passed through the view's `checkSizeConstraint` first, so
    /// the returned size can differ from `requested` (aspect-ratio locks,
    /// min/max, size grids). Call from the main/UI thread.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::EditorError`] if no editor is open, or
    /// [`Vst3Error::PluginError`] if the view rejects `onSize`.
    pub fn resize_editor(&mut self, requested: EditorSize) -> Result<EditorSize> {
        if !self.plugin.editor_open() {
            return Err(Vst3Error::EditorError("editor not open".to_string()));
        }
        self.plugin
            .resize_editor(requested)
            .map_err(|e| Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: e.code(),
            })
    }
}

pub(super) const DEFAULT_EDITOR_SIZE: (u32, u32) = (800, 600);
