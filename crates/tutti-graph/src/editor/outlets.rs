//! The editor's outlets of one kind: taps, event taps or meters.

use std::collections::BTreeMap;
use std::sync::Weak;

/// The outlets of one kind: the writers opened since the last commit, which
/// the next one sends, and, for each open id, what its reader was opened with
/// (`M`) and a handle that sees the reader dropped.
///
/// An id is open here exactly while it has an entry, and a writer waits only
/// for an id that is open: [`open`](Self::open) and [`close`](Self::close)
/// change both together.
pub(super) struct Outlets<Id, W, M> {
    pending: Vec<(Id, W)>,
    open: BTreeMap<Id, (M, Weak<()>)>,
}

/// What [`Outlets::close`] took out.
pub(super) struct Closed<M> {
    /// What the reader was opened with.
    pub(super) meta: M,
    /// Whether its writer was still waiting for a commit: the executor never
    /// held it.
    pub(super) unsent: bool,
}

impl<Id: Ord + Copy, W, M> Outlets<Id, W, M> {
    pub(super) fn new() -> Self {
        Self {
            pending: Vec::new(),
            open: BTreeMap::new(),
        }
    }

    /// Opens `id` with its writer, what its reader was opened with and the
    /// reader's watch. An `id` already open is replaced, its unsent writer
    /// with it.
    pub(super) fn open(&mut self, id: Id, writer: W, meta: M, watch: Weak<()>) {
        self.pending.retain(|(k, _)| *k != id);
        self.pending.push((id, writer));
        self.open.insert(id, (meta, watch));
    }

    /// Closes `id`, and returns what it was opened with, or `None` when it
    /// was not open.
    pub(super) fn close(&mut self, id: Id) -> Option<Closed<M>> {
        let unsent = self.pending.iter().any(|(k, _)| *k == id);
        self.pending.retain(|(k, _)| *k != id);
        self.open
            .remove(&id)
            .map(|(meta, _)| Closed { meta, unsent })
    }

    /// What the reader of `id` was opened with.
    pub(super) fn meta(&self, id: Id) -> Option<&M> {
        self.open.get(&id).map(|(meta, _)| meta)
    }

    /// The open ids whose reader was dropped.
    pub(super) fn dropped(&self) -> Vec<Id> {
        self.open
            .iter()
            .filter(|(_, (_, alive))| alive.strong_count() == 0)
            .map(|(&id, _)| id)
            .collect()
    }

    /// Whether a writer waits for the next commit.
    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The writers waiting for the next commit, which then holds them.
    pub(super) fn take_pending(&mut self) -> Vec<(Id, W)> {
        std::mem::take(&mut self.pending)
    }
}
