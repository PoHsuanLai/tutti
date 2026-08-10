//! The modulation **rules** role — the mod-matrix: [`ModEdge`],
//! [`ModRoutingSnapshot`], and the off-thread [`ModRoutingTable`] writer.
//!
//! An immutable, precomputed routing table published via `Arc<RtPublish<..>>` and
//! read lock-free each frame; an off-thread writer stages a wholesale edge
//! replacement and `commit()`s a fresh snapshot atomically.
//!
//! Modulation routes **per edge** — a mod matrix of `(source, target, depth, …)`
//! triples — so the snapshot is a `Vec<ModEdge>` with a precomputed
//! `by_source` lookup.

use std::sync::Arc;

use tutti_types::{Depth, RtPublish};

use crate::id::{LayerKey, ModTargetId};
use crate::shape::Polarity;
use crate::CurveType;

/// One mod-matrix edge: a source drives a target at a depth, under a key.
///
/// `source` is an **index** into the driver's source registry (see
/// [`crate::ModPreFrame`]) — the snapshot does not own the stateful
/// `Modulator`s (their `State` must thread across frames; the snapshot is
/// immutable and shared, so it cannot hold per-frame state). `min`/`max` are
/// baked in so the driver scales the raw `[-1, 1]` value into target units
/// without locking the target to read its range.
#[derive(Clone, Debug, PartialEq)]
pub struct ModEdge {
    /// Index into the driver's source registry.
    pub source: usize,
    /// Router address of the target accumulator.
    pub target: ModTargetId,
    /// Contributor key — must be non-zero (never [`LayerKey::AUTOMATION`]).
    pub key: LayerKey,
    /// Bipolar routing depth. Negative inverts the source.
    pub depth: Depth,
    /// Lower bound of the target range, baked in so scaling needs no lock.
    ///
    /// Bare floats because `min`/`max` are in the *target's* units — `Hz` for a
    /// cutoff, linear gain for a fader, `Semitones` for a pitch — so no one
    /// newtype is right for the field. The unit lives with the target that
    /// declared the range, and the driver only ever uses the span to scale by.
    pub min: f32,
    /// Upper bound of the target range. See [`min`](Self::min).
    pub max: f32,
    /// How the raw value is shaped (see [`crate::shape`]).
    pub polarity: Polarity,
    /// Response curve applied to the shaped magnitude. Only the parametric
    /// [`CurveType`] variants bend the offset; the rest fall back to linear.
    pub curve: CurveType,
    /// A disabled edge contributes nothing and is skipped at snapshot build.
    pub enabled: bool,
}

impl ModEdge {
    /// A linear, bipolar, full-clamp edge — the common case.
    pub fn linear(
        source: usize,
        target: ModTargetId,
        key: LayerKey,
        depth: impl Into<Depth>,
        min: f32,
        max: f32,
    ) -> Self {
        Self {
            source,
            target,
            key,
            depth: depth.into(),
            min,
            max,
            polarity: Polarity::Bipolar,
            curve: CurveType::Linear,
            enabled: true,
        }
    }
}

/// Immutable, precomputed mod-matrix. Built once, read-only, shared via
/// `Arc<RtPublish<..>>`.
#[derive(Clone, Debug, Default)]
pub struct ModRoutingSnapshot {
    edges: Vec<ModEdge>,
    /// `by_source[i]` = indices into `edges` fed by source `i`.
    by_source: Vec<Vec<usize>>,
    source_count: usize,
}

impl ModRoutingSnapshot {
    /// An empty snapshot (no edges).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build from an edge list. Disabled edges and out-of-range source indices
    /// are dropped from the lookup (they simply never fire).
    pub fn from_edges(edges: Vec<ModEdge>, source_count: usize) -> Self {
        let mut by_source = vec![Vec::new(); source_count];
        for (i, e) in edges.iter().enumerate() {
            if e.enabled && e.source < source_count {
                by_source[e.source].push(i);
            }
        }
        Self {
            edges,
            by_source,
            source_count,
        }
    }

    /// The edges fed by a given source. Zero-alloc iterator.
    #[inline]
    pub fn edges_for_source(&self, source: usize) -> impl Iterator<Item = &ModEdge> {
        self.by_source
            .get(source)
            .into_iter()
            .flatten()
            .map(move |&i| &self.edges[i])
    }

    /// Number of source slots this snapshot expects.
    #[inline]
    pub fn source_count(&self) -> usize {
        self.source_count
    }

    /// Whether any source has a live edge. False for an empty snapshot and also
    /// for one whose every edge was disabled or pointed at an out-of-range
    /// source — those are dropped at build, so the edge list may be non-empty
    /// while this is false.
    #[inline]
    pub fn has_edges(&self) -> bool {
        self.by_source.iter().any(|v| !v.is_empty())
    }

    /// Every active `(target, key)` — used by the driver to clear layers that
    /// disappear across a hot-swap (the "continuous-value tax").
    pub fn active_layers(&self) -> impl Iterator<Item = (ModTargetId, LayerKey)> + '_ {
        self.by_source
            .iter()
            .flatten()
            .map(move |&i| (self.edges[i].target, self.edges[i].key))
    }
}

/// Off-thread writer for the mod-matrix: stage a wholesale edge replacement,
/// then `commit()` swaps a fresh immutable snapshot in atomically.
pub struct ModRoutingTable {
    edges: Vec<ModEdge>,
    source_count: usize,
    snapshot: Arc<RtPublish<ModRoutingSnapshot>>,
    dirty: bool,
}

impl Default for ModRoutingTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ModRoutingTable {
    /// A writer over a freshly published empty snapshot. Stage edges with
    /// [`set_edges`](Self::set_edges), then [`commit`](Self::commit).
    pub fn new() -> Self {
        Self {
            edges: Vec::new(),
            source_count: 0,
            snapshot: Arc::new(RtPublish::new(ModRoutingSnapshot::empty())),
            dirty: false,
        }
    }

    /// The read handle for the driver (the audio-adjacent side loads this).
    pub fn snapshot_arc(&self) -> Arc<RtPublish<ModRoutingSnapshot>> {
        Arc::clone(&self.snapshot)
    }

    /// Load the current snapshot directly (for tests / a co-located reader).
    pub fn load(&self) -> tutti_types::RtRef<'_, ModRoutingSnapshot> {
        self.snapshot.read()
    }

    /// Stage a wholesale replacement of the edge set (no incremental edit).
    /// Takes effect on [`commit`].
    ///
    /// [`commit`]: Self::commit
    pub fn set_edges(&mut self, edges: impl IntoIterator<Item = ModEdge>, source_count: usize) {
        self.edges = edges.into_iter().collect();
        self.source_count = source_count;
        self.dirty = true;
    }

    /// Build a fresh snapshot from the staged edges and publish it atomically.
    /// No-op if nothing was staged since the last commit.
    pub fn commit(&mut self) {
        if !self.dirty {
            return;
        }
        self.snapshot
            .publish(Arc::new(ModRoutingSnapshot::from_edges(
                self.edges.clone(),
                self.source_count,
            )));
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_source_lookup_groups_edges() {
        let (t0, t1) = (ModTargetId::new(10), ModTargetId::new(11));
        let edges = vec![
            ModEdge::linear(0, t0, LayerKey(1), 1.0, 0.0, 1.0),
            ModEdge::linear(0, t1, LayerKey(1), 0.5, 0.0, 1.0),
            ModEdge::linear(1, t1, LayerKey(2), 0.5, 0.0, 1.0),
        ];
        let snap = ModRoutingSnapshot::from_edges(edges, 2);
        assert_eq!(snap.edges_for_source(0).count(), 2);
        assert_eq!(snap.edges_for_source(1).count(), 1);
        assert_eq!(snap.edges_for_source(5).count(), 0); // out of range = none
        assert!(snap.has_edges());
    }

    #[test]
    fn disabled_and_oob_edges_are_dropped_from_lookup() {
        let t0 = ModTargetId::new(10);
        let mut disabled = ModEdge::linear(0, t0, LayerKey(1), 1.0, 0.0, 1.0);
        disabled.enabled = false;
        let oob = ModEdge::linear(9, t0, LayerKey(1), 1.0, 0.0, 1.0); // source 9 >= count 2
        let snap = ModRoutingSnapshot::from_edges(vec![disabled, oob], 2);
        assert!(!snap.has_edges());
    }

    #[test]
    fn table_commit_publishes_atomically() {
        let (t0, t1) = (ModTargetId::new(10), ModTargetId::new(11));
        let mut table = ModRoutingTable::new();
        assert!(!table.load().has_edges());

        table.set_edges([ModEdge::linear(0, t0, LayerKey(1), 1.0, 0.0, 1.0)], 1);
        // Not visible until commit.
        assert!(!table.load().has_edges());
        table.commit();
        assert_eq!(table.load().edges_for_source(0).count(), 1);

        // Hot-swap: replace with a different edge set.
        table.set_edges([ModEdge::linear(0, t1, LayerKey(1), 0.5, 0.0, 1.0)], 1);
        table.commit();
        let snap = table.load();
        assert_eq!(snap.edges_for_source(0).next().unwrap().target, t1);
    }

    #[test]
    fn active_layers_enumerates_target_key_pairs() {
        let (t0, t1) = (ModTargetId::new(10), ModTargetId::new(11));
        let snap = ModRoutingSnapshot::from_edges(
            vec![
                ModEdge::linear(0, t0, LayerKey(1), 1.0, 0.0, 1.0),
                ModEdge::linear(1, t1, LayerKey(2), 1.0, 0.0, 1.0),
            ],
            2,
        );
        let mut pairs: Vec<_> = snap.active_layers().collect();
        pairs.sort_by_key(|(t, k)| (t.as_u64(), k.0));
        assert_eq!(pairs, vec![(t0, LayerKey(1)), (t1, LayerKey(2))]);
    }
}
