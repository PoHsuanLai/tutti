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
//! is listed in the plugin contract's `hostings` (`cargo xtask check-hosting`).

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::{
    AutomationMode, ContentScale, EditorCapabilities, EditorError, EditorSize, KeyEvent, KeyUse,
    Loaded, MainThread, Normalized, ParamEdit, ParamId, PluginError, PluginResult, Ready, Wakeups,
    WindowHandle,
};

/// What to load from a plugin file: which plugin, when the file exports
/// several.
///
/// Rate, block size and sample format are the activation's
/// ([`Prepare`](crate::Prepare)), and a state to restore is a
/// [`Control::load_state`](crate::Control::load_state) on the loaded plugin, so
/// a load needs only the choice of plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Load {
    plugin: Option<String>,
}

impl Load {
    /// The file's first plugin.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The plugin with this id in a file that exports several: the
    /// [`PluginDescriptor::id`](crate::PluginDescriptor) the scan reports, whatever the
    /// format: a CLAP plugin id, a VST3 class id (`vst3.<class id>`), an AU
    /// descriptor id (`au.<type>.<subtype>.<manufacturer>`), a VST2 plugin's
    /// `vst2.<uniqueID>` (the sub-plugin of a shell; a file that is no shell holds
    /// the one plugin, which a choice of another id refuses).
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

/// Where an instance came from: the file that was opened and the [`Load`]
/// that chose the plugin in it. What [`Reopen`] loads again.
///
/// The path is the file the format opened (a module, or a library a format
/// was pointed at in place of its bundle), so a reopen opens the same one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    path: PathBuf,
    load: Load,
}

impl Origin {
    /// The file `path`, with the plugin `load` chose in it.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, load: Load) -> Self {
        Self {
            path: path.into(),
            load,
        }
    }

    /// The file that was opened.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The choice of plugin in it.
    #[must_use]
    pub fn load(&self) -> &Load {
        &self.load
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

    /// Where this instance came from, however it was loaded: what [`Reopen`]
    /// loads again.
    fn origin(&self) -> &Origin;
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
///
/// Every [`InProcess`] format has it, from its [`Origin`]; an out-of-process
/// hosting implements it by starting a new server.
pub trait Reopen: Loaded {
    /// A fresh instance from the same file and [`Load`], not activated, with
    /// none of this one's state.
    ///
    /// # Errors
    ///
    /// As [`InProcess::load_in_process`].
    fn reopen(&self, main: &MainThread) -> PluginResult<Self>;
}

impl<F: InProcess> Reopen for F {
    fn reopen(&self, main: &MainThread) -> PluginResult<Self> {
        let origin = self.origin();
        F::load_in_process(main, origin.path(), origin.load())
    }
}

/// A plugin's editor window, in whatever object holds the plugin: a
/// [`Control`](crate::Control) (loaded or active) or an [`Editor`] (the
/// editor-only instance). Every method is a main-thread call, which the
/// `&MainThread` proves.
///
/// Every method is required but [`open_floating_editor`](Self::open_floating_editor),
/// [`set_editor_scale`](Self::set_editor_scale) and [`editor_key`](Self::editor_key), whose default is a typed
/// refusal, so an implementation states its answer to each other rather than
/// inheriting one.
pub trait EditorWindow {
    /// Whether the plugin has an editor it can embed or open.
    fn has_editor(&self, main: &MainThread) -> bool;

    /// Embeds the editor into `parent`, whose backing-store scale is `scale`
    /// (forwarded where the format takes it; see [`ContentScale`]), and returns
    /// its size.
    ///
    /// # Errors
    ///
    /// The plugin has no editor, or refuses or fails to open it.
    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize>;

    /// Opens the editor as a plugin-owned floating window. No parent in, no
    /// size out.
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

    /// Tells the open editor its window's backing-store scale changed (it moved
    /// to another monitor), without closing it: CLAP's `gui.set_scale` and VST3's
    /// `IPlugViewContentScaleSupport::setContentScaleFactor`.
    ///
    /// Defaulted to a refusal, [`EditorError::ScaleUnsupported`], as
    /// [`open_floating_editor`](Self::open_floating_editor) is: VST2 takes the
    /// scale only at open, and AU and every logical-pixel window (Cocoa) are
    /// scaled by the OS, so for them there is nothing to tell.
    ///
    /// # Errors
    ///
    /// [`EditorError::ScaleUnsupported`] as above, and any other refusal of the
    /// plugin or of a missing editor.
    fn set_editor_scale(&mut self, main: &MainThread, scale: ContentScale) -> PluginResult<()> {
        let _ = (main, scale);
        Err(PluginError::Editor(EditorError::ScaleUnsupported))
    }

    /// Forwards a key the host's window received to the open editor, and reports
    /// whether the editor used it: VST2's `effEditKeyDown` and `effEditKeyUp`.
    ///
    /// Defaulted to a refusal, [`EditorError::KeysUnsupported`], as
    /// [`set_editor_scale`](Self::set_editor_scale) is: a format states whether it takes
    /// keys rather than inheriting "not used".
    ///
    /// # Errors
    ///
    /// [`EditorError::KeysUnsupported`] as above, and any other refusal of the plugin or of
    /// a missing editor.
    fn editor_key(&mut self, main: &MainThread, event: KeyEvent) -> PluginResult<KeyUse> {
        let _ = (main, event);
        Err(PluginError::Editor(EditorError::KeysUnsupported))
    }

    /// Closes the editor. The plugin stays loaded. Nothing happens when no
    /// editor is open.
    fn close_editor(&mut self, main: &MainThread);

    /// The editor's main-thread tick, where the format has one that is not a
    /// request: VST2's `effEditIdle`, VST3's run loop on Linux. What a plugin
    /// asks for (CLAP's timers, descriptors and callbacks) is served by
    /// [`Control::take_requests`](crate::Control::take_requests) and
    /// [`Lifecycle::drive`](crate::Lifecycle::drive), so a [`Control`](crate::Control)
    /// that has no such tick does nothing here. An [`Editor`] has no
    /// `Lifecycle` to feed, so its idle serves those requests itself.
    fn editor_idle(&mut self, main: &MainThread);

    /// What the plugin wants the host's main loop to wait on, added to `out`
    /// (the caller clears it first): the descriptors it asked to have watched
    /// and the earliest deadline of its timers (CLAP's `posix-fd-support` and
    /// `timer-support`, VST3's `Linux::IRunLoop`). The loop waits on them
    /// together with its own sources, so a plugin's event sources are the
    /// loop's, not a tick's scan. Reads only; a format with none leaves `out`
    /// empty, which is the default.
    ///
    /// A deadline is when a wait should end, not a promise to fire: a format
    /// floors a period it cannot serve at once so that a timer asking for zero
    /// does not spin the loop.
    fn wakeups(&mut self, main: &MainThread, out: &mut Wakeups) {
        let _ = (main, out);
    }

    /// Hands the plugin what fired: the descriptors in `ready` and the timers
    /// due at `now`. Called after a wait on [`wakeups`](Self::wakeups) ended
    /// with something ready or due, on the main thread. A format either runs
    /// the plugin's callbacks now (VST3's handlers) or latches what is due for
    /// the pass that already serves its requests
    /// ([`Control::take_requests`](crate::Control::take_requests): CLAP's
    /// `on_fd` and `on_timer`, under the lifecycle's budget). The default does
    /// nothing.
    fn wake(&mut self, main: &MainThread, ready: &[Ready], now: Instant) {
        let _ = (main, ready, now);
    }

    /// The view's static resize capabilities.
    fn editor_capabilities(&mut self, main: &MainThread) -> EditorCapabilities;

    /// Asks the view to take `requested`; it answers with the size it took.
    ///
    /// # Errors
    ///
    /// The format or the plugin cannot resize.
    fn set_editor_size(
        &mut self,
        main: &MainThread,
        requested: EditorSize,
    ) -> PluginResult<EditorSize>;

    /// A size the plugin asked its window to take, if it asked since the last
    /// poll.
    fn poll_editor_resize_request(&mut self, main: &MainThread) -> Option<EditorSize>;
}

/// The editor-only instance an [`OutOfProcess`] format loads in the host
/// process while its audio instance runs in another.
///
/// It shows the plugin's GUI and is kept faithful to the audio instance by the
/// host: parameter writes and the state blob go to it, and the parameter edits
/// the user makes in it come back through [`poll_edits`](Self::poll_edits).
/// Every method is a main-thread call, which the `&MainThread` proves. Its
/// window is an [`EditorWindow`].
pub trait Editor: EditorWindow + Send {
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

    /// The parameter edits the user made in the editor since the last poll, in
    /// the order the plugin made them: the changes the host sends to the audio
    /// instance, and the gesture around them.
    fn poll_edits(&mut self, main: &MainThread) -> Vec<ParamEdit>;

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
}
