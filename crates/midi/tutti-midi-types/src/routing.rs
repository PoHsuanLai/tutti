//! Pure MIDI routing rules and matching.
//!
//! Defines the data types ([`MidiRoute`], [`MidiRoutingSnapshot`]) and the
//! pure routing function ([`MidiRoutingSnapshot::route`]): given a set of
//! rules and an incoming event, produce an iterator of target unit IDs (routes
//! key on channel). No interior mutability, no allocations in the hot path, no
//! threading primitives.
//!
//! The mutable writer with atomic publishing ([`MidiRoutingTable`]) lives at
//! the bottom of this module. Audio-thread consumers typically hold an
//! `Arc<RtPublish<MidiRoutingSnapshot>>` and call `.load().route(&event)`.

use crate::ump::MidiEvent;
use crate::unit_id::MidiUnitId;
use std::sync::Arc;

use std::vec::Vec;
use tutti_types::RtPublish;

/// Maximum number of targets per routing rule.
/// Supports layering up to 8 synths on a single channel.
pub const MAX_TARGETS_PER_ROUTE: usize = 8;

/// One routing rule: which events it claims, and where they go.
///
/// A rule fans one event out to up to [`MAX_TARGETS_PER_ROUTE`] units. Several
/// rules may claim the same event; the snapshot unions their targets rather than
/// picking a winner, so layering is additive.
#[derive(Clone, Debug)]
pub struct MidiRoute {
    /// Channel filter: `None` = any channel, `Some(n)` = channel n only (0-15).
    ///
    /// Only constrains messages that *carry* a channel — see
    /// [`matches`](Self::matches).
    pub channel: Option<u8>,
    /// Units that receive matching events. Silently capped at
    /// [`MAX_TARGETS_PER_ROUTE`] by the builders.
    pub targets: Vec<MidiUnitId>,
    /// Whether this route is live. A disabled route matches nothing.
    pub enabled: bool,
}

impl MidiRoute {
    /// Builds an enabled route with no channel filter and no targets.
    ///
    /// Matches every event and delivers it nowhere until targets are added.
    pub fn new() -> Self {
        Self {
            channel: None,
            targets: Vec::new(),
            enabled: true,
        }
    }

    /// Builds an enabled route filtered to one channel, 0-indexed.
    pub fn for_channel(channel: u8) -> Self {
        Self {
            channel: Some(channel),
            targets: Vec::new(),
            enabled: true,
        }
    }

    /// Adds one target, **ignoring** the call once the route already holds
    /// [`MAX_TARGETS_PER_ROUTE`].
    ///
    /// The overflow is silent by design — a routing table is edited from a UI and
    /// dropping the surplus beats failing the whole edit — so a caller that needs
    /// to know must check `targets.len()` itself.
    pub fn with_target(mut self, unit_id: MidiUnitId) -> Self {
        if self.targets.len() < MAX_TARGETS_PER_ROUTE {
            self.targets.push(unit_id);
        }
        self
    }

    /// Adds targets until the route holds [`MAX_TARGETS_PER_ROUTE`], then stops.
    ///
    /// Truncates the tail of `unit_ids` silently, exactly as
    /// [`with_target`](Self::with_target) does.
    pub fn with_targets(mut self, unit_ids: &[MidiUnitId]) -> Self {
        for &id in unit_ids {
            if self.targets.len() >= MAX_TARGETS_PER_ROUTE {
                break;
            }
            self.targets.push(id);
        }
        self
    }

    /// Whether this route carries `event`.
    ///
    /// A channel filter can only exclude messages that *have* a channel. Only
    /// MIDI 1.0 and MIDI 2.0 Channel Voice messages (UMP types 0x2 / 0x4) do;
    /// Flex Data (tempo, time signature, key), UMP Stream, SysEx and utility
    /// messages are group- or stream-scoped, so there is nothing for a channel
    /// filter to compare against and they pass every enabled route.
    ///
    /// Treating a channelless message as a mismatch is the tempting error, and it
    /// is silent: a per-channel route stops seeing Set Tempo, and a clip's own
    /// tempo map never reaches a channel-scoped consumer.
    #[inline]
    pub fn matches(&self, event: &MidiEvent) -> bool {
        if !self.enabled {
            return false;
        }
        match (self.channel, event.channel()) {
            (None, _) => true,
            // Channelless message: no channel to filter on, so it passes.
            (Some(_), None) => true,
            (Some(want), Some(got)) => want == got,
        }
    }
}

impl Default for MidiRoute {
    fn default() -> Self {
        Self::new()
    }
}

/// Immutable, precomputed routing rules.
///
/// Built once from a `Vec<MidiRoute>` (see `from_routes`). The channel lookup
/// is a cache derived from the routes — rebuilding it on construction is the
/// only "work" this struct does. After that, `route()` is a pure read.
#[derive(Clone, Debug)]
pub struct MidiRoutingSnapshot {
    /// All routing rules
    /// Shared with the staging table rather than copied: `commit` can fire per
    /// frame while a user drags routing UI, and each `MidiRoute` owns a
    /// `targets` Vec, so copying cost 1 + N allocations every time. Read-only
    /// once published, which is what makes sharing safe.
    routes: Arc<Vec<MidiRoute>>,
    /// Precomputed channel→targets lookup (17 entries: 0-15 + "any channel")
    /// Index 16 is for routes that match any channel.
    channel_lookup: [Vec<MidiUnitId>; 17],
    /// Fallback target when no routes match
    fallback_target: Option<MidiUnitId>,
}

impl MidiRoutingSnapshot {
    /// Builds a snapshot with no routes and no fallback — every event is dropped.
    pub fn empty() -> Self {
        Self {
            routes: Arc::new(Vec::new()),
            channel_lookup: Default::default(),
            fallback_target: None,
        }
    }

    /// Builds a snapshot from `routes`, precomputing the per-channel lookup.
    ///
    /// This is where the allocation happens, on the control thread — after it,
    /// [`route`](Self::route) is a pure read and allocation-free, which is what
    /// makes the snapshot safe to hand to the audio thread through `RtPublish`.
    ///
    /// `fallback` receives events no enabled route claims; `None` drops them.
    /// Takes `impl Into<Arc<Vec<MidiRoute>>>` so a caller that already holds the
    /// shared list (the staging table, on every commit) passes it without a
    /// copy, while one building a list fresh still passes a plain `Vec`.
    pub fn from_routes(
        routes: impl Into<Arc<Vec<MidiRoute>>>,
        fallback: Option<MidiUnitId>,
    ) -> Self {
        let routes = routes.into();
        let mut snapshot = Self {
            routes,
            channel_lookup: Default::default(),
            fallback_target: fallback,
        };
        snapshot.rebuild_lookup();
        snapshot
    }

    fn rebuild_lookup(&mut self) {
        for lookup in self.channel_lookup.iter_mut() {
            lookup.clear();
        }

        for route in self.routes.iter() {
            if !route.enabled {
                continue;
            }

            let channel_idx = route.channel.map_or(16, |c| c as usize);
            for &target in &route.targets {
                if !self.channel_lookup[channel_idx].contains(&target) {
                    self.channel_lookup[channel_idx].push(target);
                }
            }
        }
    }

    /// Every unit `event` should be delivered to, deduplicated.
    ///
    /// Allocation-free — the iterator dedupes through a fixed 16-slot stack
    /// array, so this is the accessor to call from the audio thread. Past 16
    /// distinct targets the array stops recording and deduplication degrades: no
    /// target is lost, but one may be yielded twice.
    #[inline]
    pub fn route<'a>(&'a self, event: &'a MidiEvent) -> RouteIterator<'a> {
        RouteIterator {
            snapshot: self,
            event,
            phase: RoutePhase::ChannelLookup,
            target_idx: 0,
            channel_idx: 0,
            seen: [MidiUnitId::new(0); 16],
            seen_count: 0,
        }
    }

    /// The single highest-priority target for `event`, or `None` if nothing
    /// claims it and there is no fallback.
    ///
    /// For a monophonic consumer that cannot fan out. Prefer
    /// [`route`](Self::route) where layering matters — this discards every target
    /// after the first, and which one survives is the lookup's order, not a
    /// documented priority among equal routes.
    ///
    /// Allocation-free.
    #[inline]
    pub fn route_single(&self, event: &MidiEvent) -> Option<MidiUnitId> {
        // A channelless message (Flex Data, UMP Stream, SysEx, utility) has no
        // channel to filter on, so it belongs to every route — take the first
        // per-channel target rather than skipping the buckets entirely.
        match event.channel() {
            Some(channel) => {
                if let Some(&target) = self.channel_lookup[channel as usize].first() {
                    return Some(target);
                }
            }
            None => {
                if let Some(&target) = self.channel_lookup[..16].iter().flatten().next() {
                    return Some(target);
                }
            }
        }

        if let Some(&target) = self.channel_lookup[16].first() {
            return Some(target);
        }

        for route in self.routes.iter() {
            if route.matches(event) {
                if let Some(&target) = route.targets.first() {
                    return Some(target);
                }
            }
        }

        self.fallback_target
    }

    /// Every unit any enabled route can reach, deduplicated, plus the fallback.
    ///
    /// **Allocates** — a control-thread query (which units to instantiate, what to
    /// show in a UI), not something to call per event.
    pub fn all_targets(&self) -> Vec<MidiUnitId> {
        let mut targets = Vec::new();
        for route in self.routes.iter() {
            if !route.enabled {
                continue;
            }
            for &t in &route.targets {
                if !targets.contains(&t) {
                    targets.push(t);
                }
            }
        }
        if let Some(fb) = self.fallback_target {
            if !targets.contains(&fb) {
                targets.push(fb);
            }
        }
        targets
    }

    /// Reports whether this snapshot can deliver anywhere at all.
    ///
    /// True if any route exists — enabled or not — or a fallback is set, so this
    /// answers "is the table configured", not "will this event go somewhere".
    #[inline]
    pub fn has_routes(&self) -> bool {
        !self.routes.is_empty() || self.fallback_target.is_some()
    }

    /// The unit receiving events no enabled route claims, if one is set.
    #[inline]
    pub fn fallback(&self) -> Option<MidiUnitId> {
        self.fallback_target
    }
}

impl Default for MidiRoutingSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Copy, Debug)]
enum RoutePhase {
    ChannelLookup,
    AnyChannelLookup,
    Fallback,
    Done,
}

/// Iterator over the units an event routes to, yielded in lookup order.
///
/// Allocation-free: the deduplication buffer is a fixed 16-slot array on the
/// stack. See [`MidiRoutingSnapshot::route`] for what happens past 16.
pub struct RouteIterator<'a> {
    snapshot: &'a MidiRoutingSnapshot,
    event: &'a MidiEvent,
    phase: RoutePhase,
    target_idx: usize,
    /// Which per-channel bucket the channelless sweep is on. Unused when the
    /// event has a channel (that path reads exactly one bucket).
    channel_idx: usize,
    seen: [MidiUnitId; 16],
    seen_count: usize,
}

impl RouteIterator<'_> {
    #[inline]
    fn is_seen(&self, target: MidiUnitId) -> bool {
        self.seen[..self.seen_count].contains(&target)
    }

    #[inline]
    fn mark_seen(&mut self, target: MidiUnitId) {
        if self.seen_count < self.seen.len() {
            self.seen[self.seen_count] = target;
            self.seen_count += 1;
        }
    }
}

impl Iterator for RouteIterator<'_> {
    type Item = MidiUnitId;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.phase {
                RoutePhase::ChannelLookup => {
                    // A channel filter can only exclude a message that has a
                    // channel. Flex Data (tempo/time-signature/key), UMP Stream,
                    // SysEx and utility messages are group- or stream-scoped, so
                    // they belong to *every* enabled route — including the
                    // per-channel ones, which is how a clip's tempo map reaches
                    // a channel-scoped consumer.
                    match self.event.channel() {
                        Some(channel) => {
                            let targets = &self.snapshot.channel_lookup[channel as usize];
                            while self.target_idx < targets.len() {
                                let target = targets[self.target_idx];
                                self.target_idx += 1;
                                if !self.is_seen(target) {
                                    self.mark_seen(target);
                                    return Some(target);
                                }
                            }
                        }
                        None => {
                            // Walk every per-channel bucket; `seen` dedupes a
                            // target that several channels route to.
                            while self.channel_idx < 16 {
                                let targets = &self.snapshot.channel_lookup[self.channel_idx];
                                while self.target_idx < targets.len() {
                                    let target = targets[self.target_idx];
                                    self.target_idx += 1;
                                    if !self.is_seen(target) {
                                        self.mark_seen(target);
                                        return Some(target);
                                    }
                                }
                                self.channel_idx += 1;
                                self.target_idx = 0;
                            }
                        }
                    }
                    self.target_idx = 0;
                    self.phase = RoutePhase::AnyChannelLookup;
                }
                RoutePhase::AnyChannelLookup => {
                    let targets = &self.snapshot.channel_lookup[16];
                    while self.target_idx < targets.len() {
                        let target = targets[self.target_idx];
                        self.target_idx += 1;
                        if !self.is_seen(target) {
                            self.mark_seen(target);
                            return Some(target);
                        }
                    }
                    self.target_idx = 0;
                    self.phase = RoutePhase::Fallback;
                }
                RoutePhase::Fallback => {
                    self.phase = RoutePhase::Done;
                    if self.seen_count == 0 {
                        if let Some(target) = self.snapshot.fallback_target {
                            return Some(target);
                        }
                    }
                }
                RoutePhase::Done => {
                    return None;
                }
            }
        }
    }
}

/// UI-thread writer for MIDI routing configuration.
///
/// The mutable writer that publishes [`MidiRoutingSnapshot`]s atomically to
/// the audio thread via [`RtPublish`]. Stage the full rule set with
/// [`set_routes`](MidiRoutingTable::set_routes) from the UI thread, then
/// [`commit`](MidiRoutingTable::commit) to publish. The audio thread reads via
/// the `Arc<RtPublish<MidiRoutingSnapshot>>` returned by
/// [`snapshot_arc`](MidiRoutingTable::snapshot_arc).
pub struct MidiRoutingTable {
    routes: Arc<Vec<MidiRoute>>,
    fallback_target: Option<MidiUnitId>,
    snapshot: Arc<RtPublish<MidiRoutingSnapshot>>,
    dirty: bool,
}

impl MidiRoutingTable {
    /// Builds a table with no routes, having already published an empty
    /// snapshot — so an audio thread reading before the first
    /// [`commit`](Self::commit) sees a valid table that routes nowhere, never an
    /// uninitialised one.
    pub fn new() -> Self {
        let snapshot = MidiRoutingSnapshot::empty();
        Self {
            routes: Arc::new(Vec::new()),
            fallback_target: None,
            snapshot: Arc::new(RtPublish::new(snapshot)),
            dirty: false,
        }
    }

    /// The published cell, for handing to the audio thread.
    ///
    /// Clone this once at setup; the audio thread then calls
    /// [`RtPublish::read`] on it per block. This is the *cell*, not the snapshot —
    /// handing over an owning snapshot instead would let the audio thread free a
    /// retired one inside the callback.
    pub fn snapshot_arc(&self) -> Arc<RtPublish<MidiRoutingSnapshot>> {
        self.snapshot.clone()
    }

    /// Borrows the currently published snapshot.
    ///
    /// Read once per block and never park the returned `RtRef` — its lifetime is
    /// what keeps the audio thread from holding an owning handle across a
    /// [`commit`](Self::commit).
    #[inline]
    pub fn load(&self) -> tutti_types::RtRef<'_, MidiRoutingSnapshot> {
        self.snapshot.read()
    }

    /// Stage a complete replacement of the routing rules.
    ///
    /// Callers rebuild the full rule set from their source of truth (e.g.
    /// `MidiReceiver` components) and hand it over wholesale; there is no
    /// incremental edit. The staged rules are published to the audio thread
    /// on the next [`commit`](Self::commit), which coalesces with the graph
    /// net flush so routes and topology flip atomically.
    pub fn set_routes(
        &mut self,
        routes: impl IntoIterator<Item = MidiRoute>,
        fallback: Option<MidiUnitId>,
    ) {
        self.routes = Arc::new(routes.into_iter().collect());
        self.fallback_target = fallback;
        self.dirty = true;
    }

    /// How many rules are *staged*, including any not yet committed.
    ///
    /// Not necessarily what the audio thread is currently routing on — compare
    /// [`is_dirty`](Self::is_dirty).
    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

    /// Whether staged rules are waiting for a [`commit`](Self::commit).
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Creates snapshot and atomically swaps. Audio thread sees new config on next load().
    pub fn commit(&mut self) {
        if !self.dirty {
            return;
        }

        let snapshot =
            MidiRoutingSnapshot::from_routes(Arc::clone(&self.routes), self.fallback_target);
        self.snapshot.publish(Arc::new(snapshot));
        self.dirty = false;
    }
}

impl Default for MidiRoutingTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::{MidiChannel, MidiGroup};

    const fn id(n: u64) -> MidiUnitId {
        MidiUnitId::new(n)
    }

    fn note_on(channel: u8, note: u8) -> MidiEvent {
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(channel), note, 0x8000)
    }

    /// A commit publishes the staged routes without copying them.
    ///
    /// `commit` can fire per frame while a user drags routing UI, and each
    /// `MidiRoute` owns a `targets` Vec — so copying was 1 + N allocations
    /// every time. Asserted with `Arc::ptr_eq` against the table's own list,
    /// the only observable that separates sharing from an equal copy.
    #[test]
    fn a_commit_publishes_the_staged_routes_without_copying_them() {
        let mut table = MidiRoutingTable::new();
        table.set_routes([MidiRoute::new().with_targets(&[id(1)])], Some(id(9)));
        table.commit();

        let published = table.snapshot_arc();
        let guard = published.read();
        assert!(
            Arc::ptr_eq(&guard.routes, &table.routes),
            "the published snapshot must share the staged list, not copy it"
        );

        // And it still routes: sharing must not have skipped the lookup build.
        let targets: Vec<_> = guard.route(&note_on(0, 60)).collect();
        assert_eq!(targets, vec![id(1)]);
    }

    #[test]
    fn test_empty_routing() {
        let snapshot = MidiRoutingSnapshot::empty();
        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(&event).collect();
        assert!(targets.is_empty());
    }

    #[test]
    fn test_fallback_routing() {
        let snapshot = MidiRoutingSnapshot::from_routes(Vec::new(), Some(id(42)));
        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(&event).collect();
        assert_eq!(targets, vec![id(42)]);
    }

    #[test]
    fn test_channel_routing() {
        let routes = vec![
            MidiRoute::for_channel(0).with_target(id(100)),
            MidiRoute::for_channel(1).with_target(id(200)),
        ];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, None);

        // Channel 0 → unit 100
        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(&event).collect();
        assert_eq!(targets, vec![id(100)]);

        // Channel 1 → unit 200
        let event = note_on(1, 60);
        let targets: Vec<_> = snapshot.route(&event).collect();
        assert_eq!(targets, vec![id(200)]);

        // Channel 2 → no targets
        let event = note_on(2, 60);
        let targets: Vec<_> = snapshot.route(&event).collect();
        assert!(targets.is_empty());
    }

    #[test]
    fn test_channel_layering() {
        let routes = vec![MidiRoute::for_channel(0).with_targets(&[id(100), id(200), id(300)])];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, None);

        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(&event).collect();
        assert_eq!(targets, vec![id(100), id(200), id(300)]);
    }

    #[test]
    fn test_global_layer() {
        let routes = vec![MidiRoute::new().with_targets(&[id(100), id(200)])];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, None);

        // Any channel, any port → both units
        let event = note_on(5, 60);
        let targets: Vec<_> = snapshot.route(&event).collect();
        assert_eq!(targets, vec![id(100), id(200)]);
    }

    #[test]
    fn test_route_single() {
        let routes = vec![MidiRoute::for_channel(0).with_target(id(100))];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, Some(id(999)));

        // Channel 0 → 100 (via route)
        let event = note_on(0, 60);
        assert_eq!(snapshot.route_single(&event), Some(id(100)));

        // Channel 5 → 999 (via fallback)
        let event = note_on(5, 60);
        assert_eq!(snapshot.route_single(&event), Some(id(999)));
    }

    #[test]
    fn test_no_duplicate_targets() {
        let routes = vec![
            MidiRoute::for_channel(0).with_target(id(100)),
            MidiRoute::new().with_targets(&[id(100), id(200)]),
        ];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, None);

        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(&event).collect();

        // Should not have duplicates
        assert_eq!(targets.len(), 2);
        assert!(targets.contains(&id(100)));
        assert!(targets.contains(&id(200)));
    }

    #[test]
    fn test_all_targets_for_system_messages() {
        let routes = vec![
            MidiRoute::for_channel(0).with_target(id(100)),
            MidiRoute::for_channel(1).with_target(id(200)),
            MidiRoute::new().with_target(id(300)),
        ];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, Some(id(999)));

        let targets = snapshot.all_targets();
        assert!(targets.contains(&id(100)));
        assert!(targets.contains(&id(200)));
        assert!(targets.contains(&id(300)));
        assert!(targets.contains(&id(999)));
        assert_eq!(targets.len(), 4);
    }

    #[test]
    fn channelless_messages_reach_channel_scoped_routes() {
        // A channel filter can only exclude a message that has a channel. Flex
        // Data, UMP Stream, SysEx and utility messages have none, so a
        // per-channel route must still receive them — otherwise a clip's Set
        // Tempo never reaches a channel-scoped consumer.
        let routes = vec![
            MidiRoute::for_channel(0).with_target(id(100)),
            MidiRoute::for_channel(1).with_target(id(200)),
        ];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, None);

        let tempo = MidiEvent::flex_set_tempo(MidiGroup::FIRST, 128.0);
        assert_eq!(tempo.channel(), None, "Set Tempo carries no channel");

        let targets: Vec<_> = snapshot.route(&tempo).collect();
        assert!(targets.contains(&id(100)), "channel-0 route gets the tempo");
        assert!(targets.contains(&id(200)), "channel-1 route gets the tempo");
        assert_eq!(targets.len(), 2, "each target exactly once");
        assert!(snapshot.route_single(&tempo).is_some());

        // A channel-voice message is still filtered by channel.
        let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 60, 0x8000);
        let targets: Vec<_> = snapshot.route(&note).collect();
        assert_eq!(
            targets,
            vec![id(200)],
            "note on ch1 goes only to ch1's route"
        );
    }

    #[test]
    fn test_all_targets_no_duplicates() {
        let routes = vec![
            MidiRoute::for_channel(0).with_target(id(100)),
            MidiRoute::new().with_targets(&[id(100), id(200)]),
        ];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, Some(id(100)));

        let targets = snapshot.all_targets();
        let count_100 = targets.iter().filter(|&&t| t == id(100)).count();
        assert_eq!(count_100, 1);
    }

    #[test]
    fn test_fallback_through_table() {
        let mut table = MidiRoutingTable::new();
        table.set_routes([], Some(id(42)));
        table.commit();

        let snapshot = table.load();
        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(&event).collect();
        assert_eq!(targets, vec![id(42)]);
    }

    #[test]
    fn test_channel_through_table() {
        let mut table = MidiRoutingTable::new();
        table.set_routes(
            [
                MidiRoute::for_channel(0).with_target(id(100)),
                MidiRoute::for_channel(1).with_target(id(200)),
            ],
            None,
        );
        table.commit();

        let snapshot = table.load();
        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(&event).collect();
        assert_eq!(targets, vec![id(100)]);
    }

    #[test]
    fn test_set_routes_replaces_wholesale() {
        let mut table = MidiRoutingTable::new();
        table.set_routes(
            [MidiRoute::for_channel(0).with_targets(&[id(100), id(200)])],
            Some(id(100)),
        );
        table.commit();

        // A second set_routes fully replaces the prior rules — no merge.
        table.set_routes([MidiRoute::for_channel(0).with_target(id(200))], None);
        table.commit();

        let snapshot = table.load();
        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(&event).collect();
        assert_eq!(targets, vec![id(200)]);
        assert_eq!(snapshot.fallback(), None);
    }

    #[test]
    fn test_dirty_flag() {
        let mut table = MidiRoutingTable::new();
        assert!(!table.is_dirty());

        table.set_routes([MidiRoute::for_channel(0).with_target(id(100))], None);
        assert!(table.is_dirty());

        table.commit();
        assert!(!table.is_dirty());
    }

    /// Two tables do not share a snapshot — the hazard every wrapper of this
    /// type has to guard.
    ///
    /// A host that builds a second `MidiRoutingTable` instead of using the one
    /// the RT pre-block was assembled with gets a `commit()` that publishes
    /// into a cell nothing reads: every hardware MIDI event is dropped,
    /// silently, with nothing in the log. `bevy_tutti::midi::MidiRoutingRes`
    /// makes that a compile error by keeping its field private, and this is the
    /// behavioural half of that guarantee — it pins *why* the guard is needed,
    /// where the type lives.
    #[test]
    fn two_tables_do_not_share_a_snapshot() {
        let rt_table = MidiRoutingTable::new();
        let rt_view = rt_table.snapshot_arc();

        // The mistake: a fresh table rather than the one the pre-block shares.
        let mut orphan = MidiRoutingTable::new();
        let unit = id(9);
        orphan.set_routes(vec![MidiRoute::for_channel(3).with_target(unit)], None);
        orphan.commit();

        let snapshot = rt_view.read();
        let targets: Vec<MidiUnitId> = snapshot
            .route(&crate::ump::MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::new(3),
                60,
                0x8000,
            ))
            .collect();
        assert!(
            !targets.contains(&unit),
            "an orphaned table must not appear to work — this is the silent failure"
        );
    }
}
