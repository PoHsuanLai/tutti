//! The plugin's editor: opening, closing, sizing and input.

use super::*;

impl Plugin {
    // ── The editor ───────────────────────────────────────────────────────────

    pub(super) fn create_view(&self) -> Option<ComPtr<IPlugView>> {
        let ctrl = self.controller.get()?;
        // SAFETY: `c"editor"` is the NUL-terminated view name the SDK defines;
        // the result is a +1 reference to the view, or null.
        let view = unsafe { ctrl.createView(c"editor".as_ptr()) };
        // SAFETY: a non-null result is that +1 reference, which `ComPtr` adopts.
        unsafe { ComPtr::from_raw(view) }
    }

    /// Whether the plugin offers an editor view.
    #[must_use]
    pub fn has_editor(&self) -> bool {
        self.create_view().is_some()
    }

    /// Creates the editor view, attaches it to `parent` and returns its size,
    /// or `None` if the view will not say.
    ///
    /// # Errors
    ///
    /// [`EditorError`] for a plugin without a controller or a view, a platform
    /// the view refuses, or an `attached` that fails.
    pub fn open_editor(
        &mut self,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> Result<Option<EditorSize>, EditorError> {
        if self.controller.get().is_none() {
            return Err(EditorError::NoController);
        }
        let view = self.create_view().ok_or(EditorError::NoView)?;
        let frame = HostPlugFrame::new(
            #[cfg(target_os = "linux")]
            self.module.run_loop(),
        );
        let (editor, size) = Editor::open(view, parent, scale, frame)?;
        self.editor = Some(editor);
        Ok(size)
    }

    /// Detaches and releases the editor, if one is open.
    pub fn close_editor(&mut self) {
        if let Some(editor) = self.editor.take() {
            editor.close();
        }
    }

    /// Whether an editor is open.
    #[must_use]
    pub fn editor_open(&self) -> bool {
        self.editor.is_some()
    }

    /// What the open editor can do; the defaults when none is open.
    #[must_use]
    pub fn editor_capabilities(&self) -> EditorCapabilities {
        let Some(editor) = &self.editor else {
            return EditorCapabilities::default();
        };
        EditorCapabilities {
            resize: tutti_plugin_types::ResizeHints {
                resizable: editor.resizable(),
                ..Default::default()
            },
            ..EditorCapabilities::default()
        }
    }

    /// The latest size the editor asked the host for, taken.
    pub fn take_editor_resize_request(&self) -> Option<EditorSize> {
        self.editor.as_ref()?.plug_frame.take_resize_request()
    }

    /// The size the open editor snaps `requested` to; `None` with no editor
    /// open.
    #[must_use]
    pub fn editor_size_constraint(&self, requested: EditorSize) -> Option<EditorSize> {
        Some(self.editor.as_ref()?.constrain(requested).0)
    }

    /// Resizes the open editor.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::NotInitialized`] with no editor open, or the view's
    /// refusal.
    pub fn resize_editor(&self, requested: EditorSize) -> Result<EditorSize, Vst3Error> {
        self.editor
            .as_ref()
            .ok_or(Vst3Error::NotInitialized)?
            .resize(requested)
    }

    /// Delivers a key press to the open editor; `true` if it was consumed.
    #[must_use]
    pub fn editor_key_down(&self, key: u16, virtual_key: i16, modifiers: i16) -> bool {
        self.editor
            .as_ref()
            .is_some_and(|e| crate::view::send_key_down(&e.view, key, virtual_key, modifiers))
    }

    /// Delivers a key release to the open editor; `true` if it was consumed.
    #[must_use]
    pub fn editor_key_up(&self, key: u16, virtual_key: i16, modifiers: i16) -> bool {
        self.editor
            .as_ref()
            .is_some_and(|e| crate::view::send_key_up(&e.view, key, virtual_key, modifiers))
    }

    /// Delivers a wheel step to the open editor; `true` if it was consumed.
    #[must_use]
    pub fn editor_wheel(&self, distance: f32) -> bool {
        self.editor
            .as_ref()
            .is_some_and(|e| crate::view::send_wheel(&e.view, distance))
    }

    /// Tells the open editor whether it has focus.
    pub fn set_editor_focus(&self, focused: bool) {
        if let Some(e) = &self.editor {
            crate::view::set_view_focus(&e.view, focused);
        }
    }

    /// Pumps the run loop the plugin registered timers and file descriptors on
    /// (Linux); nothing elsewhere.
    pub fn run_editor_loop_iteration(&self) {
        #[cfg(target_os = "linux")]
        self.module.run_loop().run_iteration();
    }

    /// What the run loop has registered and fired.
    #[cfg(feature = "conformance")]
    #[must_use]
    pub fn run_loop_activity(&self) -> crate::RunLoopActivity {
        #[cfg(target_os = "linux")]
        {
            self.module.run_loop().activity()
        }
        #[cfg(not(target_os = "linux"))]
        {
            crate::RunLoopActivity::default()
        }
    }

    /// The reference count of the host application, as a plugin holding it
    /// would see it.
    #[cfg(feature = "conformance")]
    #[must_use]
    pub fn host_context_refcount(&self) -> usize {
        use vst3::com_scrape_types::Unknown;

        let Some(iface) = self.application.as_com_ref::<IHostApplication>() else {
            return 0;
        };
        // SAFETY: `iface` is the live host application; the add/release pair is
        // balanced, and the refcount read between them is the figure asked for.
        unsafe {
            let after_add = IHostApplication::add_ref(iface.as_ptr());
            IHostApplication::release(iface.as_ptr());
            after_add - 1
        }
    }

    /// Calls `IComponentHandler::performEdit` as a plugin's editor would.
    #[cfg(feature = "conformance")]
    pub fn simulate_perform_edit(&self, id: u32, value: f64) {
        use vst3::Steinberg::Vst::IComponentHandlerTrait;
        if let Some(handler) = self.handler.as_com_ref::<IComponentHandler>() {
            // SAFETY: a plain call on the live host handler.
            unsafe { handler.performEdit(id, value) };
        }
    }

    /// Calls `IComponentHandler::restartComponent` as a plugin would.
    #[cfg(feature = "conformance")]
    pub fn simulate_restart_component(&self, flags: i32) {
        use vst3::Steinberg::Vst::IComponentHandlerTrait;
        if let Some(handler) = self.handler.as_com_ref::<IComponentHandler>() {
            // SAFETY: a plain call on the live host handler.
            unsafe { handler.restartComponent(flags) };
        }
    }
}
