//! The source registry: which modulator kinds this app can build.
//!
//! The mirror of [`ModTargetRegistry`](super::ModTargetRegistry), for the send
//! half. A target is registered by *node type* and resolved by downcast; a
//! source is registered by **component**, and its builder is a plain
//! constructor — [`Sourced<M>`](tutti_mod::Sourced) erases `M` at construction,
//! so nothing downstream ever recovers the concrete type.
//!
//! ```rust,ignore
//! app.add_mod_source::<Lfo>();          // built in
//! app.add_mod_source::<StepSequencer>(); // yours
//! ```
//!
//! # Why a component, not a kind enum
//!
//! Different modulators want different parameters — an LFO a shape, a step
//! sequencer a pattern, an envelope follower attack/release. A single
//! `ModSource { kind, .. }` would have to carry the union of every kind's
//! config and leave most fields meaningless for any given one. A component per
//! kind keeps each modulator's parameters typed, individually
//! change-detectable, and reflectable — the same reasoning that already keeps
//! [`ModRate`](super::ModRate) off `ModSource`.
//!
//! [`ModRate`] stays shared: *how a source derives phase from the transport* is
//! a property of being a source at all, not of which kind it is.

use bevy_app::App;
use bevy_ecs::prelude::*;

use tutti_mod::{ErasedModulator, Modulator, SourceRate};

use crate::modulation::components::ModRate;

/// A component that describes how to build one kind of modulator.
///
/// Implement it on the component carrying that modulator's parameters; the
/// component *is* the authored declaration, and [`build`](Self::build) turns it
/// into the engine object.
pub trait ModSourceKind: Component + Sized {
    /// The modulator this component builds.
    type Source: Modulator + Send + Sync + 'static;

    /// Construct the modulator from the authored parameters.
    ///
    /// Rate is deliberately absent: it arrives from the entity's [`ModRate`]
    /// and is applied by the caller, so a kind cannot accidentally own two
    /// notions of frequency.
    fn build(&self) -> Self::Source;
}

/// Every source declared this frame, and whether any of them changed.
///
/// Filled by one `collect::<K>` system per registered kind, then read by
/// `rebuild`. The indirection is what keeps `rebuild` a normal system: a kind's
/// component type cannot appear in `rebuild`'s signature — that is the whole
/// point of the registry — but it can appear in a system of the kind's own,
/// scheduled by `add_mod_source`.
#[derive(Resource, Default)]
pub struct CollectedModSources {
    pub(crate) sources: Vec<(Entity, Box<dyn ErasedModulator>)>,
    /// Set when any kind component or its rate changed, so `rebuild` knows to
    /// recompile without naming a kind type.
    pub(crate) dirty: bool,
}

impl CollectedModSources {
    /// How many sources the registered kinds built this frame.
    ///
    /// One per source entity. A count higher than the number of source
    /// entities means a kind was registered twice, or an entity carries two
    /// kind components — both cases where a route would bind to only one of
    /// the duplicates.
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

/// Drop any sources a previous frame built but no rebuild consumed.
///
/// Runs ahead of [`mark_dirty`]. It deliberately does **not** clear `dirty`:
/// that flag is cleared by `rebuild` once it has actually recompiled, so a
/// frame where the engine is not yet ready leaves the pending change standing
/// rather than losing it.
pub(crate) fn clear_collected(mut collected: ResMut<CollectedModSources>) {
    collected.sources.clear();
}

/// Report whether kind `K`'s declaration moved this frame.
///
/// Split from [`collect`] and scheduled ahead of it because **building a source
/// is not free of consequence**: a fresh [`Sourced`](tutti_mod::Sourced) starts
/// at phase zero, so constructing one per frame would restart every modulator
/// sixty times a second. The build must happen only when a rebuild will
/// actually consume it, which means the dirty answer has to exist first.
#[allow(
    clippy::type_complexity,
    reason = "Bevy queries are tuple-shaped by design"
)]
fn mark_dirty<K: ModSourceKind>(
    mut collected: ResMut<CollectedModSources>,
    changed: Query<Entity, Or<(Changed<K>, Changed<ModRate>)>>,
    mut removed: RemovedComponents<K>,
) {
    // Draining the reader marks this frame's removals as seen either way.
    let any_removed = removed.read().next().is_some();
    for _ in removed.read() {}
    if !changed.is_empty() || any_removed {
        collected.dirty = true;
    }
}

/// Build every source declared by kind `K`, but only into a pending rebuild.
///
/// One instance per registered kind. `K` appears only here and in
/// [`mark_dirty`] — systems the kind's own registration scheduled — which is
/// how `rebuild` stays free of every modulator type.
fn collect<K: ModSourceKind>(
    mut collected: ResMut<CollectedModSources>,
    sources: Query<(Entity, &K, &ModRate)>,
) {
    if !collected.dirty {
        return;
    }
    for (entity, kind, rate) in &sources {
        let source: Box<dyn ErasedModulator> =
            Box::new(tutti_mod::Sourced::new(kind.build(), source_rate(rate)));
        collected.sources.push((entity, source));
    }
}

/// Registers a modulator kind.
pub trait ModSourceAppExt {
    /// Let source entities carrying `K` be built into modulators.
    ///
    /// Idempotent: registering a kind twice schedules one collector. Two would
    /// each push a source for the same entity, and the routing table would bind
    /// every route to only the first of them.
    fn add_mod_source<K: ModSourceKind>(&mut self) -> &mut Self;
}

impl ModSourceAppExt for App {
    fn add_mod_source<K: ModSourceKind>(&mut self) -> &mut Self {
        if !self
            .world_mut()
            .get_resource_or_init::<RegisteredModSources>()
            .0
            .insert(core::any::TypeId::of::<K>())
        {
            return self;
        }
        self.add_systems(
            bevy_app::Update,
            (
                mark_dirty::<K>.in_set(ModSourceSystems::MarkDirty),
                collect::<K>.in_set(ModSourceSystems::Collect),
            ),
        )
    }
}

/// The two per-kind phases, ordered ahead of the rebuild that consumes them.
///
/// Separate sets because the second is conditional on the first: `MarkDirty`
/// answers "did anything move?", and only then does `Collect` pay to build.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModSourceSystems {
    /// Every registered kind reports whether its declaration changed.
    MarkDirty,
    /// Every registered kind builds its sources, if a rebuild is pending.
    Collect,
}

/// Which kinds are already registered, so `add_mod_source` can be idempotent.
#[derive(Resource, Default)]
struct RegisteredModSources(std::collections::HashSet<core::any::TypeId>);

/// Turn a [`ModRate`] component into the engine's [`SourceRate`].
pub(crate) fn source_rate(rate: &ModRate) -> SourceRate {
    if rate.beat_synced {
        SourceRate::beat_synced(rate.frequency, rate.phase_offset)
    } else {
        SourceRate::free_running(rate.frequency, rate.phase_offset)
    }
}
