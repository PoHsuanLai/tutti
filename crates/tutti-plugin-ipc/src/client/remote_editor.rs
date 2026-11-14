//! The editor that lives in the server.
//!
//! A served plugin's editor is in one of two places. CLAP, VST3 and AU keep an
//! editor-only second instance in the host, which the host's
//! [`EditorLoader`](super::EditorLoader) loads. A client launched with no loader
//! gets this one instead: the editor opens on the served instance itself, in the
//! server process, and every window call is a message to it. It is a runtime
//! choice of the client, not a marker on a format.

use dpi::Size;
use tutti_plugin_types::{
    ContentScale, EditorCapabilities, EditorError, EditorWindow, MainThread, PluginError,
    PluginResult, WindowHandle,
};

use super::audio::AudioBridge;

/// An [`EditorWindow`] that forwards open, close and resize over the wire.
///
/// It holds no window and no plugin: the server owns both, and what the plugin
/// asks of its window (a resize, a close) comes back as the server's messages,
/// which the bridge thread latches (`EditorWantsSize`, `EditorClosed`) for
/// [`poll_editor_resize_request`](EditorWindow::poll_editor_resize_request) and
/// for this type's own record of whether an editor is open.
pub(crate) struct RemoteEditor {
    audio: AudioBridge,
    open: bool,
}

impl RemoteEditor {
    pub(crate) fn new(audio: AudioBridge) -> Self {
        Self { audio, open: false }
    }

    /// Whether an editor is open in the server, as far as this end knows.
    pub(crate) fn is_open(&mut self) -> bool {
        self.hear_closed();
        self.open
    }

    /// Takes the server's news that it closed the editor itself.
    fn hear_closed(&mut self) {
        if self.audio.take_editor_closed() {
            self.open = false;
        }
    }
}

fn plugin_error(e: EditorError) -> PluginError {
    PluginError::Editor(e)
}

impl EditorWindow for RemoteEditor {
    /// Always `true`: whether the plugin has an editor is the descriptor's to
    /// say (`PluginDescriptor::editor`, which the host asks), and one without
    /// is refused by the server when it is asked to open it.
    fn has_editor(&self, _main: &MainThread) -> bool {
        true
    }

    /// Embeds the server's editor into `parent`, which the server can only do
    /// where a window number means the same in both processes
    /// ([`WindowHandle::embeds_across_processes`]); elsewhere use
    /// [`open_floating_editor`](Self::open_floating_editor).
    fn open_editor(
        &mut self,
        _main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<Size> {
        if !WindowHandle::embeds_across_processes() {
            return Err(plugin_error(EditorError::UnsupportedPlatform {
                got: "a window of this platform cannot be embedded into by another process; \
                      open the editor floating"
                    .into(),
            }));
        }
        self.hear_closed();
        let size = self
            .audio
            .open_editor_in_server(Some(parent.as_u64()), scale)
            .map_err(plugin_error)?;
        let size = size.ok_or_else(|| {
            plugin_error(EditorError::PluginError(
                "the server opened a floating editor where an embedded one was asked for".into(),
            ))
        })?;
        self.open = true;
        Ok(size)
    }

    fn open_floating_editor(&mut self, _main: &MainThread) -> PluginResult<()> {
        self.hear_closed();
        self.audio
            .open_editor_in_server(None, ContentScale::IDENTITY)
            .map_err(plugin_error)?;
        self.open = true;
        Ok(())
    }

    fn close_editor(&mut self, _main: &MainThread) {
        self.hear_closed();
        if self.open {
            let _ = self.audio.close_editor_in_server();
            self.open = false;
        }
    }

    /// Nothing: the server ticks the editor's idle itself, in its own event
    /// loop.
    fn editor_idle(&mut self, _main: &MainThread) {
        self.hear_closed();
    }

    /// The default: the server's view is not asked over the wire.
    fn editor_capabilities(&mut self, _main: &MainThread) -> EditorCapabilities {
        EditorCapabilities::default()
    }

    fn set_editor_size(&mut self, _main: &MainThread, requested: Size) -> PluginResult<Size> {
        self.hear_closed();
        if !self.open {
            return Err(plugin_error(EditorError::PluginError(
                "editor not open".into(),
            )));
        }
        self.audio
            .set_editor_size_in_server(requested)
            .map_err(plugin_error)
    }

    fn poll_editor_resize_request(&mut self, _main: &MainThread) -> Option<Size> {
        self.audio.take_editor_wants_size()
    }
}
