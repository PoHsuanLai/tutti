//! What the session's `Phase::Plugins` does with a hosted plugin's health: the
//! engine's latched cause reaches the death report verbatim, a crash is not a
//! death until the debounce window elapses, a death removes the node and the
//! record, a live plugin is never written off, and a graph with no hosted
//! plugin is never marked edited by the phase.
//!
//! These are the session halves of `bevy-tutti`'s `plugin_host::health` tests
//! (the component mirror, `PluginHealth`, and the entity's `AudioNode`
//! removal stayed with the adapter), moved here when the adapter was parked.
//! The plugin is a fake backend (`PluginHandle::from_backend`) hosted at a
//! node with `Graph::host_plugin`, so no `plugin-server` is needed; what the
//! plugin reports (`SessionEvent::Plugin`'s `ParamChanged` and the rest) is driven through the same fake's listeners. The real
//! plugin's death is `plugin.rs`'s
//! `a_plugin_dead_for_the_window_is_removed_and_reported_once`.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tutti_types::ChannelLayout;

use tutti_graph::ForkByClone;
use tutti_graph::NodeKey;
use tutti_nodes::testing::Const;
use tutti_plugin_host::Liveness;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::ParameterInfo;
use tutti_plugin_types::{Normalized, ParamId};
use tutti_session::{Phase, PhaseCx, PluginLiveness, Session, SessionEvent};
use tutti_session::{PluginChange, PluginEvent};
use tutti_types::SampleRate;

/// A backend whose liveness is whatever the test says it is.
struct FakeBackend {
    cause: Option<String>,
}

impl tutti_plugin_types::AnyThread for FakeBackend {
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
    ) -> Result<(), tutti_plugin_types::EditorError> {
        Err(tutti_plugin_types::EditorError::PluginCrashed)
    }
}

impl tutti_plugin_types::Health for FakeBackend {
    fn is_crashed(&self) -> bool {
        self.cause.is_some()
    }
    fn crash_cause(&self) -> Option<String> {
        self.cause.clone()
    }
    fn rate_refusal(&self) -> Option<(tutti_types::SampleRate, String)> {
        None
    }
    fn block_counts(&self) -> tutti_plugin_types::BlockCounts {
        tutti_plugin_types::BlockCounts::default()
    }
    fn terminate(&self, _cause: &str) {}
}

fn handle_with(cause: Option<&str>) -> PluginHandle {
    PluginHandle::from_backend(
        Arc::new(FakeBackend {
            cause: cause.map(str::to_string),
        }),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    )
}

fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}

/// A detached session with a fake plugin (reporting `cause`) hosted at a dc
/// node.
fn session_with(cause: Option<&str>) -> (Session, NodeKey) {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    let (node, _) = s.graph().add(ForkByClone(Const::mono(0.0)));
    assert!(s.graph().host_plugin(
        node,
        handle_with(cause),
        tutti_plugin::catalog::PluginRef::file("fake.clap").expect("a plugin file"),
    ));
    // The insert is an edit: send it, so the flag starts clear.
    s.commit();
    (s, node.erase())
}

/// `Phase::Plugins` at `t0 + ms`: the deaths it reported.
fn deaths_at(s: &mut Session, t0: Instant, ms: u64) -> Vec<(NodeKey, String)> {
    let mut out = Vec::new();
    s.run_phase(
        Phase::Plugins,
        PhaseCx::at(t0 + Duration::from_millis(ms)),
        &mut out,
    );
    out.into_iter()
        .filter_map(|e| match e {
            SessionEvent::Plugin(PluginEvent {
                node,
                change: PluginChange::Died { cause },
                ..
            }) => Some((node, cause)),
            _ => None,
        })
        .collect()
}

/// One observation every half window, as frames at 60 Hz span it: `frames`
/// of them. The deaths each reported.
fn observe(s: &mut Session, frames: usize) -> Vec<(NodeKey, String)> {
    let _ = main_thread();
    let t0 = Instant::now();
    let half = Liveness::DEFAULT_WINDOW.as_millis() as u64 / 2;
    let mut died = Vec::new();
    for frame in 0..frames {
        died.extend(deaths_at(s, t0, frame as u64 * half));
    }
    died
}

/// **A graph with no hosted plugin is not edited by the phase**: a dc node
/// that is no plugin, and the phase alone (no commit, so an edit the phase
/// made would stay on the session's flag). Every node is a graph node, so a
/// phase that treated each as a plugin would mark the graph edited every
/// update and re-plan compensation forever.
#[test]
fn a_non_plugin_node_never_marks_the_graph_edited() {
    let _ = main_thread();
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    s.graph().add(ForkByClone(Const::mono(0.0)));
    s.commit();
    assert!(!s.graph_ref().is_edited(), "precondition: committed");
    let t0 = Instant::now();
    for ms in [0, 10, 20] {
        deaths_at(&mut s, t0, ms);
    }
    assert!(
        !s.graph_ref().is_edited(),
        "a dc node is not a plugin and has no latency to compensate"
    );
}

/// **The engine's latched reason reaches the death report verbatim.**
/// Asserting on the content is what distinguishes reading the engine's cause
/// from inventing one: a fixed placeholder would pass a check for a death
/// alone.
#[test]
fn a_dead_plugin_reports_the_engines_cause_not_a_placeholder() {
    let (mut s, node) = session_with(Some(
        "could not connect to plugin-server: No such file or directory",
    ));
    let died = observe(&mut s, Liveness::DEATHS as usize);
    assert_eq!(
        died,
        [(
            node,
            "could not connect to plugin-server: No such file or directory".to_string()
        )],
        "the cause must be the engine's latched reason, carried through unchanged"
    );
}

/// **A plugin is not written off on the first bad observation.** Pinned at
/// every count below the threshold, because an off-by-one here would unwire a
/// live plugin: it is Failing with that many consecutive observations, and
/// its node stays.
#[test]
fn a_crash_is_not_declared_until_the_debounce_elapses() {
    for frames in 1..Liveness::DEATHS as usize {
        let (mut s, node) = session_with(Some("stream closed"));
        let died = observe(&mut s, frames);
        assert!(died.is_empty(), "at {frames} observation(s): {died:?}");
        assert_eq!(
            s.plugin(node).expect("hosted").liveness(),
            &PluginLiveness::Failing {
                consecutive: frames as u8
            },
            "at {frames} observation(s) the plugin is failing, not dead"
        );
        assert!(
            s.graph_ref().contains(node),
            "a failing plugin must stay wired: unwiring it early silences a live plugin"
        );
    }
}

/// **Declaring a plugin dead removes its node and its record.**
#[test]
fn declaring_death_removes_the_node_and_the_record() {
    let (mut s, node) = session_with(Some("stream closed"));
    let died = observe(&mut s, Liveness::DEATHS as usize);
    assert_eq!(died.len(), 1, "{died:?}");
    assert!(
        !s.graph_ref().contains(node),
        "a dead plugin's node is removed"
    );
    assert_eq!(s.plugins().count(), 0, "and the session no longer hosts it");
}

/// **A plugin the engine reports as alive is never written off**, however
/// long it runs: the negative case, which none of the above would notice a
/// phase declaring death unconditionally fail.
#[test]
fn a_live_plugin_is_never_declared_dead() {
    let (mut s, node) = session_with(None);
    let died = observe(&mut s, Liveness::DEATHS as usize + 5);
    assert!(died.is_empty(), "{died:?}");
    assert_eq!(
        s.plugin(node).expect("hosted").liveness(),
        &PluginLiveness::Healthy
    );
    assert!(s.graph_ref().contains(node), "a healthy plugin stays wired");
}

/// A detached session with a fake plugin hosted at a dc node, and the
/// listeners its backend reports through (firing one is the plugin
/// reporting).
fn session_reporting() -> (
    Session,
    NodeKey,
    tutti_plugin_host::ParameterChangeSink,
    tutti_plugin_host::RefreshSink,
    tutti_plugin_host::InvalidateSink,
) {
    let (params, refresh, invalidate) = (
        tutti_plugin_host::ParameterChangeSink::new(),
        tutti_plugin_host::RefreshSink::new(),
        tutti_plugin_host::InvalidateSink::new(),
    );
    let handle = PluginHandle::from_backend(
        Arc::new(FakeBackend { cause: None }),
        None,
        Default::default(),
        Default::default(),
        params.clone(),
    )
    .with_refresh_sink(refresh.clone())
    .with_invalidate_sink(invalidate.clone());
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    let (node, _) = s.graph().add(ForkByClone(Const::mono(0.0)));
    assert!(s.graph().host_plugin(
        node,
        handle,
        tutti_plugin::catalog::PluginRef::file("fake.clap").expect("a plugin file"),
    ));
    s.commit();
    (s, node.erase(), params, refresh, invalidate)
}

/// The plugin events one `Phase::Plugins` reported.
fn plugin_events(s: &mut Session) -> Vec<PluginEvent> {
    let mut out = Vec::new();
    s.run_phase(Phase::Plugins, PhaseCx::at(Instant::now()), &mut out);
    out.into_iter()
        .filter_map(|e| match e {
            SessionEvent::Plugin(event) => Some(event),
            _ => None,
        })
        .collect()
}

/// **A plugin's own edit of a parameter arrives as `ParamChanged`**, in the
/// order the plugin made it, with its gesture's ends, at the plugin's node: a
/// gesture is a `GestureBegan`, a change with its value and its
/// recording flag, a `GestureEnded`. A later update reports nothing again.
#[test]
fn a_plugins_own_edit_arrives_as_param_changed() {
    use tutti_plugin_types::ParamEdit;
    let _ = main_thread();
    let (mut s, node, params, _, _) = session_reporting();
    let id = ParamId::new(7);
    let value = Normalized::clamped(0.25);
    params.fire(ParamEdit::Begin(id));
    params.fire(ParamEdit::change(id, value));
    params.fire(ParamEdit::change(id, value).unrecorded());
    params.fire(ParamEdit::End(id));
    let events = plugin_events(&mut s);
    let changes: Vec<_> = events
        .iter()
        .inspect(|e| assert_eq!(e.node, node))
        .map(|e| e.change.clone())
        .collect();
    assert_eq!(
        changes,
        [
            PluginChange::GestureBegan { id },
            PluginChange::ParamChanged {
                id,
                value,
                recorded: true
            },
            PluginChange::ParamChanged {
                id,
                value,
                recorded: false
            },
            PluginChange::GestureEnded { id },
        ]
    );
    assert!(plugin_events(&mut s).is_empty(), "each is reported once");
}

/// **What a plugin reports besides edits reaches the same stream**: a refresh
/// is `ParamsRescanned` or `ParamTitlesChanged`, a tail and a bus change are
/// `TailChanged` and `IoChanged`. A latency it reports is not repeated here
/// (the update that gives it to the graph reports it once, as
/// `LatencyChanged`), and a rate refusal is the plugin's health.
#[test]
fn a_plugins_refreshes_and_invalidations_arrive_in_the_stream() {
    use tutti_plugin_types::{PluginInvalidation, PluginRefresh, PluginTail};
    let _ = main_thread();
    let (mut s, _, _, refresh, invalidate) = session_reporting();
    refresh.fire(PluginRefresh::ParamValues);
    refresh.fire(PluginRefresh::ParamTitles);
    invalidate.fire(PluginInvalidation::Latency {
        samples: tutti_types::Samples(64),
    });
    invalidate.fire(PluginInvalidation::Tail {
        tail: PluginTail::Unbounded,
    });
    invalidate.fire(PluginInvalidation::Io);
    invalidate.fire(PluginInvalidation::RateRestored);
    let changes: Vec<_> = plugin_events(&mut s)
        .into_iter()
        .map(|e| e.change)
        .collect();
    assert_eq!(
        changes,
        [
            PluginChange::ParamsRescanned,
            PluginChange::ParamTitlesChanged,
            PluginChange::TailChanged {
                tail: PluginTail::Unbounded
            },
            PluginChange::IoChanged,
        ]
    );
}
