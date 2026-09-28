//! Declaring where inbound MIDI goes.
//!
//! A route decides which node an event arriving from the hardware MIDI input
//! reaches. Anything already bound to a node (clip playback, a keyboard's
//! `LiveMidiInput`, a plugin's MIDI out declared with `EventSources`) is wired
//! to it directly and never consults a route.
//!
//! A rule is an entity:
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_ecs::prelude::*;
//! use bevy_tutti::midi::{MidiRouteFallback, MidiRouteRule};
//! use tutti_midi_types::MidiChannel;
//!
//! /// The three synth entities a host already has. Real ones carry `AudioNode`;
//! /// a rule names the entity either way, and `rebuild` resolves it.
//! #[derive(Resource, Clone, Copy)]
//! struct Synths { lead: Entity, pad: Entity, sampler: Entity }
//!
//! fn declare_routes(synths: Res<Synths>, mut commands: Commands) {
//!     // Channel 1 plays the lead synth and the pad at once.
//!     commands.spawn(MidiRouteRule::for_channel(MidiChannel::FIRST).to(synths.lead).to(synths.pad));
//!
//!     // Anything unmatched falls through to the sampler.
//!     commands.insert_resource(MidiRouteFallback(Some(synths.sampler)));
//! }
//!
//! let mut app = App::new();
//! let synths = Synths {
//!     lead: app.world_mut().spawn_empty().id(),
//!     pad: app.world_mut().spawn_empty().id(),
//!     sampler: app.world_mut().spawn_empty().id(),
//! };
//! app.insert_resource(synths);
//! app.add_systems(Startup, declare_routes);
//! app.update();
//!
//! // A rule is an entity, and it names entities rather than nodes — which is
//! // what keeps a `crossfade` from stranding it.
//! let rule = app.world_mut().query::<&MidiRouteRule>().single(app.world()).unwrap();
//! assert_eq!(rule.channel, Some(MidiChannel::FIRST));
//! assert_eq!(rule.targets, vec![synths.lead, synths.pad]);
//! assert_eq!(app.world().resource::<MidiRouteFallback>().0, Some(synths.sampler));
//! ```
//!
//! # Routing is wiring
//!
//! The hardware input is a graph node ([`MidiEngineNodes::input`]) with one
//! event output per channel and one for channelless messages (system, SysEx,
//! Flex). [`rebuild`] turns the rules into which of those ports each target's
//! event input takes (through [`EventFeeds`]): a channel rule gives its
//! targets that channel's port and the channelless one, an any-channel rule
//! all seventeen, several rules the union. The fallback takes every port no
//! rule covers — a channel no rule names, and the channelless port while no
//! rule is armed. A target is an entity, resolved to its node by the event
//! wiring every frame, so a `crossfade` strands nothing.
//!
//! [`MidiEngineNodes::input`]: crate::midi::MidiEngineNodes

use std::collections::{BTreeSet, HashMap, HashSet};

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use tutti_midi_runtime::{input_ports, MIDI_INPUT_PORTS};
use tutti_midi_types::MidiChannel;

use crate::graph::{engine_ready, EventFeeds, EventSource, EventWiring, GraphReconcileSystems};
use crate::midi::MidiEngineNodes;

/// One inbound routing rule: which channel reaches which entities.
///
/// Spawn one per rule. Several rules may name the same channel; every matching
/// rule's targets receive the event.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct MidiRouteRule {
    /// Channel filter: `None` matches any channel, `Some(n)` only channel `n`
    /// (0-15).
    pub channel: Option<MidiChannel>,
    /// The entities this rule feeds.
    pub targets: Vec<Entity>,
    /// A disabled rule stays declared but routes nothing — for a mute that does
    /// not lose the rule.
    pub enabled: bool,
}

impl MidiRouteRule {
    /// Creates a rule matching every channel. Add targets with [`to`](Self::to).
    pub fn any_channel() -> Self {
        Self {
            channel: None,
            targets: Vec::new(),
            enabled: true,
        }
    }

    /// Creates a rule matching one channel.
    pub fn for_channel(channel: MidiChannel) -> Self {
        Self {
            channel: Some(channel),
            targets: Vec::new(),
            enabled: true,
        }
    }

    /// Adds a destination.
    pub fn to(mut self, target: Entity) -> Self {
        self.targets.push(target);
        self
    }

    /// Declares the rule without arming it.
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

/// Where an event matching no rule goes, if anywhere.
///
/// `None` — the default — drops unmatched events, which is what a host wants
/// before it has declared anything: silence beats a note arriving at whichever
/// synth happened to spawn first.
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct MidiRouteFallback(pub Option<Entity>);

/// Who feeds a target through the route rules, in [`EventFeeds`].
const ROUTES: &str = "midi routes";

/// The targets [`rebuild`] fed last, so one no rule names any more is
/// unwired.
#[derive(Resource, Default)]
pub struct RoutedTargets(HashSet<Entity>);

/// Compiles every declared rule into which of the hardware input's ports each
/// target takes, and writes it into [`EventFeeds`] (see the module docs).
///
/// Runs when a rule changes, is added, or is removed, and when the fallback
/// changes: a whole recompile, since a target's ports are the union of every
/// rule naming it. A target with no node yet is wired as soon as it has one
/// (the event wiring resolves entities every frame).
pub fn rebuild(
    nodes: Option<Res<MidiEngineNodes>>,
    rules: Query<&MidiRouteRule>,
    fallback: Res<MidiRouteFallback>,
    changed: Query<(), Changed<MidiRouteRule>>,
    mut removed: RemovedComponents<MidiRouteRule>,
    mut feeds: ResMut<EventFeeds>,
    mut routed: ResMut<RoutedTargets>,
) {
    let dirty = !changed.is_empty()
        || !removed.is_empty()
        || fallback.is_changed()
        || nodes_changed(&nodes);
    // An event reader: draining is what marks this frame's removals as seen, so
    // it happens whether or not a rebuild follows.
    removed.clear();
    if !dirty {
        return;
    }
    let Some(nodes) = nodes else {
        return;
    };

    let mut ports: HashMap<Entity, BTreeSet<usize>> = HashMap::new();
    let mut covered: BTreeSet<usize> = BTreeSet::new();
    for rule in rules.iter().filter(|r| r.enabled && !r.targets.is_empty()) {
        let theirs: Vec<usize> = input_ports(rule.channel).collect();
        covered.extend(theirs.iter().copied());
        for &target in &rule.targets {
            ports
                .entry(target)
                .or_default()
                .extend(theirs.iter().copied());
        }
    }
    if let Some(target) = fallback.0 {
        let uncovered = (0..MIDI_INPUT_PORTS).filter(|p| !covered.contains(p));
        ports.entry(target).or_default().extend(uncovered);
    }

    let wanted: HashSet<Entity> = ports
        .iter()
        .filter(|(_, p)| !p.is_empty())
        .map(|(e, _)| *e)
        .collect();
    for gone in routed.0.difference(&wanted) {
        feeds.remove(*gone, ROUTES);
    }
    for (target, ports) in ports {
        if ports.is_empty() {
            continue;
        }
        let sources = ports
            .into_iter()
            .filter_map(|p| u16::try_from(p).ok())
            .map(|p| EventSource::new(nodes.input_node, p))
            .collect();
        feeds.set(target, ROUTES, sources);
    }
    routed.0 = wanted;
}

/// Whether the engine's MIDI nodes arrived (or changed) this frame: rules
/// declared before the engine built are compiled then.
fn nodes_changed(nodes: &Option<Res<MidiEngineNodes>>) -> bool {
    nodes.as_ref().is_some_and(|n| n.is_changed())
}

/// Declared MIDI routing: the rules, the fallback, and the rebuild that
/// publishes them.
///
/// [`rebuild`] runs before the event wiring, so a routing change reaches the
/// graph in the frame's commit, with any graph edit that motivated it.
pub struct MidiRoutePlugin;

impl Plugin for MidiRoutePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MidiRouteFallback>();
        app.init_resource::<RoutedTargets>();
        app.init_resource::<EventFeeds>();
        app.add_systems(
            Update,
            rebuild
                .after(GraphReconcileSystems::Spawn)
                .before(EventWiring)
                .run_if(engine_ready),
        );
    }
}
