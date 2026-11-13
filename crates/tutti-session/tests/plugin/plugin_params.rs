//! A hosted plugin's parameters through the session's views (design 029 step 3):
//! `Plugin::params`, `Param`, `TypedParam` and `ParamGesture`, over a fake
//! backend that declares its parameters and records what the host sets (no
//! `plugin-server`), hosted at a node with `Graph::host_plugin`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tutti_graph::{ForkByClone, NodeKey};
use tutti_nodes::testing::Const;
use tutti_plugin_host::{PluginHandle, RefreshSink};
use tutti_plugin_types::{
    AnyThread, AutomationMode, AutomationModeError, Health, Normalized, ParamFlags, ParamId,
    ParamUnit, ParameterInfo, PluginFailure, PluginRefresh, RenderMode,
};
use tutti_session::{
    Key, PluginChange, PluginEvent, PluginNode, PluginParamError, Session, SessionEvent,
};
use tutti_types::{ChannelLayout, Db, Hz, SampleRate};

const CUTOFF: ParamId = ParamId::new(1);
const LEVEL: ParamId = ParamId::new(2);
const MODE: ParamId = ParamId::new(3);
const LOCKED: ParamId = ParamId::new(4);

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a plugin declares: `cutoff` in hertz over `20..=20000`, `level` in
/// decibels over `-60..=12`, `mode` with no unit and a read-only `locked`.
fn declared() -> Vec<ParameterInfo> {
    vec![
        ParameterInfo::new(CUTOFF, "Cutoff")
            .with_plain_range(20.0, 20_000.0, 1_000.0)
            .with_measures(Some(ParamUnit::Hertz)),
        ParameterInfo::new(LEVEL, "Level")
            .with_plain_range(-60.0, 12.0, 0.0)
            .with_measures(Some(ParamUnit::Decibels)),
        ParameterInfo::new(MODE, "Mode").with_normalized_default(0.0),
        ParameterInfo::new(LOCKED, "Locked")
            .with_normalized_default(0.25)
            .with_flags(ParamFlags::READ_ONLY, ParamFlags::READ_ONLY),
    ]
}

/// The plugin: what it declares now, the value of each, and every value the
/// host set, in order. Plain values map linearly over the declared range.
#[derive(Default)]
struct Fake {
    infos: Mutex<Vec<ParameterInfo>>,
    values: Mutex<HashMap<ParamId, Normalized>>,
    sets: Mutex<Vec<(ParamId, Normalized)>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        let fake = Arc::new(Self::default());
        *locked(&fake.infos) = declared();
        for (id, value) in [(CUTOFF, 0.5), (LEVEL, 0.25), (MODE, 0.0), (LOCKED, 0.25)] {
            locked(&fake.values).insert(id, Normalized::of(value));
        }
        fake
    }

    fn range(&self, id: ParamId) -> Option<(f64, f64)> {
        locked(&self.infos)
            .iter()
            .find(|i| i.id() == id)
            .and_then(|i| i.bounds())
    }

    fn sets(&self) -> Vec<(ParamId, f64)> {
        locked(&self.sets)
            .iter()
            .map(|&(id, v)| (id, v.get()))
            .collect()
    }
}

impl AnyThread for Fake {
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
        Some(locked(&self.infos).clone())
    }
    fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
        locked(&self.values).get(&id).copied()
    }
    fn set_parameter_value(&self, id: ParamId, value: Normalized) {
        locked(&self.values).insert(id, value);
        locked(&self.sets).push((id, value));
    }
    fn parameter_plain(&self, id: ParamId, value: Normalized) -> Option<f64> {
        let (min, max) = self.range(id)?;
        Some(min + (max - min) * value.get())
    }
    fn parameter_normalized(&self, id: ParamId, plain: f64) -> Option<Normalized> {
        let (min, max) = self.range(id)?;
        Some(Normalized::clamped((plain - min) / (max - min)))
    }
    fn parameter_text(&self, _id: ParamId, value: Normalized) -> Option<String> {
        Some(format!("{:.2} units", value.get()))
    }
    fn parameter_value_from_text(&self, _id: ParamId, text: &str) -> Option<Normalized> {
        text.strip_suffix(" units")?
            .parse()
            .ok()
            .and_then(Normalized::new)
    }
    fn save_state(&self) -> Result<Vec<u8>, tutti_plugin::error::StateError> {
        Err(tutti_plugin::error::StateError::NoStateRoute)
    }
    fn set_render_mode(&self, _mode: RenderMode) -> bool {
        false
    }
    fn set_automation_mode(&self, _mode: AutomationMode) -> Result<(), AutomationModeError> {
        Err(AutomationModeError::Unsupported {
            format: "fake".into(),
        })
    }
}

impl Health for Fake {
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

/// A detached session with the fake hosted at a node, its parameters not yet
/// read, and the sink the fake announces a rescan through.
fn session() -> (Session, NodeKey, Arc<Fake>, RefreshSink) {
    let (s, key, fake, refresh) = session_keyed();
    (s, key.erase(), fake, refresh)
}

/// [`session`], with the key `host_plugin` returned.
fn session_keyed() -> (Session, Key<PluginNode>, Arc<Fake>, RefreshSink) {
    let fake = Fake::new();
    let refresh = RefreshSink::default();
    let handle = PluginHandle::from_backend(
        Arc::clone(&fake),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    )
    .with_refresh_sink(refresh.clone());
    let mut s = Session::builder()
        .spawn_with(|job| job.run())
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    let (node, _) = s.graph().add(ForkByClone(Const::mono(0.0)));
    let key = s
        .graph()
        .host_plugin(
            node,
            handle,
            tutti_plugin::catalog::PluginRef::file("fake.clap").expect("a plugin file"),
        )
        .expect("the node is in the graph");
    s.commit();
    (s, key, fake, refresh)
}

/// The plugin events among `events`.
fn plugin_events(events: Vec<SessionEvent>) -> Vec<PluginEvent> {
    events
        .into_iter()
        .filter_map(|event| match event {
            SessionEvent::Plugin(event) => Some(event),
            _ => None,
        })
        .collect()
}

/// An update that submits a parameter read (the spawner runs it inline) and
/// one that lands it; the plugin events both reported.
fn update(s: &mut Session) -> Vec<PluginEvent> {
    let mut reported = plugin_events(s.update());
    reported.extend(plugin_events(s.update()));
    reported
}

/// A session whose parameters are read.
fn ready() -> (Session, NodeKey, Arc<Fake>, RefreshSink) {
    let (mut s, node, fake, refresh) = session();
    let _ = update(&mut s);
    (s, node, fake, refresh)
}

fn cutoff(s: &Session, node: NodeKey) -> tutti_session::Param<'_> {
    s.plugin(node).unwrap().params().get(CUTOFF).unwrap()
}

fn changes(events: Vec<PluginEvent>) -> Vec<PluginChange> {
    events.into_iter().map(|e| e.change).collect()
}

/// **A gesture guard that is dropped in the middle of a drag ends the
/// gesture.** The hand lets go after two moves of four planned (the loop is cut
/// short by an early return): the session reports the begin, the two changes
/// and the end, in that order, and the plugin took the two values.
#[test]
fn a_gesture_guard_dropped_mid_drag_ends_the_gesture() {
    let (mut s, node, fake, _) = ready();
    let drag = |s: &Session| -> Result<(), &'static str> {
        let gesture = cutoff(s, node).edit().unwrap();
        for (i, v) in [0.1, 0.2, 0.3, 0.4].into_iter().enumerate() {
            if i == 2 {
                return Err("the hand was lifted");
            }
            gesture.set(Normalized::of(v)).unwrap();
        }
        Ok(())
    };
    assert_eq!(drag(&s), Err("the hand was lifted"));

    let heard = changes(update(&mut s));
    let [
        PluginChange::GestureBegan { id: began },
        PluginChange::ParamChanged {
            id: first_id,
            value: first,
            recorded: first_recorded,
        },
        PluginChange::ParamChanged {
            value: second,
            recorded: second_recorded,
            ..
        },
        PluginChange::GestureEnded { id: ended },
    ] = heard.as_slice()
    else {
        panic!("begin, the two changes and the end, in order: {heard:?}");
    };
    assert_eq!((*began, *first_id, *ended), (CUTOFF, CUTOFF, CUTOFF));
    assert_eq!((first.get(), second.get()), (0.1, 0.2));
    assert!(
        *first_recorded && *second_recorded,
        "a host's edit is recorded"
    );
    assert_eq!(
        fake.sets(),
        [(CUTOFF, 0.1), (CUTOFF, 0.2)],
        "the plugin took the two values"
    );
    assert_eq!(cutoff(&s, node).normalized(), Normalized::of(0.2));
}

/// **A gesture on a read-only parameter is refused before it begins**, so the
/// guard's own `set` has nothing to refuse and the stream shows no gesture.
#[test]
fn a_gesture_on_a_read_only_parameter_never_begins() {
    let (mut s, node, fake, _) = ready();
    let locked = s.plugin(node).unwrap().params().get(LOCKED).unwrap();
    assert!(matches!(
        locked.edit(),
        Err(PluginParamError::ReadOnly(id)) if id == LOCKED
    ));
    assert_eq!(
        locked.set(Normalized::of(1.0)),
        Err(PluginParamError::ReadOnly(LOCKED))
    );
    assert!(changes(update(&mut s)).is_empty());
    assert!(fake.sets().is_empty());
}

/// **`typed::<Db>` on a parameter that measures hertz is refused**, once, when
/// the handle is made; `typed::<Hz>` on it is accepted, and one on a parameter
/// that declares no unit is refused for that.
#[test]
fn a_typed_handle_of_the_wrong_unit_is_refused() {
    let (s, node, _, _) = ready();
    let params = s.plugin(node).unwrap().params();
    assert!(matches!(
        params.typed::<Db>(CUTOFF),
        Err(PluginParamError::UnitMismatch { id, declared: ParamUnit::Hertz, .. }) if id == CUTOFF
    ));
    assert!(matches!(
        params.typed::<Hz>(LEVEL),
        Err(PluginParamError::UnitMismatch {
            declared: ParamUnit::Decibels,
            ..
        })
    ));
    assert!(params.typed::<Hz>(CUTOFF).is_ok());
    assert!(params.typed::<Db>(LEVEL).is_ok());
    assert!(matches!(
        params.typed::<Hz>(MODE),
        Err(PluginParamError::NoUnit(id)) if id == MODE
    ));
}

/// **A typed parameter reads and writes in its unit through the plugin's own
/// map**: the fake's linear map over `20..=20000` is the oracle (`Hz(10010)` is
/// the middle, position `0.5`), a value outside the declared range is refused,
/// and the position the plugin was given is the one its map names.
#[test]
fn a_typed_parameter_goes_through_the_range_and_the_plugins_map() {
    let (s, node, fake, _) = ready();
    let cutoff = s
        .plugin(node)
        .unwrap()
        .params()
        .typed::<Hz>(CUTOFF)
        .unwrap();
    // The fake started it at 0.5: 20 + 19980 / 2.
    assert_eq!(cutoff.get(), Hz::new(10_010.0));

    cutoff.set(Hz::new(20.0 + 19_980.0 * 0.25)).unwrap();
    assert_eq!(fake.sets(), [(CUTOFF, 0.25)]);
    assert_eq!(cutoff.get(), Hz::new(5_015.0), "read-your-writes, in Hz");
    assert_eq!(cutoff.param().normalized(), Normalized::of(0.25));

    assert_eq!(
        cutoff.set(Hz::new(30_000.0)),
        Err(PluginParamError::OutOfRange(CUTOFF))
    );
    assert_eq!(
        cutoff.set(Hz::new(10.0)),
        Err(PluginParamError::OutOfRange(CUTOFF))
    );
    assert_eq!(fake.sets().len(), 1, "a refused value reaches nothing");
}

/// **An id the plugin does not declare is refused**, by `get` and by `typed`,
/// and before the parameters are read nothing resolves.
#[test]
fn an_unknown_id_is_refused_and_an_unread_plugin_resolves_nothing() {
    let (mut s, node, _, _) = session();
    let params = s.plugin(node).unwrap().params();
    assert!(!params.is_ready());
    assert!(matches!(params.get(CUTOFF), Err(PluginParamError::NotRead)));

    let _ = update(&mut s);
    let params = s.plugin(node).unwrap().params();
    assert!(params.is_ready());
    assert_eq!(params.len(), 4);
    let unknown = ParamId::new(99);
    assert!(matches!(
        params.get(unknown),
        Err(PluginParamError::Unknown(id)) if id == unknown
    ));
    assert!(matches!(
        params.typed::<Hz>(unknown),
        Err(PluginParamError::Unknown(id)) if id == unknown
    ));
}

/// **A rescan replaces what `params()` shows, and an id the plugin stopped
/// declaring is refused when it is resolved again.** The plugin announces a
/// rescan of its titles: until the read lands the old declarations stand, after
/// it the new ones do, together with their values. (A handle held across the
/// landing is a borrow error: see the module docs of `Params`.)
#[test]
fn a_rescan_replaces_the_declarations_and_the_values_with_them() {
    let (mut s, node, fake, refresh) = ready();
    assert!(s.plugin(node).unwrap().params().get(MODE).is_ok());

    // The plugin drops `mode` and gains `shape`, and moves `cutoff`.
    let shape = ParamId::new(7);
    *locked(&fake.infos) = vec![
        ParameterInfo::new(CUTOFF, "Cutoff")
            .with_plain_range(20.0, 20_000.0, 1_000.0)
            .with_measures(Some(ParamUnit::Hertz)),
        ParameterInfo::new(shape, "Shape").with_normalized_default(0.5),
    ];
    locked(&fake.values).insert(CUTOFF, Normalized::of(0.75));
    locked(&fake.values).insert(shape, Normalized::of(0.5));
    refresh.fire(PluginRefresh::ParamTitles);

    // The notice is heard by this update, which submits the read; the old
    // declarations stand until the read lands, in the next update.
    let mut events = plugin_events(s.update());
    assert!(
        s.plugin(node).unwrap().params().get(MODE).is_ok(),
        "not yet"
    );
    events.extend(plugin_events(s.update()));
    assert!(
        changes(events)
            .iter()
            .any(|c| matches!(c, PluginChange::ParamTitlesChanged))
    );

    let params = s.plugin(node).unwrap().params();
    assert!(matches!(params.get(MODE), Err(PluginParamError::Unknown(id)) if id == MODE));
    assert_eq!(params.get(shape).unwrap().normalized(), Normalized::of(0.5));
    assert_eq!(
        params.get(CUTOFF).unwrap().normalized(),
        Normalized::of(0.75),
        "the values are the rescan's"
    );
    assert_eq!(params.len(), 2);
}

/// **A value the host sets reads back at once**, the text and the plain value
/// go through the plugin, and a value set from text is the one the plugin read.
#[test]
fn text_and_plain_values_go_through_the_plugin() {
    let (s, node, fake, _) = ready();
    let level = s.plugin(node).unwrap().params().get(LEVEL).unwrap();
    assert_eq!(level.plain(), Some(-60.0 + 72.0 * 0.25));

    assert_eq!(level.set_plain(-24.0).unwrap(), Normalized::of(0.5));
    assert_eq!(level.normalized(), Normalized::of(0.5));
    assert_eq!(level.text().as_deref(), Some("0.50 units"));

    assert_eq!(level.set_text("0.75 units").unwrap(), Normalized::of(0.75));
    assert_eq!(level.normalized(), Normalized::of(0.75));
    assert_eq!(
        level.set_text("loud"),
        Err(PluginParamError::UnreadableText {
            id: LEVEL,
            text: "loud".into()
        })
    );
    assert_eq!(fake.sets(), [(LEVEL, 0.5), (LEVEL, 0.75)]);
}

/// **The automation mode has an error of its own**: the fake carries no such
/// channel, and says so with `Unsupported`, not an editor error.
#[test]
fn the_automation_mode_refusal_is_its_own_type() {
    let (s, node, _, _) = ready();
    assert_eq!(
        s.plugin(node)
            .unwrap()
            .params()
            .set_automation_mode(AutomationMode::Writing),
        Err(AutomationModeError::Unsupported {
            format: "fake".into()
        })
    );
}

/// **A plugin parameter is targeted by the same reference a native one is**
/// (`modulation`): `Param::target` is the `ParamRef` a route takes.
#[cfg(feature = "modulation")]
#[test]
fn a_plugin_parameter_targets_a_modulation_route_through_a_param_ref() {
    let (s, node, _, _) = ready();
    let level = s.plugin(node).unwrap().params().get(LEVEL).unwrap();
    assert_eq!(
        level.target(),
        tutti_session::ParamRef::plugin(node, LEVEL.get())
    );
}

/// **A plugin's key names a parameter as a route's target** (`modulation`):
/// `key.param(id)` is the reference `Param::target` gives, and a route onto it
/// is declared and read back through the key, as a native param's is.
#[cfg(feature = "modulation")]
#[test]
fn a_plugin_key_names_a_parameter_a_route_targets() {
    use tutti_session::{ParamRef, RouteSpec};
    use tutti_types::Depth;

    let (mut s, key, _, _) = session_keyed();
    let _ = update(&mut s);
    let target = key.param(LEVEL);
    assert_eq!(target, ParamRef::plugin(key, LEVEL.get()));
    assert_eq!(
        target,
        s.plugin(key).unwrap().params().get(LEVEL).unwrap().target()
    );
    let id = {
        let mut m = s.mods_mut();
        m.range_raw(target, 0.0, 1.0).unwrap();
        let source = m.macro_(0.5);
        m.route(RouteSpec::new(source, target, Depth::of(0.25)))
            .unwrap()
    };
    let routed: Vec<_> = s.mods().routes_to(target).map(|r| r.id).collect();
    assert_eq!(routed, [id]);
}

/// **A hosted plugin's key wires like a native node's**: `connect` takes the
/// key from `host_plugin` (and its ports) and the root, and the edge is in the
/// graph.
#[test]
fn a_hosted_plugin_key_wires_like_a_native_key() {
    let (mut s, key, _, _) = session_keyed();
    let _ = update(&mut s);
    s.graph()
        .connect(key.out(0), tutti_session::Root::output(0))
        .unwrap();
    let sources = s.graph().sources(tutti_session::Root::output(0)).unwrap();
    assert_eq!(sources, [key.out(0)]);
}

/// **A gesture outlives frames**: begun, then `update` runs (and a rescan lands),
/// then the hand moves, `update` runs again, then lets go. The stream holds
/// exactly one begin, the changes and one end, and `set` after the plugin is
/// removed is refused and sends nothing.
#[test]
fn a_gesture_spans_updates_and_refuses_once_the_plugin_is_gone() {
    let (mut s, node, fake, refresh) = ready();
    let gesture = cutoff(&s, node).edit().unwrap();
    let mut heard = changes(update(&mut s));
    // A rescan lands in between; the parameter is still declared.
    refresh.fire(PluginRefresh::ParamTitles);
    heard.extend(changes(update(&mut s)));
    gesture.set(Normalized::of(0.3)).unwrap();
    heard.extend(changes(update(&mut s)));
    gesture.set(Normalized::of(0.6)).unwrap();
    heard.extend(changes(update(&mut s)));
    drop(gesture);
    heard.extend(changes(update(&mut s)));

    let began = heard
        .iter()
        .filter(|c| matches!(c, PluginChange::GestureBegan { .. }))
        .count();
    let ended = heard
        .iter()
        .filter(|c| matches!(c, PluginChange::GestureEnded { .. }))
        .count();
    let moved: Vec<f64> = heard
        .iter()
        .filter_map(|c| match c {
            PluginChange::ParamChanged {
                value,
                recorded: true,
                ..
            } => Some(value.get()),
            _ => None,
        })
        .collect();
    assert_eq!((began, ended), (1, 1), "{heard:?}");
    assert_eq!(moved, [0.3, 0.6]);
    assert_eq!(fake.sets(), [(CUTOFF, 0.3), (CUTOFF, 0.6)]);

    // Gone: removed.
    let gesture = cutoff(&s, node).edit().unwrap();
    let _ = update(&mut s);
    s.remove(node).unwrap();
    assert_eq!(
        gesture.set(Normalized::of(0.9)),
        Err(PluginParamError::Gone(CUTOFF))
    );
    drop(gesture);
    assert_eq!(fake.sets().len(), 2, "nothing was sent blind");
    assert!(
        !changes(update(&mut s))
            .iter()
            .any(|c| matches!(c, PluginChange::GestureEnded { .. })),
        "a gone plugin hears no end"
    );
}

/// **A rescan that drops the parameter ends the hold**: `set` is refused.
#[test]
fn a_gesture_on_a_parameter_the_rescan_dropped_is_refused() {
    let (mut s, node, fake, refresh) = ready();
    let gesture = cutoff(&s, node).edit().unwrap();
    *locked(&fake.infos) = vec![ParameterInfo::new(MODE, "Mode").with_normalized_default(0.0)];
    refresh.fire(PluginRefresh::ParamTitles);
    let _ = update(&mut s);
    assert_eq!(
        gesture.set(Normalized::of(0.5)),
        Err(PluginParamError::Gone(CUTOFF))
    );
}
