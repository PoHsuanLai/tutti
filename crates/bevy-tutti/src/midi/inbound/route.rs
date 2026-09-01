//! Declaring where inbound MIDI goes.
//!
//! The routing table decides which unit an event arriving from *outside* the
//! app reaches — hardware in, or a plugin's MIDI-out re-entering as if it were a
//! device. Anything already bound to a unit (clip playback, a preview, musical
//! typing) writes to that unit's port directly and never consults a route.
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
//! // A rule is an entity, and it names entities rather than engine ids — which
//! // is what keeps a `crossfade` from stranding it.
//! let rule = app.world_mut().query::<&MidiRouteRule>().single(app.world()).unwrap();
//! assert_eq!(rule.channel, Some(MidiChannel::FIRST));
//! assert_eq!(rule.targets, vec![synths.lead, synths.pad]);
//! assert_eq!(app.world().resource::<MidiRouteFallback>().0, Some(synths.sampler));
//! ```
//!
//! # Entities, not unit ids
//!
//! A rule names the *entity* it feeds and [`rebuild`] resolves that to a
//! [`MidiUnitId`] each time it runs, for the reason
//! [`target`](crate::midi::endpoint::target) documents at length: a `crossfade` replaces a
//! node's unit while keeping its `NodeId`, so any id stored on an entity is
//! silently stale from that moment on. Re-deriving is immune, and it means an
//! app never handles an engine id.
//!
//! # Why the whole table, every time
//!
//! [`set_routes`](tutti_midi_types::MidiRoutingTable::set_routes) takes the rule
//! set wholesale — the engine offers no incremental edit, deliberately, since a
//! partially-applied routing change is a state the audio thread must never
//! observe. So [`rebuild`] collects every rule and replaces the table, the same
//! collect-and-replace the modulation driver does with its matrix.
//!
//! That also means a rule whose target has not resolved *yet* is skipped rather
//! than dropped: the rebuild runs again next frame and picks it up once the node
//! exists.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;

use tutti_midi_types::MAX_TARGETS_PER_ROUTE;
use tutti_midi_types::{MidiChannel, MidiRoute, MidiUnitId};

use super::routing_table::MidiRoutingRes;
use crate::graph::{engine_ready, GraphReconcileSystems};
use crate::midi::endpoint::target::MidiTargetResolver;

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
    ///
    /// The engine caps a single rule at
    /// [`MAX_TARGETS_PER_ROUTE`] (8); targets past that are dropped when the
    /// rule is compiled, with a warning naming the rule.
    pub targets: Vec<Entity>,
    /// A disabled rule stays declared but routes nothing — for a mute that does
    /// not lose the rule.
    pub enabled: bool,
}

impl MidiRouteRule {
    /// A rule matching every channel. Add targets with [`to`](Self::to).
    pub fn any_channel() -> Self {
        Self {
            channel: None,
            targets: Vec::new(),
            enabled: true,
        }
    }

    /// A rule matching one channel.
    pub fn for_channel(channel: MidiChannel) -> Self {
        Self {
            channel: Some(channel),
            targets: Vec::new(),
            enabled: true,
        }
    }

    /// Add a destination.
    pub fn to(mut self, target: Entity) -> Self {
        self.targets.push(target);
        self
    }

    /// Declare the rule without arming it.
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

/// Compile every declared rule into the routing table and publish it.
///
/// Runs when a rule changes, is added, or is removed, and when the fallback
/// changes. It is a whole-table replace, so it must see every rule each time,
/// not only the changed ones — the `Query` is unfiltered and the change
/// detection only decides *whether* to run.
///
/// Unresolvable targets are skipped silently, as everywhere else in this
/// subsystem: an entity's node routinely materialises a frame after the entity
/// does, and a log line here would fire on every such frame. A rule whose
/// targets all fail to resolve contributes nothing this frame and is retried on
/// the next.
///
/// # What counts as a change
///
/// A rule edit, a rule removal, or a fallback edit — and also a *new
/// `AudioNode`*, which is the non-obvious one. A rule may name an entity whose
/// node does not exist yet; nothing about the rule changes when it finally
/// arrives, so without watching for that the rule would stay unresolved until
/// something unrelated happened to touch it. `Added<AudioNode>` is the signal
/// that a previously-skipped target may now resolve.
pub fn rebuild(
    // `Option`: the routing table is inserted by `engine::build_into`, but the
    // `engine_ready` gate only reads `AudioEngineState`. A host that declares the
    // engine up without running the build has no table, and a hard `ResMut`
    // panics the schedule rather than skipping the rebuild.
    table: Option<ResMut<MidiRoutingRes>>,
    resolver: MidiTargetResolver,
    rules: Query<(Entity, &MidiRouteRule)>,
    fallback: Res<MidiRouteFallback>,
    changed: Query<(), Changed<MidiRouteRule>>,
    arrived: Query<(), Added<tutti_core::AudioNode>>,
    mut removed: RemovedComponents<MidiRouteRule>,
) {
    let dirty =
        !changed.is_empty() || !removed.is_empty() || !arrived.is_empty() || fallback.is_changed();
    // An event reader: draining is what marks this frame's removals as seen, so
    // it happens whether or not a rebuild follows.
    removed.clear();
    if !dirty {
        return;
    }
    let Some(mut table) = table else {
        return;
    };

    let mut compiled: Vec<MidiRoute> = Vec::new();
    for (rule_entity, rule) in rules.iter() {
        if !rule.enabled {
            continue;
        }
        let mut route = match rule.channel {
            Some(channel) => MidiRoute::for_channel(channel),
            None => MidiRoute::new(),
        };
        let mut resolved = 0;
        for &target in &rule.targets {
            if resolved == MAX_TARGETS_PER_ROUTE {
                bevy_log::warn!(
                    "MIDI route {rule_entity:?} declares more than {MAX_TARGETS_PER_ROUTE} \
                     targets; the rest are dropped"
                );
                break;
            }
            let Some(port) = resolver.port(target) else {
                continue;
            };
            route = route.with_target(port.unit_id());
            resolved += 1;
        }
        if resolved == 0 {
            // Every target unresolved: an empty rule would match events and
            // deliver them nowhere, which is indistinguishable from a drop but
            // shadows a later rule. Leave it out and retry next frame.
            continue;
        }
        compiled.push(route);
    }

    let fallback_id: Option<MidiUnitId> = fallback
        .0
        .and_then(|entity| resolver.port(entity))
        .map(|port| port.unit_id());

    table.publish(compiled, fallback_id);
}

/// Declared MIDI routing: the rules, the fallback, and the rebuild that
/// publishes them.
///
/// [`rebuild`] runs before `Commit` so a routing change and the graph edit that
/// motivated it reach the audio thread together — the engine's `set_routes`
/// documents that coalescing as the point of deferring the publish. It runs
/// after `Spawn` for the same reason
/// [`register_midi_senders`](crate::midi::endpoint::registration::register_midi_senders) does: a node must be
/// in the graph before it can be asked for its port.
pub struct MidiRoutePlugin;

impl Plugin for MidiRoutePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MidiRouteFallback>();
        app.add_systems(
            Update,
            rebuild
                .after(GraphReconcileSystems::Spawn)
                .before(GraphReconcileSystems::Commit)
                // Resolution reads the audio graph, and the table it publishes
                // into is the RT pre-block's — neither means anything without a
                // running engine.
                .run_if(engine_ready),
        );
    }
}
