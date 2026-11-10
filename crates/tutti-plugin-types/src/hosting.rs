//! Where a format can run: the hosting markers (design 021 §2.1, §3).
//!
//! A format is [`Loaded`] in its `-host` crate. Whether the host may load it
//! into its own process, or serve it from another one with an editor-only
//! second instance here, is a fact the format states by implementing
//! [`InProcess`] or [`OutOfProcess`]. A host that needs one names it as a
//! bound, so a format without it is refused at compile time, not at load.
//!
//! The markers are safe traits: a wrong impl is a behaviour bug (an editor that
//! drifts from its audio instance), not undefined behaviour. They are explicit
//! opt-in, so a format that writes neither impl is hostable nowhere. Every impl
//! is listed in the plugin contract's `hostings` (`scripts/check-hosting.sh`).

use std::path::Path;

use crate::{
    AutomationMode, ContentScale, EditorCapabilities, EditorError, EditorSize, Loaded, MainThread,
    Normalized, ParamId, PluginError, PluginResult, WindowHandle,
};

/// What to load from a plugin file: which plugin, when the file exports
/// several.
///
/// Rate, block size and sample format are the activation's
/// ([`Prepare`](crate::Prepare)), and a state to restore is a
/// [`Control::load_state`](crate::Control::load_state) on the loaded plugin, so
/// a load needs only the choice of plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Load {
    plugin: Option<String>,
}

impl Load {
    /// The file's first plugin.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The plugin with this id in a file that exports several: a CLAP plugin
    /// id, a VST3 class name. A format whose files hold one plugin refuses a
    /// choice.
    #[must_use]
    pub fn plugin(mut self, id: impl Into<String>) -> Self {
        self.plugin = Some(id.into());
        self
    }

    /// The chosen plugin, if one was named.
    #[must_use]
    pub fn chosen(&self) -> Option<&str> {
        self.plugin.as_deref()
    }
}

/// The format can run in the host's own process.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be hosted in this process",
    note = "implement `InProcess` only if the plugin can be loaded and run here (design 021 §3)"
)]
pub trait InProcess: Loaded {
    /// Loads the plugin into this process, not activated, honouring the
    /// format's own thread rules.
    ///
    /// # Errors
    ///
    /// The file cannot be opened, the chosen plugin is not in it, or the
    /// plugin refuses to be created.
    fn load_in_process(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self>;
}

/// The format can be served from another process, with its editor elsewhere.
///
/// The promise (design 021 §3.2): the audio instance runs in another process;
/// an editor-only second instance of the same module runs here and shows what
/// the audio instance holds; state and parameters cross only by id and by
/// blob, never as a pointer, handle or callback of the plugin's.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be hosted over IPC",
    note = "its editor cannot run apart from its audio instance (design 021 §4)"
)]
pub trait OutOfProcess: Loaded {
    /// The editor-only instance in the host process.
    type Editor: Editor;

    /// What the server calls to build the audio instance, not activated.
    ///
    /// # Errors
    ///
    /// As [`InProcess::load_in_process`].
    fn load_served(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self>;

    /// What the host calls to build the editor instance. It is never
    /// activated and never renders.
    ///
    /// # Errors
    ///
    /// As [`InProcess::load_in_process`].
    fn load_editor(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self::Editor>;
}

/// A loaded plugin can be loaded again the same way, in the same hosting: what
/// a fork of the graph needs before it copies the state across.
pub trait Reopen: Loaded {
    /// A fresh instance from the same file and [`Load`].
    ///
    /// # Errors
    ///
    /// As [`InProcess::load_in_process`].
    fn reopen(&self, main: &MainThread) -> PluginResult<Self>;
}

/// The editor-only instance an [`OutOfProcess`] format loads in the host
/// process while its audio instance runs in another.
///
/// It shows the plugin's GUI and is kept faithful to the audio instance by the
/// host: parameter writes and the state blob go to it, and the parameter edits
/// the user makes in it come back through [`poll_edits`](Self::poll_edits).
/// Every method is a main-thread call, which the `&MainThread` proves.
pub trait Editor: Send {
    /// Embeds into `parent`, whose backing-store scale is `scale` (forwarded
    /// where the format takes it; see [`ContentScale`]).
    ///
    /// # Errors
    ///
    /// The plugin refuses or fails to open its editor.
    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize>;

    /// Opens as a plugin-owned floating window. No parent in, no size out.
    ///
    /// Defaulted to a refusal: only CLAP has the concept.
    ///
    /// # Errors
    ///
    /// The format has no floating editor, or the plugin refuses.
    fn open_floating_editor(&mut self, main: &MainThread) -> PluginResult<()> {
        let _ = main;
        Err(PluginError::Editor(EditorError::PluginError(
            "this plugin format has no floating-window editor".into(),
        )))
    }

    /// Closes the editor. The instance stays loaded.
    fn close_editor(&mut self, main: &MainThread);

    /// The editor's main-thread tick: its timers, callbacks and file
    /// descriptors are served from here.
    fn editor_idle(&mut self, main: &MainThread);

    /// Mirrors a parameter write so the editor's display follows a host-side
    /// edit. Normalized, as the audio path takes it; an impl denormalizes at its
    /// own edge if its format speaks plain units.
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized);

    /// Mirrors the audio instance's state into the editor instance.
    ///
    /// # Errors
    ///
    /// The plugin refuses the state.
    fn set_state(&mut self, main: &MainThread, data: &[u8]) -> PluginResult<()>;

    /// The parameter edits the user made in the editor since the last poll,
    /// normalized, for the host to send to the audio instance.
    fn poll_edits(&mut self, main: &MainThread) -> Vec<(ParamId, f32)>;

    /// The editor instance's whole state, for the host to give the audio
    /// instance what the user changed in the editor that is not a parameter
    /// (design 021 Q8).
    ///
    /// # Errors
    ///
    /// The plugin cannot save its state.
    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>>;

    /// Whether the plugin said its state changed since the last call (CLAP
    /// `state.mark_dirty`, VST3 `setDirty`). Consumes the signal. `false` for a
    /// format without one; the host still reads the state when the editor
    /// closes.
    fn take_dirty(&mut self, main: &MainThread) -> bool {
        let _ = main;
        false
    }

    /// Shows the host's automation mode in the editor. No-op for a format
    /// without the concept.
    fn set_automation_state(&mut self, main: &MainThread, mode: AutomationMode) {
        let _ = (main, mode);
    }

    /// The view's static resize capabilities.
    fn editor_capabilities(&mut self, main: &MainThread) -> EditorCapabilities {
        let _ = main;
        EditorCapabilities::default()
    }

    /// Asks the view to take `requested`; it answers with the size it took.
    ///
    /// # Errors
    ///
    /// The format or the plugin cannot resize.
    fn set_editor_size(
        &mut self,
        main: &MainThread,
        requested: EditorSize,
    ) -> PluginResult<EditorSize> {
        let _ = (main, requested);
        Err(PluginError::Editor(EditorError::PluginError(
            "set_editor_size not supported".into(),
        )))
    }

    /// A size the plugin asked its window to take, if it asked since the last
    /// poll.
    fn poll_editor_resize_request(&mut self, main: &MainThread) -> Option<EditorSize> {
        let _ = main;
        None
    }
}
