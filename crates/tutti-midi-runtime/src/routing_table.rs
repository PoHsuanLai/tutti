//! UI-thread writer for MIDI routing configuration.
//!
//! Pure routing types ([`MidiRoute`], [`MidiRoutingSnapshot`], and the
//! `route()` iterator) live in the [`tutti_midi_types`] crate. This module provides
//! [`MidiRoutingTable`], the mutable writer that publishes snapshots
//! atomically to the audio thread via [`arc_swap::ArcSwap`].
//!
//! Call configuration methods ([`channel`](MidiRoutingTable::channel),
//! [`port`](MidiRoutingTable::port), etc.) from the UI thread, then
//! [`commit`](MidiRoutingTable::commit) to publish. The audio thread reads
//! via the `Arc<ArcSwap<MidiRoutingSnapshot>>` returned by
//! [`snapshot_arc`](MidiRoutingTable::snapshot_arc).

use arc_swap::ArcSwap;
use std::sync::Arc;
use tutti_midi_types::{MidiRoute, MidiRoutingSnapshot, MidiUnitId};

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

    /// Set fallback target (for unmapped events).
    pub fn fallback(&mut self, target: MidiUnitId) -> &mut Self {
        self.fallback_target = Some(target);
        self.dirty = true;
        self
    }

    pub fn clear_fallback(&mut self) -> &mut Self {
        self.fallback_target = None;
        self.dirty = true;
        self
    }

    /// Route channel to target. Multiple calls add multiple targets per channel.
    pub fn channel(&mut self, channel: u8, unit_id: MidiUnitId) -> &mut Self {
        for route in &mut self.routes {
            if route.port.is_none() && route.channel == Some(channel) {
                if !route.targets.contains(&unit_id) {
                    route.targets.push(unit_id);
                }
                self.dirty = true;
                return self;
            }
        }

        self.routes
            .push(MidiRoute::for_channel(channel).with_target(unit_id));
        self.dirty = true;
        self
    }

    pub fn port(&mut self, port: usize, unit_id: MidiUnitId) -> &mut Self {
        for route in &mut self.routes {
            if route.port == Some(port) && route.channel.is_none() {
                if !route.targets.contains(&unit_id) {
                    route.targets.push(unit_id);
                }
                self.dirty = true;
                return self;
            }
        }

        self.routes
            .push(MidiRoute::for_port(port).with_target(unit_id));
        self.dirty = true;
        self
    }

    pub fn port_channel(&mut self, port: usize, channel: u8, unit_id: MidiUnitId) -> &mut Self {
        for route in &mut self.routes {
            if route.port == Some(port) && route.channel == Some(channel) {
                if !route.targets.contains(&unit_id) {
                    route.targets.push(unit_id);
                }
                self.dirty = true;
                return self;
            }
        }

        self.routes
            .push(MidiRoute::for_port_channel(port, channel).with_target(unit_id));
        self.dirty = true;
        self
    }

    /// Route all MIDI to multiple targets. Replaces existing global layer.
    pub fn layer(&mut self, targets: &[MidiUnitId]) -> &mut Self {
        self.routes
            .retain(|r| r.port.is_some() || r.channel.is_some());

        if !targets.is_empty() {
            self.routes.push(MidiRoute::new().with_targets(targets));
        }
        self.dirty = true;
        self
    }

    /// Route channel to multiple targets. Replaces existing routes for this channel.
    pub fn channel_layer(&mut self, channel: u8, targets: &[MidiUnitId]) -> &mut Self {
        self.routes
            .retain(|r| !(r.port.is_none() && r.channel == Some(channel)));

        if !targets.is_empty() {
            self.routes
                .push(MidiRoute::for_channel(channel).with_targets(targets));
        }
        self.dirty = true;
        self
    }

    pub fn remove_unit(&mut self, unit_id: MidiUnitId) -> &mut Self {
        for route in &mut self.routes {
            route.targets.retain(|&id| id != unit_id);
        }
        self.routes.retain(|r| !r.targets.is_empty());

        if self.fallback_target == Some(unit_id) {
            self.fallback_target = None;
        }
        self.dirty = true;
        self
    }

    pub fn clear(&mut self) -> &mut Self {
        self.routes.clear();
        self.fallback_target = None;
        self.dirty = true;
        self
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

    const fn id(n: u64) -> MidiUnitId {
        MidiUnitId::new(n)
    }

    fn note_on(channel: u8, note: u8) -> tutti_midi_types::ump::MidiEvent {
        tutti_midi_types::ump::MidiEvent::note_on(
            0,
            channel,
            note,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        )
    }

    #[test]
    fn test_fallback_through_table() {
        let mut table = MidiRoutingTable::new();
        table.fallback(id(42));
        table.commit();

        let snapshot = table.load();
        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(0, &event).collect();
        assert_eq!(targets, vec![id(42)]);
    }

    #[test]
    fn test_channel_through_table() {
        let mut table = MidiRoutingTable::new();
        table.channel(0, id(100)).channel(1, id(200));
        table.commit();

        let snapshot = table.load();
        let event = note_on(0, 60);
        let targets: Vec<_> = snapshot.route(0, &event).collect();
        assert_eq!(targets, vec![id(100)]);
    }

    #[test]
    fn test_remove_unit() {
        let mut table = MidiRoutingTable::new();
        table
            .channel(0, id(100))
            .channel(0, id(200))
            .fallback(id(100));
        table.commit();

        table.remove_unit(id(100));
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

        table.channel(0, id(100));
        assert!(table.is_dirty());

        table.commit();
        assert!(!table.is_dirty());
    }
}
