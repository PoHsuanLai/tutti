//! The main-thread view of a session (design 029 section 3.2): the calls a
//! plugin format requires on the UI thread, and the servicing they need.
//!
//! A [`Session`] is `Send` and has no main thread of its own: a headless
//! session has none to be handed. A caller that has one (it claimed it with
//! [`MainThread::claim`], once, on its UI thread) makes a [`MainSession`] with
//! [`Session::main`], which is `&mut Session` and that proof, and nothing else.
//!
//! What only the main thread may do is reachable only through the view:
//!
//! - [`MainSession::update`] runs [`Session::update`], then drops what the
//!   session retired (a removed plugin, the outgoing plugin of a crossfade: the
//!   graveyard), runs the work other threads handed to the main thread
//!   (`tutti_plugin_types::run_deferred`) and services every open editor.
//! - [`MainPlugin::editor`] is the editor: `open`, `resize`, `rescale`, `close`.
//!
//! The session's own freeing paths (`update`, `commit`, `remove`, `replace`)
//! stay on [`Session`], whatever feature is on: a plugin they retire is parked
//! in the graveyard (and a format drops an in-process plugin on the main
//! thread whatever thread releases it), so none tears down an editor off the
//! main thread.
//!
//! ```no_run
//! # use tutti_session::Session;
//! # use tutti_graph::NodeKey;
//! # use tutti_plugin_types::{ContentScale, MainThread};
//! # fn open(session: &mut Session, node: NodeKey, window: &impl raw_window_handle::HasWindowHandle) {
//! let main = MainThread::claim().expect("the UI thread claims first");
//! let mut ui = session.main(main);
//! let _events = ui.update();
//! let _ = ui.plugin(node).unwrap().editor().open(window, ContentScale::IDENTITY);
//! # }
//! ```
//!
//! An editor is the main thread's: the session's read-only view has none.
//!
//! ```compile_fail
//! # use tutti_session::Session;
//! # use tutti_graph::NodeKey;
//! # fn open(session: &Session, node: NodeKey) {
//! let _ = session.plugin(node).unwrap().editor();
//! # }
//! ```
//!
//! The view is the proof, so it cannot leave the thread that made it:
//!
//! ```compile_fail
//! use tutti_session::MainSession;
//! fn assert_send<T: Send>() {}
//! assert_send::<MainSession<'static>>();
//! ```
//!
//! and `Session` itself stays `Send`, for the host with no main thread:
//!
//! ```
//! fn assert_send<T: Send>() {}
//! assert_send::<tutti_session::Session>();
//! ```

use std::ops::{Deref, DerefMut};

use raw_window_handle::HasWindowHandle;
use tutti_graph::NodeKey;
use tutti_plugin_types::{
    ContentScale, EditorError, EditorSize, Features, MainThread, WindowHandle,
};

use crate::plugin::HostedPlugin;
use crate::{NoSuchNode, PluginChange, PluginEvent, Session, SessionEvent};

/// A session and the proof that the caller is on the main thread. Made by
/// [`Session::main`]; see the module docs.
///
/// `!Send` (the proof is), and it holds no state of its own: dropping it
/// leaves the session as it was. It derefs to the session, so every other call
/// is the session's.
#[derive(Debug)]
pub struct MainSession<'a> {
    session: &'a mut Session,
    main: MainThread,
}

impl Session {
    /// The main-thread view of this session, for the caller that holds the
    /// proof ([`MainThread::claim`]). `MainThread` is a `Copy`, zero-sized
    /// value passed as it is.
    pub fn main(&mut self, main: MainThread) -> MainSession<'_> {
        MainSession {
            session: self,
            main,
        }
    }
}

impl Deref for MainSession<'_> {
    type Target = Session;

    fn deref(&self) -> &Session {
        self.session
    }
}

impl DerefMut for MainSession<'_> {
    fn deref_mut(&mut self) -> &mut Session {
        self.session
    }
}

impl MainSession<'_> {
    /// The proof this view holds.
    pub fn proof(&self) -> MainThread {
        self.main
    }

    /// [`Session::update`], and then the main thread's share of it: drops the
    /// plugins the session retired since the last call, runs the work other
    /// threads handed to the main thread, and services every open editor
    /// (the format's idle tick: VST2's `effEditIdle`, VST3's run loop on
    /// Linux), so a host calls no `editor_idle` of its own. An editor that
    /// asks for a size is [`PluginChange::EditorWantsSize`] in the events.
    #[must_use = "the events carry faults and dropouts (a refused commit, a plugin that died, a lost block): handle them, or bind `let _events` with a reason"]
    pub fn update(&mut self) -> Vec<SessionEvent> {
        let mut out = self.session.update();
        let main = self.main;
        // Before the servicing: a plugin the update removed has no editor left
        // to service.
        self.session.graph.graveyard.exhume(&main);
        tutti_plugin_types::run_deferred(&main);
        for (&node, hosted) in &self.session.graph.plugins {
            if hosted.editor == EditorState::Closed {
                continue;
            }
            hosted.handle.editor_idle(&main);
            if let Some(size) = hosted.handle.poll_editor_resize_request(&main) {
                out.push(SessionEvent::Plugin(PluginEvent {
                    node,
                    change: PluginChange::EditorWantsSize(size),
                }));
            }
        }
        out
    }

    /// The plugin hosted at `node`, with the calls that need the main thread.
    ///
    /// # Errors
    ///
    /// [`NoSuchNode`] when no plugin is hosted at `node`.
    pub fn plugin(&mut self, node: impl Into<NodeKey>) -> Result<MainPlugin<'_>, NoSuchNode> {
        let node = node.into();
        let main = self.main;
        self.session
            .graph
            .plugins
            .get_mut(&node)
            .map(|hosted| MainPlugin { node, hosted, main })
            .ok_or_else(|| NoSuchNode::new(node))
    }
}

/// A hosted plugin lent by [`MainSession::plugin`]: its read-only view
/// ([`view`](Self::view)) and its editor.
#[derive(Debug)]
pub struct MainPlugin<'a> {
    node: NodeKey,
    hosted: &'a mut HostedPlugin,
    main: MainThread,
}

impl MainPlugin<'_> {
    /// The plugin's node.
    pub fn node(&self) -> NodeKey {
        self.node
    }

    /// The read-only view of the same plugin.
    pub fn view(&self) -> crate::Plugin<'_> {
        crate::Plugin::new(self.node, self.hosted)
    }

    /// The plugin's editor.
    pub fn editor(&mut self) -> EditorView<'_> {
        EditorView {
            hosted: self.hosted,
            main: self.main,
        }
    }
}

/// What the session knows of a plugin's editor window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum EditorState {
    /// Not opened through the view, or closed since.
    Closed,
    /// Embedded in this parent, at this scale.
    Embedded(WindowHandle, ContentScale),
    /// The plugin's own window.
    Floating,
}

/// Whether the editor embedded in the parent the host gave, or the plugin
/// opened a window of its own ([`EditorView::open`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Opened {
    /// Embedded in the parent window, at this size: the host sizes the parent
    /// to it.
    Embedded(EditorSize),
    /// The plugin made a floating window of its own (a CLAP plugin that cannot
    /// embed): there is no size, and nothing to lay out.
    Floating,
}

/// A hosted plugin's editor, lent by [`MainPlugin::editor`].
#[derive(Debug)]
pub struct EditorView<'a> {
    hosted: &'a mut HostedPlugin,
    main: MainThread,
}

impl EditorView<'_> {
    /// Whether the plugin has an editor to open.
    pub fn exists(&self) -> bool {
        self.hosted.handle.has_editor()
    }

    /// Opens the editor in `window` (a winit or Bevy window, or any
    /// `raw-window-handle` 0.6 window), whose backing-store scale is `scale`:
    /// a window handle carries none. A plugin that can only float
    /// ([`Features::EDITOR_FLOATING`]) opens its own window instead and `window`
    /// is not used. From then on [`MainSession::update`] services it.
    ///
    /// Close it before the window is destroyed.
    ///
    /// # Errors
    ///
    /// The plugin has no editor, refuses it, or `window` has no handle a
    /// plugin can embed into.
    pub fn open(
        &mut self,
        window: &impl HasWindowHandle,
        scale: ContentScale,
    ) -> Result<Opened, EditorError> {
        let handle = &self.hosted.handle;
        if handle.loaded().features.contains(Features::EDITOR_FLOATING) {
            handle.open_floating_editor(&self.main)?;
            self.hosted.editor = EditorState::Floating;
            return Ok(Opened::Floating);
        }
        let parent = WindowHandle::from_window(window)?;
        let size = handle.open_editor(&self.main, parent, scale)?;
        self.hosted.editor = EditorState::Embedded(parent, scale);
        Ok(Opened::Embedded(size))
    }

    /// Asks the plugin to take `requested` (the answer to
    /// [`PluginChange::EditorWantsSize`], or the host's own resize of its
    /// window) and returns the size it took, which may be snapped or clamped.
    ///
    /// # Errors
    ///
    /// No editor is open, or the plugin cannot resize.
    pub fn resize(&mut self, requested: EditorSize) -> Result<EditorSize, EditorError> {
        self.hosted.handle.set_editor_size(&self.main, requested)
    }

    /// Follows a change of the window's backing-store scale (it moved to
    /// another monitor): an embedded editor is closed and opened again in its
    /// parent at `scale`, since every format takes the scale at open, and the
    /// new size is returned. A floating editor is the plugin's window, which
    /// follows its own display. Nothing happens when no editor is open.
    ///
    /// # Errors
    ///
    /// The plugin refuses to open again.
    pub fn rescale(&mut self, scale: ContentScale) -> Result<Option<EditorSize>, EditorError> {
        let EditorState::Embedded(parent, was) = self.hosted.editor else {
            return Ok(None);
        };
        if was == scale {
            return Ok(None);
        }
        self.hosted.handle.close_editor(&self.main);
        self.hosted.editor = EditorState::Closed;
        let size = self.hosted.handle.open_editor(&self.main, parent, scale)?;
        self.hosted.editor = EditorState::Embedded(parent, scale);
        Ok(Some(size))
    }

    /// Closes the editor; the plugin stays loaded. Nothing happens when none
    /// is open.
    pub fn close(&mut self) {
        self.hosted.handle.close_editor(&self.main);
        self.hosted.editor = EditorState::Closed;
    }
}
