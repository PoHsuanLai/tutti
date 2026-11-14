//! What the editor holds for each key besides its spec and shape: the unit
//! waiting for the next commit, its wake, its fork source, the crossfade or
//! latency cut that commit carries, and whether the key is leaving.

use std::collections::BTreeMap;

use crate::NodeKey;
use crate::fade::Fade;
use crate::fork::ForkSource;
use crate::node::Node;
use crate::wake::Wake;

/// One record per key that was ever placed.
///
/// A record outlives its node: the generation a key's next unit gets keeps
/// counting across a removal, so a unit placed after it is never taken for
/// one before. [`forget`](Self::forget) clears everything else at once.
pub(super) struct Placements(BTreeMap<NodeKey, Placement>);

#[derive(Default)]
struct Placement {
    /// The generation the next unit placed here gets.
    next_gen: u32,
    /// The unit placed since the last commit, which the next one sends.
    unit: Option<Box<dyn Node>>,
    /// The [`Wake`] the latest placed unit was prepared with. A commit hands
    /// the executor a clone with the unit, and a re-prepare hands the unit
    /// this same one again.
    wake: Option<Wake>,
    /// Where the key's forks come from, with the generation of the unit
    /// that handed it over. `None`: the key is not forkable.
    fork: Option<(u32, Box<dyn ForkSource>)>,
    /// The crossfade a `replace` asked for, carried by the next commit.
    fade: Option<Fade>,
    /// Whether `set_latency` changed the key since the last commit: a
    /// crossfade running or waiting there is cut by it.
    latency_cut: bool,
    /// Whether `remove_declicked` keeps the key until its output fades.
    leaving: bool,
}

impl Placements {
    pub(super) fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Places `unit` at `key` with its wake and fork source, and returns the
    /// unit's generation. A fade or latency cut waiting at `key` was for the
    /// unit this one supersedes, and goes; a unit with no fork source makes
    /// the key unforkable.
    pub(super) fn place(
        &mut self,
        key: NodeKey,
        unit: Box<dyn Node>,
        wake: Wake,
        fork: Option<Box<dyn ForkSource>>,
    ) -> u32 {
        let p = self.0.entry(key).or_default();
        let generation = p.next_gen;
        p.next_gen += 1;
        p.unit = Some(unit);
        p.wake = Some(wake);
        p.fork = fork.map(|f| (generation, f));
        p.fade = None;
        p.latency_cut = false;
        generation
    }

    /// The crossfade the next commit carries for the unit placed at `key`.
    pub(super) fn set_fade(&mut self, key: NodeKey, fade: Fade) {
        self.0.entry(key).or_default().fade = Some(fade);
    }

    /// Marks `key`'s latency changed: the next commit cuts a crossfade
    /// running or waiting there, and a replace not yet committed lands as a
    /// swap.
    pub(super) fn cut_latency(&mut self, key: NodeKey) {
        let p = self.0.entry(key).or_default();
        p.fade = None;
        p.latency_cut = true;
    }

    /// Forgets everything `key` holds but its generation count.
    pub(super) fn forget(&mut self, key: NodeKey) {
        if let Some(p) = self.0.get_mut(&key) {
            *p = Placement {
                next_gen: p.next_gen,
                leaving: p.leaving,
                ..Placement::default()
            };
        }
    }

    /// Keeps `key` until its output fades.
    pub(super) fn leave(&mut self, key: NodeKey) {
        self.0.entry(key).or_default().leaving = true;
    }

    /// Ends `key`'s leaving.
    pub(super) fn stop_leaving(&mut self, key: NodeKey) {
        if let Some(p) = self.0.get_mut(&key) {
            p.leaving = false;
        }
    }

    /// Whether `key` is leaving.
    pub(super) fn is_leaving(&self, key: NodeKey) -> bool {
        self.0.get(&key).is_some_and(|p| p.leaving)
    }

    /// The keys leaving.
    pub(super) fn leaving(&self) -> Vec<NodeKey> {
        self.0
            .iter()
            .filter(|(_, p)| p.leaving)
            .map(|(&k, _)| k)
            .collect()
    }

    /// The wake of the unit placed at `key` last.
    pub(super) fn wake(&self, key: NodeKey) -> Option<&Wake> {
        self.0.get(&key).and_then(|p| p.wake.as_ref())
    }

    /// Where `key`'s forks come from, and the generation of the unit that
    /// handed it over.
    pub(super) fn fork_source(&self, key: NodeKey) -> Option<(u32, &dyn ForkSource)> {
        self.0
            .get(&key)
            .and_then(|p| p.fork.as_ref())
            .map(|(generation, f)| (*generation, f.as_ref()))
    }

    /// Whether a unit placed at `key` waits for a commit.
    pub(super) fn has_unit(&self, key: NodeKey) -> bool {
        self.0.get(&key).is_some_and(|p| p.unit.is_some())
    }

    /// Takes the unit waiting at `key`.
    pub(super) fn take_unit(&mut self, key: NodeKey) -> Option<Box<dyn Node>> {
        self.0.get_mut(&key).and_then(|p| p.unit.take())
    }

    /// Takes every unit waiting for a commit.
    pub(super) fn take_units(&mut self) -> BTreeMap<NodeKey, Box<dyn Node>> {
        self.0
            .iter_mut()
            .filter_map(|(&k, p)| p.unit.take().map(|u| (k, u)))
            .collect()
    }

    /// The crossfade waiting at `key`.
    pub(super) fn fade(&self, key: NodeKey) -> Option<Fade> {
        self.0.get(&key).and_then(|p| p.fade)
    }

    /// The keys whose latency changed since the last commit.
    pub(super) fn latency_cuts(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.0
            .iter()
            .filter(|(_, p)| p.latency_cut)
            .map(|(&k, _)| k)
    }

    /// Clears every waiting crossfade and latency cut: a commit carried
    /// them, or a re-prepare made them moot.
    pub(super) fn clear_fades_and_cuts(&mut self) {
        for p in self.0.values_mut() {
            p.fade = None;
            p.latency_cut = false;
        }
    }

    /// Whether anything waits for the next commit: a unit, a crossfade or a
    /// latency cut.
    pub(super) fn is_staged(&self) -> bool {
        self.0
            .values()
            .any(|p| p.unit.is_some() || p.fade.is_some() || p.latency_cut)
    }
}

impl std::fmt::Debug for Placements {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(k, p)| {
                (
                    k,
                    (
                        p.next_gen,
                        p.unit.is_some(),
                        p.fade,
                        p.latency_cut,
                        p.leaving,
                    ),
                )
            }))
            .finish()
    }
}
