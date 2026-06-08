//! Pure MIDI routing rules and matching.
//!
//! Defines the data types ([`MidiRoute`], [`MidiRoutingSnapshot`]) and the
//! pure routing function ([`MidiRoutingSnapshot::route`]): given a set of
//! rules and an incoming `(port, event)`, produce an iterator of target unit
//! IDs. No interior mutability, no allocations in the hot path, no threading
//! primitives.
//!
//! The mutable writer with atomic publishing ([`MidiRoutingTable`]) lives at
//! the bottom of this module. Audio-thread consumers typically hold an
//! `Arc<ArcSwap<MidiRoutingSnapshot>>` and call `.load().route(port, &event)`.

use crate::compat::Vec;
use crate::ump::MidiEvent;
use crate::unit_id::MidiUnitId;
use alloc::sync::Arc;
use arc_swap::ArcSwap;

/// Maximum number of targets per routing rule.
/// Supports layering up to 8 synths on a single channel.
pub const MAX_TARGETS_PER_ROUTE: usize = 8;

/// Extract the channel nibble from a channel-voice UMP event.
///
/// Returns `None` for non-channel-voice messages (system, sysex, utility).
/// Used on the hot path to route by channel without paying for a full
/// `midi2::UmpMessage::try_from` dispatch.
#[inline]
fn event_channel(event: &MidiEvent) -> Option<u8> {
    let type_nibble = (event.data[0] >> 28) & 0x0F;
    // UMP type 0x2 = MIDI 1.0 channel voice, 0x4 = MIDI 2.0 channel voice.
    if type_nibble == 0x2 || type_nibble == 0x4 {
        Some(((event.data[0] >> 16) & 0x0F) as u8)
    } else {
        None
    }
}

#[derive(Clone, Debug)]
pub struct MidiRoute {
    /// Port filter: `None` = any port, `Some(n)` = port n only
    pub port: Option<usize>,
    /// Channel filter: `None` = any channel, `Some(n)` = channel n only (0-15)
    pub channel: Option<u8>,
    /// Target unit IDs to receive matching events
    pub targets: Vec<MidiUnitId>,
    /// Whether this route is enabled
    pub enabled: bool,
}

impl MidiRoute {
    pub fn new() -> Self {
        Self {
            port: None,
            channel: None,
            targets: Vec::new(),
            enabled: true,
        }
    }

    pub fn for_channel(channel: u8) -> Self {
        Self {
            port: None,
            channel: Some(channel),
            targets: Vec::new(),
            enabled: true,
        }
    }

    pub fn for_port(port: usize) -> Self {
        Self {
            port: Some(port),
            channel: None,
            targets: Vec::new(),
            enabled: true,
        }
    }

    pub fn for_port_channel(port: usize, channel: u8) -> Self {
        Self {
            port: Some(port),
            channel: Some(channel),
            targets: Vec::new(),
            enabled: true,
        }
    }

    pub fn with_target(mut self, unit_id: MidiUnitId) -> Self {
        if self.targets.len() < MAX_TARGETS_PER_ROUTE {
            self.targets.push(unit_id);
        }
        self
    }

    pub fn with_targets(mut self, unit_ids: &[MidiUnitId]) -> Self {
        for &id in unit_ids {
            if self.targets.len() >= MAX_TARGETS_PER_ROUTE {
                break;
            }
            self.targets.push(id);
        }
        self
    }

    #[inline]
    pub fn matches(&self, port: usize, event: &MidiEvent) -> bool {
        if !self.enabled {
            return false;
        }
        let port_matches = self.port.is_none_or(|p| p == port);
        let channel_matches = match self.channel {
            None => true,
            Some(c) => event_channel(event) == Some(c),
        };
        port_matches && channel_matches
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
    routes: Vec<MidiRoute>,
    /// Precomputed channel→targets lookup (17 entries: 0-15 + "any channel")
    /// Index 16 is for routes that match any channel.
    channel_lookup: [Vec<MidiUnitId>; 17],
    /// Fallback target when no routes match
    fallback_target: Option<MidiUnitId>,
}

impl MidiRoutingSnapshot {
    pub fn empty() -> Self {
        Self {
            routes: Vec::new(),
            channel_lookup: Default::default(),
            fallback_target: None,
        }
    }

    pub fn from_routes(routes: Vec<MidiRoute>, fallback: Option<MidiUnitId>) -> Self {
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

        for route in &self.routes {
            if !route.enabled {
                continue;
            }
            if route.port.is_some() {
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

    /// Returns iterator over targets. Zero allocations, deduplicates.
    #[inline]
    pub fn route<'a>(&'a self, port: usize, event: &'a MidiEvent) -> RouteIterator<'a> {
        RouteIterator {
            snapshot: self,
            port,
            event,
            phase: RoutePhase::ChannelLookup,
            route_idx: 0,
            target_idx: 0,
            seen: [MidiUnitId::new(0); 16],
            seen_count: 0,
        }
    }

    #[inline]
    pub fn route_single(&self, port: usize, event: &MidiEvent) -> Option<MidiUnitId> {
        // Non-channel-voice messages (system, SysEx, utility) skip the
        // channel-indexed fast path.
        if let Some(channel) = event_channel(event) {
            if let Some(&target) = self.channel_lookup[channel as usize].first() {
                return Some(target);
            }
        }

        if let Some(&target) = self.channel_lookup[16].first() {
            return Some(target);
        }

        for route in &self.routes {
            if route.matches(port, event) {
                if let Some(&target) = route.targets.first() {
                    return Some(target);
                }
            }
        }

        self.fallback_target
    }

    pub fn all_targets(&self) -> Vec<MidiUnitId> {
        let mut targets = Vec::new();
        for route in &self.routes {
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

    #[inline]
    pub fn has_routes(&self) -> bool {
        !self.routes.is_empty() || self.fallback_target.is_some()
    }

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
    PortRoutes,
    Fallback,
    Done,
}

/// Zero allocations - uses stack-allocated seen buffer.
pub struct RouteIterator<'a> {
    snapshot: &'a MidiRoutingSnapshot,
    port: usize,
    event: &'a MidiEvent,
    phase: RoutePhase,
    route_idx: usize,
    target_idx: usize,
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
                    // Non-channel-voice messages (system/SysEx/utility) bypass
                    // the channel-indexed fast path and fall through to the
                    // "any channel" bucket.
                    if let Some(channel) = event_channel(self.event) {
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
                    self.phase = RoutePhase::PortRoutes;
                }
                RoutePhase::PortRoutes => {
                    while self.route_idx < self.snapshot.routes.len() {
                        let route = &self.snapshot.routes[self.route_idx];
                        if route.port.is_some() && route.matches(self.port, self.event) {
                            while self.target_idx < route.targets.len() {
                                let target = route.targets[self.target_idx];
                                self.target_idx += 1;
                                if !self.is_seen(target) {
                                    self.mark_seen(target);
                                    return Some(target);
                                }
                            }
                        }
                        self.target_idx = 0;
                        self.route_idx += 1;
                    }
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
/// the audio thread via [`arc_swap::ArcSwap`]. Stage the full rule set with
/// [`set_routes`](MidiRoutingTable::set_routes) from the UI thread, then
/// [`commit`](MidiRoutingTable::commit) to publish. The audio thread reads via
/// the `Arc<ArcSwap<MidiRoutingSnapshot>>` returned by
/// [`snapshot_arc`](MidiRoutingTable::snapshot_arc).
pub struct MidiRoutingTable {
    routes: Vec<MidiRoute>,
    fallback_target: Option<MidiUnitId>,
    snapshot: Arc<ArcSwap<MidiRoutingSnapshot>>,
    dirty: bool,
}

impl MidiRoutingTable {
    pub fn new() -> Self {
        let snapshot = MidiRoutingSnapshot::empty();
        Self {
            routes: Vec::new(),
            fallback_target: None,
            snapshot: Arc::new(ArcSwap::from_pointee(snapshot)),
            dirty: false,
        }
    }

    pub fn snapshot_arc(&self) -> Arc<ArcSwap<MidiRoutingSnapshot>> {
        self.snapshot.clone()
    }

    #[inline]
    pub fn load(&self) -> arc_swap::Guard<Arc<MidiRoutingSnapshot>> {
        self.snapshot.load()
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
        self.routes = routes.into_iter().collect();
        self.fallback_target = fallback;
        self.dirty = true;
    }

    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Creates snapshot and atomically swaps. Audio thread sees new config on next load().
    pub fn commit(&mut self) {
        if !self.dirty {
            return;
        }

        let snapshot = MidiRoutingSnapshot::from_routes(self.routes.clone(), self.fallback_target);
        self.snapshot.store(Arc::new(snapshot));
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
    use alloc::vec;

    const fn id(n: u64) -> MidiUnitId {
        MidiUnitId::new(n)
    }

    fn note_on(channel: u8, note: u8) -> MidiEvent {
        MidiEvent::note_on(0, channel, note, 0x8000)
    }

    #[test]
    fn test_empty_routing() {
        let snapshot = MidiRoutingSnapshot::empty();
        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(0, &event).collect();
        assert!(targets.is_empty());
    }

    #[test]
    fn test_fallback_routing() {
        let snapshot = MidiRoutingSnapshot::from_routes(Vec::new(), Some(id(42)));
        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(0, &event).collect();
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
        let targets: Vec<_> = snapshot.route(0, &event).collect();
        assert_eq!(targets, vec![id(100)]);

        // Channel 1 → unit 200
        let event = note_on(1, 60);
        let targets: Vec<_> = snapshot.route(0, &event).collect();
        assert_eq!(targets, vec![id(200)]);

        // Channel 2 → no targets
        let event = note_on(2, 60);
        let targets: Vec<_> = snapshot.route(0, &event).collect();
        assert!(targets.is_empty());
    }

    #[test]
    fn test_channel_layering() {
        let routes = vec![MidiRoute::for_channel(0).with_targets(&[id(100), id(200), id(300)])];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, None);

        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(0, &event).collect();
        assert_eq!(targets, vec![id(100), id(200), id(300)]);
    }

    #[test]
    fn test_port_routing() {
        let routes = vec![
            MidiRoute::for_port(0).with_target(id(100)),
            MidiRoute::for_port(1).with_target(id(200)),
        ];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, None);

        let event = note_on(0, 60);

        // Port 0 → unit 100
        let targets: Vec<_> = snapshot.route(0, &event).collect();
        assert_eq!(targets, vec![id(100)]);

        // Port 1 → unit 200
        let targets: Vec<_> = snapshot.route(1, &event).collect();
        assert_eq!(targets, vec![id(200)]);
    }

    #[test]
    fn test_port_channel_routing() {
        let routes = vec![
            MidiRoute::for_port_channel(0, 0).with_target(id(100)),
            MidiRoute::for_port_channel(0, 1).with_target(id(200)),
            MidiRoute::for_port_channel(1, 0).with_target(id(300)),
        ];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, None);

        // Port 0, Channel 0 → 100
        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(0, &event).collect();
        assert_eq!(targets, vec![id(100)]);

        // Port 0, Channel 1 → 200
        let event = note_on(1, 60);
        let targets: Vec<_> = snapshot.route(0, &event).collect();
        assert_eq!(targets, vec![id(200)]);

        // Port 1, Channel 0 → 300
        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(1, &event).collect();
        assert_eq!(targets, vec![id(300)]);
    }

    #[test]
    fn test_global_layer() {
        let routes = vec![MidiRoute::new().with_targets(&[id(100), id(200)])];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, None);

        // Any channel, any port → both units
        let event = note_on(5, 60);
        let targets: Vec<_> = snapshot.route(2, &event).collect();
        assert_eq!(targets, vec![id(100), id(200)]);
    }

    #[test]
    fn test_route_single() {
        let routes = vec![MidiRoute::for_channel(0).with_target(id(100))];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, Some(id(999)));

        // Channel 0 → 100 (via route)
        let event = note_on(0, 60);
        assert_eq!(snapshot.route_single(0, &event), Some(id(100)));

        // Channel 5 → 999 (via fallback)
        let event = note_on(5, 60);
        assert_eq!(snapshot.route_single(0, &event), Some(id(999)));
    }

    #[test]
    fn test_no_duplicate_targets() {
        let routes = vec![
            MidiRoute::for_channel(0).with_target(id(100)),
            MidiRoute::new().with_targets(&[id(100), id(200)]),
        ];
        let snapshot = MidiRoutingSnapshot::from_routes(routes, None);

        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(0, &event).collect();

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
        let targets: Vec<_> = snapshot.route(0, &event).collect();
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
        let targets: Vec<_> = snapshot.route(0, &event).collect();
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
        let targets: Vec<_> = snapshot.route(0, &event).collect();
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
}
