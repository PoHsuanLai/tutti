//! The main-thread view (design 029 step 2): `MainSession::update` services
//! the editors opened through the view and reports what they ask for, the
//! editor's `open`, `resize`-side `rescale` and `close`, and the graveyard a
//! removal parks a plugin in until the view empties it.
//!
//! The plugin is `tutti_plugin_types::fake`'s: a control with an editor the
//! test scripts (`FakePlugin::with_editor`, `request_editor_size`), hosted at a
//! node with `Graph::host_plugin`, so no `plugin-server` and no display is
//! needed.

use std::sync::Arc;

use dpi::{PhysicalSize, Size};

use tutti_fixtures::StandInWindow;
use tutti_graph::{ForkByClone, NodeKey};
use tutti_nodes::testing::Const;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::fake::{Call, FakePlugin};
use tutti_plugin_types::{
    ContentScale, EditorError, MainThread, Normalized, ParamId, ParameterInfo, PluginFailure,
};
use tutti_session::{Opened, PluginChange, PluginEvent, Session, SessionEvent};
use tutti_types::{ChannelLayout, SampleRate};

/// A backend that answers nothing: the editor route is the fake control's.
struct Inert;

impl tutti_plugin_types::AnyThread for Inert {
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
        None
    }
    fn parameter_value(&self, _id: ParamId) -> Option<Normalized> {
        None
    }
    fn set_parameter_value(&self, _id: ParamId, _value: Normalized) {}
    fn parameter_plain(&self, _id: ParamId, _value: Normalized) -> Option<f64> {
        None
    }
    fn parameter_normalized(&self, _id: ParamId, _plain: f64) -> Option<Normalized> {
        None
    }
    fn parameter_text(&self, _id: ParamId, _value: Normalized) -> Option<String> {
        None
    }
    fn parameter_value_from_text(&self, _id: ParamId, _text: &str) -> Option<Normalized> {
        None
    }
    fn save_state(&self) -> Result<Vec<u8>, tutti_plugin::error::StateError> {
        Err(tutti_plugin::error::StateError::NoStateRoute)
    }
    fn set_render_mode(&self, _mode: tutti_plugin_types::RenderMode) -> bool {
        false
    }
    fn set_automation_mode(
        &self,
        _mode: tutti_plugin_types::AutomationMode,
    ) -> Result<(), tutti_plugin_types::AutomationModeError> {
        Err(tutti_plugin_types::AutomationModeError::PluginDead)
    }
}

impl tutti_plugin_types::Health for Inert {
    fn is_crashed(&self) -> bool {
        false
    }
    fn crash_cause(&self) -> Option<PluginFailure> {
        None
    }
    fn rate_refusal(&self) -> Option<(SampleRate, String)> {
        None
    }
    fn block_counts(&self) -> tutti_plugin_types::BlockCounts {
        tutti_plugin_types::BlockCounts::default()
    }
    fn terminate(&self, _reason: tutti_plugin_types::TerminationReason) {}
}

fn main_thread() -> MainThread {
    MainThread::claim().expect("one test, one process")
}

/// A detached session with a fake plugin (with an editor) at a node.
fn session_with_editor() -> (Session, NodeKey, FakePlugin) {
    let (fake, loaded) = FakePlugin::new();
    fake.with_editor(true);
    let control: Arc<dyn tutti_plugin_host::MainThreadControl> = Arc::new(loaded);
    let handle = PluginHandle::from_backend(
        Arc::new(Inert),
        Some(control),
        Default::default(),
        Default::default(),
        Default::default(),
    );
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    let (node, _) = s.graph().add(ForkByClone(Const::mono(0.0)));
    s.graph()
        .host_plugin(
            node,
            handle,
            tutti_plugin::catalog::PluginRef::file("fake.clap").expect("a plugin file"),
        )
        .expect("the node is in the graph");
    s.commit();
    (s, node.erase(), fake)
}

fn wanted_sizes(events: &[SessionEvent], node: NodeKey) -> Vec<Size> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Plugin(PluginEvent {
                node: n,
                change: PluginChange::EditorWantsSize(size),
                ..
            }) if *n == node => Some(*size),
            _ => None,
        })
        .collect()
}

fn idles(fake: &FakePlugin) -> usize {
    fake.calls()
        .iter()
        .filter(|c| **c == Call::EditorIdle)
        .count()
}

/// **An editor that asks for a size raises `EditorWantsSize`, once**, from
/// `MainSession::update`: the plugin's request, polled while it services the
/// open editor, is the event, and a second update has nothing more to say.
#[test]
fn an_editor_that_requests_a_size_raises_editor_wants_size() {
    let (mut s, node, fake) = session_with_editor();
    let mut ui = s.main(main_thread());
    let opened = ui
        .plugin(node)
        .unwrap()
        .editor()
        .open(&StandInWindow, ContentScale::IDENTITY)
        .unwrap();
    assert_eq!(opened, Opened::Embedded(PhysicalSize::new(640, 480).into()));

    let want = PhysicalSize::new(1000, 700);
    fake.request_editor_size(want);
    assert_eq!(wanted_sizes(&ui.update(), node), vec![want.into()]);
    assert!(
        wanted_sizes(&ui.update(), node).is_empty(),
        "a request is answered once"
    );
}

/// **`update` services an editor only while it is open through the view:** the
/// plugin's idle runs once per update with the editor open, and neither the
/// idle nor the request is touched before it opens or after it closes.
#[test]
fn update_services_the_open_editor_and_no_other() {
    let (mut s, node, fake) = session_with_editor();
    let mut ui = s.main(main_thread());

    fake.request_editor_size(PhysicalSize::new(1, 1));
    assert!(wanted_sizes(&ui.update(), node).is_empty());
    assert_eq!(idles(&fake), 0, "nothing is serviced before it opens");

    ui.plugin(node)
        .unwrap()
        .editor()
        .open(&StandInWindow, ContentScale::IDENTITY)
        .unwrap();
    assert_eq!(
        wanted_sizes(&ui.update(), node).len(),
        1,
        "the request waiting since before the open is heard once it is serviced"
    );
    let _ = ui.update();
    assert_eq!(idles(&fake), 2, "one idle per update");

    ui.plugin(node).unwrap().editor().close();
    let before = idles(&fake);
    fake.request_editor_size(PhysicalSize::new(2, 2));
    assert!(wanted_sizes(&ui.update(), node).is_empty());
    assert_eq!(idles(&fake), before, "a closed editor is not serviced");
    assert!(fake.calls().contains(&Call::CloseEditor));
}

/// **`rescale` reaches the plugin's live scale call, without closing the
/// editor**, and a format with none answers the typed refusal: the fake
/// takes the scale only when told to (as CLAP and VST3 do), and refuses
/// like VST2 and AU otherwise.
#[test]
fn rescale_reaches_the_plugins_scale_call_or_is_refused_by_name() {
    let (mut s, node, fake) = session_with_editor();
    let mut ui = s.main(main_thread());
    let mut plugin = ui.plugin(node).unwrap();
    let mut editor = plugin.editor();
    editor.open(&StandInWindow, ContentScale::IDENTITY).unwrap();
    let two = ContentScale::new(2.0).unwrap();

    assert!(
        matches!(editor.rescale(two), Err(EditorError::ScaleUnsupported)),
        "a format with no live scale call refuses by name"
    );
    fake.with_editor_scale(true);
    editor.rescale(two).unwrap();
    let calls = fake.calls();
    assert!(calls.contains(&Call::SetEditorScale(2.0_f64.to_bits())));
    assert_eq!(
        calls.iter().filter(|c| **c == Call::OpenEditor).count(),
        1,
        "the editor was not reopened"
    );
    assert!(
        !calls.contains(&Call::CloseEditor),
        "the editor was not closed"
    );
}

/// **A removed plugin waits in the graveyard until the main thread empties
/// it:** `Session::update` (no proof) leaves it, `MainSession::update` drops
/// it. The count is `Session::retired_plugins`.
#[test]
fn a_removed_plugin_is_dropped_by_the_main_view_not_by_update() {
    let (mut s, node, _fake) = session_with_editor();
    assert_eq!(s.retired_plugins(), 0);
    s.remove(node).unwrap();
    assert_eq!(s.retired_plugins(), 1, "removal parks the record");
    let _ = s.update();
    assert_eq!(s.retired_plugins(), 1, "Session::update frees no plugin");
    let _ = s.main(main_thread()).update();
    assert_eq!(s.retired_plugins(), 0, "the view's update drops it");
}
