//! What crosses the rings: a commit, a crossfade's outgoing unit, and the
//! rings themselves.

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use ringbuf::traits::{Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_rt::RtScope;

use super::state::{State, Unit};
use super::{EDGE_FADE_CAPACITY, FADE_CAPACITY, QUEUE_CAPACITY};
use crate::NodeKey;
use crate::arena::Arena;
use crate::event::Event;
use crate::event_tap::EventTapWriter;
use crate::fade::CrossfadeCurve;
use crate::gain::GainCell;
use crate::held::Tables;
use crate::io::Io;
use crate::loss::EventLoss;
use crate::meter::MeterWriter;
use crate::mode::{BypassLine, ModeState};
use crate::node::{Cx, Node, Prepare, Status};
use crate::param::ParamState;
use crate::plan::{Delta, Plan, UnitIdx};
use crate::play::PlayReader;
use crate::tap::TapWriter;
use crate::wake::Wake;
use crate::{EdgeFade, EdgeId, EventTapId, MeterId, TapId};

/// A unit, owned. Crate-private, so the only code that can replace or drop
/// the box is this crate's; its drop is checked against the audio-thread
/// marker (`RtScope::check_outside`). This crate must *run* its
/// units, so the box hands `&mut` out.
///
/// It carries the unit's [`Wake`]: the executor's clone of the flag lives and
/// dies with the unit, so its drop is the control side's too, and the audio
/// thread is never the last owner of the flag's storage.
pub(crate) struct NodeBox {
    node: Box<dyn Node>,
    wake: Wake,
}

impl NodeBox {
    pub(crate) fn new(node: Box<dyn Node>, wake: Wake) -> Self {
        Self { node, wake }
    }

    /// Whether the unit's [`Wake`] was raised since the last call, clearing
    /// it.
    #[inline]
    pub(crate) fn take_wake(&self) -> bool {
        self.wake.take()
    }

    /// The unit, out of its box — control side, to re-prepare it. Leaves a
    /// zero-sized placeholder behind for the box's own drop, which therefore
    /// frees nothing (a `Box` of a zero-sized type never allocated).
    pub(crate) fn into_inner(mut self) -> Box<dyn Node> {
        std::mem::replace(&mut self.node, Box::new(Vacant))
    }
}

/// What a [`NodeBox`] holds once its unit has been taken out.
pub(super) struct Vacant;

impl Node for Vacant {
    fn shape(&self) -> crate::node::Shape {
        crate::node::Shape::EMPTY
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

impl Deref for NodeBox {
    type Target = dyn Node;
    fn deref(&self) -> &(dyn Node + 'static) {
        &*self.node
    }
}

impl DerefMut for NodeBox {
    fn deref_mut(&mut self) -> &mut (dyn Node + 'static) {
        &mut *self.node
    }
}

impl Drop for NodeBox {
    fn drop(&mut self) {
        RtScope::check_outside("a unit");
    }
}

/// One crossfade's own state (see the `fade` module, `src/fade.rs`). Built on
/// the control side with the commit that asks for it, so its scratch is
/// allocated there; it travels back in a commit box, so it is freed there
/// too.
pub(crate) struct Crossfade {
    key: NodeKey,
    /// The unit fading out, once the fade has started.
    pub(super) out: Option<NodeBox>,
    /// The unit fading in, while this fade waits behind the one running at
    /// its key. It moves into the store when this fade starts.
    pub(super) waiting: Option<NodeBox>,
    /// The outgoing unit's outputs, one aligned slot per channel.
    pub(super) scratch: Arena,
    /// Frames of the fade rendered so far.
    pub(super) done: usize,
    pub(super) len: usize,
    pub(super) curve: CrossfadeCurve,
    /// While this fade waits behind the one running at its key: what the unit
    /// fading in wrote for the unit that waits (`Node::chase`), delivered to
    /// it when this fade starts. Empty otherwise.
    pub(super) chase: Vec<Event>,
}

impl Crossfade {
    /// The units it carries back: the outgoing one, a waiting one that
    /// never ran, or both after a re-prepare's cut.
    pub(crate) fn units(&self) -> usize {
        usize::from(self.out.is_some()) + usize::from(self.waiting.is_some())
    }

    /// Its key.
    pub(crate) fn key(&self) -> NodeKey {
        self.key
    }
}

impl Drop for Crossfade {
    fn drop(&mut self) {
        RtScope::check_outside("a crossfade");
    }
}

/// A unit a commit places, with everything built for it on the control
/// side.
pub(super) struct Incoming {
    pub(super) idx: UnitIdx,
    pub(super) generation: u32,
    pub(super) node: NodeBox,
    /// The crossfade it arrives with, over the unit at its key.
    pub(super) fade: Option<Box<Crossfade>>,
    pub(super) params: ParamState,
    pub(super) mode: ModeState,
}

/// A kept unit's rebuilt bypass line.
pub(super) struct Relined {
    pub(super) idx: UnitIdx,
    pub(super) latency: usize,
    pub(super) line: Option<BypassLine>,
}

/// A plan and the unit changes that go with it — what travels to the
/// executor and back. Crate-private: nobody outside the pair ever holds one.
///
/// Before it is applied it holds the new plan and the incoming units; after,
/// everything the executor replaced. Dropping it on the audio thread panics
/// in a debug build.
pub(crate) struct Commit {
    /// The editor's count of commits sent, this one included: a scheduled
    /// command checked against this commit waits until it is applied.
    pub(super) seq: u64,
    /// A re-prepare's first half: check every unit out and adopt this
    /// `Prepare` (see `Editor::reprepare`). Carries no plan.
    pub(super) suspend: Option<Prepare>,
    pub(super) plan: Option<Arc<Plan>>,
    pub(super) delta: Delta,
    pub(super) incoming: Vec<Incoming>,
    /// Kept units whose declared latency changed, each with its rebuilt
    /// bypass line (`Editor::set_latency`).
    pub(super) relined: Vec<Relined>,
    pub(super) retired: Vec<(NodeKey, NodeBox)>,
    /// Param state the executor let go of, carried back to be freed on the
    /// control side: a retired unit's, and one built for an incoming unit
    /// that crossfades in over one already at its key, which keeps its own
    /// (the port's modulation runs on across a replace).
    pub(super) spare_params: Vec<ParamState>,
    /// Mode state the executor let go of (a retired unit's, an unused
    /// incoming one's), freed on the control side.
    pub(super) spare_modes: Vec<ModeState>,
    /// Bypass lines a rebuilt one replaced.
    pub(super) spare_lines: Vec<BypassLine>,
    /// Held-key tables the executor let go of, freed on the control side.
    pub(super) spare_held: Vec<Tables>,
    pub(super) old_state: Option<State>,
    /// Tap producers and meter cells the editor opened since the last commit.
    pub(super) opened: Opened,
}

/// What the editor hands the executor with a commit: what it opened since its
/// last one (a tap's producers and a meter's cells), and the cell of every
/// weighted lane the plan lists (`Plan::sum_lanes`).
#[derive(Debug, Default)]
pub(crate) struct Opened {
    /// The live base of each weighted lane of the plan, by edge. The editor
    /// owns the cells and keeps one per edge across commits.
    pub(crate) gains: BTreeMap<EdgeId, GainCell>,
    pub(crate) taps: Vec<(TapId, TapWriter)>,
    pub(crate) event_taps: Vec<(EventTapId, EventTapWriter)>,
    pub(crate) meters: Vec<(MeterId, MeterWriter)>,
}

impl Drop for Commit {
    fn drop(&mut self) {
        RtScope::check_outside("Commit");
    }
}

impl Commit {
    /// `params` builds each incoming unit's param state from its key and
    /// declared port count (the editor's range cells).
    pub(crate) fn build(
        seq: u64,
        plan: Arc<Plan>,
        delta: Delta,
        mut units: BTreeMap<NodeKey, Box<dyn Node>>,
        mut params: impl FnMut(NodeKey, usize, usize) -> ParamState,
        mut modes: impl FnMut(NodeKey) -> ModeState,
        mut wakes: impl FnMut(NodeKey) -> Wake,
        opened: Opened,
    ) -> Box<Commit> {
        let wanted = delta
            .insert
            .iter()
            .copied()
            .chain(delta.replace.iter().map(|&(_, new)| new));
        let max_block = plan.prepare.max_block().get();
        let incoming = wanted
            .map(|p| {
                let unit = units
                    .remove(&p.key)
                    .unwrap_or_else(|| panic!("no unit supplied for node {}", p.key.0));
                // A fade's scratch holds the outgoing unit's outputs, which
                // have the incoming unit's width (`verify_fades`).
                let fade = delta
                    .fades
                    .iter()
                    .find(|(k, f)| *k == p.key && !f.is_cut())
                    .map(|&(key, f)| {
                        let width = plan.unit(key).map_or(0, |u| u.shape.audio_out().count());
                        Box::new(Crossfade {
                            key,
                            out: None,
                            waiting: None,
                            scratch: Arena::new(width as usize, max_block),
                            done: 0,
                            len: f.duration.get(),
                            curve: f.curve,
                            chase: Vec::new(),
                        })
                    });
                // The unit's param state, sized here so the audio thread
                // never allocates it (see the `param` module).
                let params = params(
                    p.key,
                    plan.unit(p.key).map_or(0, |u| u.shape.params.len()),
                    max_block,
                );
                Incoming {
                    idx: p.idx,
                    generation: p.generation,
                    node: NodeBox::new(unit, wakes(p.key)),
                    fade,
                    params,
                    mode: modes(p.key),
                }
            })
            .collect::<Vec<_>>();
        // A kept unit whose declared latency changed gets the line its new
        // latency needs, built here (`Editor::set_latency` cuts it).
        let relined = delta
            .cuts
            .iter()
            .filter_map(|p| {
                let shape = &plan.unit(p.key)?.shape;
                Some(Relined {
                    idx: p.idx,
                    latency: shape.latency.samples().get(),
                    line: BypassLine::for_shape(shape),
                })
            })
            .collect::<Vec<_>>();
        assert!(
            units.is_empty(),
            "units supplied for nodes the delta does not place: {:?}",
            units.keys().collect::<Vec<_>>()
        );
        // Reserved here so applying moves retirees in without growing.
        let retired = Vec::with_capacity(delta.retire.len() + delta.replace.len());
        // Every incoming unit's may come back unused, and every unit leaving
        // brings its own.
        let spare_params =
            Vec::with_capacity(incoming.len() + delta.retire.len() + delta.replace.len());
        let delta_units = delta.retire.len() + delta.replace.len();
        let spare_modes = Vec::with_capacity(incoming.len() + delta_units);
        let spare_lines = Vec::with_capacity(relined.len());
        Box::new(Self {
            seq,
            suspend: None,
            plan: Some(plan),
            delta,
            incoming,
            relined,
            retired,
            spare_params,
            spare_modes,
            spare_lines,
            spare_held: Vec::with_capacity(delta_units),
            old_state: None,
            opened,
        })
    }

    /// A re-prepare's first half: when applied, every unit leaves the store
    /// and comes back in this box, and the executor adopts `prepare`.
    /// `units` is how many the running plan has, reserved here so applying
    /// moves them in without growing.
    pub(crate) fn suspend(seq: u64, prepare: Prepare, units: usize) -> Box<Commit> {
        Box::new(Self {
            seq,
            suspend: Some(prepare),
            plan: None,
            delta: Delta::default(),
            incoming: Vec::new(),
            relined: Vec::new(),
            retired: Vec::with_capacity(units),
            spare_params: Vec::with_capacity(units),
            spare_modes: Vec::with_capacity(units),
            spare_lines: Vec::new(),
            spare_held: Vec::with_capacity(units),
            old_state: None,
            opened: Opened::default(),
        })
    }

    /// Whether this commit crossfades `key` in.
    pub(super) fn fades_in(&self, key: NodeKey) -> bool {
        self.incoming
            .iter()
            .any(|i| i.fade.as_ref().is_some_and(|x| x.key == key))
    }

    /// Whether this box is a re-prepare's first half.
    pub(crate) fn is_suspend(&self) -> bool {
        self.suspend.is_some()
    }

    /// The units applying removed, by key.
    pub(crate) fn retired(&self) -> impl Iterator<Item = NodeKey> + '_ {
        self.retired.iter().map(|(k, _)| *k)
    }

    /// The units applying removed, out of their boxes (control side).
    pub(crate) fn take_retired(&mut self) -> Vec<(NodeKey, Box<dyn Node>)> {
        self.retired
            .drain(..)
            .map(|(k, b)| (k, b.into_inner()))
            .collect()
    }
}

/// Send an applied box back to the editor. The push cannot fail by the
/// credit count (module docs); were it ever full, leak rather than free on
/// the audio thread.
pub(super) fn send_back(back: &mut HeapProd<Box<Commit>>, commit: Box<Commit>) {
    if let Err(full) = back.try_push(commit) {
        debug_assert!(false, "the return ring is full");
        std::mem::forget(full);
    }
}

/// Send a crossfade that ended, or was cut, superseded or never started,
/// back to the editor on the fade-return ring, with the units it carries.
/// The push cannot fail: the editor reserved this crossfade's slot before
/// sending the commit that built it (module docs). Were it ever full, leak
/// rather than free on the audio thread.
pub(super) fn retire_fade(fade_back: &mut HeapProd<Box<Crossfade>>, x: Box<Crossfade>) {
    if let Err(full) = fade_back.try_push(x) {
        debug_assert!(false, "the fade-return ring is full");
        std::mem::forget(full);
    }
}

/// Cut `u`'s crossfades: the newest unit at the key (a waiting one, if
/// any — the one the plan's generation names) becomes `u`'s unit, and the
/// others go back on the fade-return ring with their crossfades.
///
/// Returns the keys of the unit that stopped writing, if the cut displaced
/// one (`Keys::displace`).
pub(super) fn cut_fades(u: &mut Unit, fade_back: &mut HeapProd<Box<Crossfade>>) -> Option<Tables> {
    let mut stale = None;
    if let Some(mut q) = u.queued.take() {
        let newest = q.waiting.take().expect("a queued fade holds its unit");
        q.out = Some(std::mem::replace(&mut u.node, newest));
        retire_fade(fade_back, q);
        stale = Some(u.keys.displace());
    }
    if let Some(f) = u.fade.take() {
        retire_fade(fade_back, f);
        // A cut is a step: never skip the next call on stale flags.
        u.quiet = 0;
        u.last_quiet = false;
        u.last_idle = false;
        u.reported = None;
    }
    stale
}

/// The queue pair between an editor and its executor.
pub(crate) struct Channels {
    pub(crate) to_executor: HeapProd<Box<Commit>>,
    pub(crate) returned: HeapCons<Box<Commit>>,
    /// Crossfades coming back (see the module docs, "Crossfades").
    pub(crate) faded: HeapCons<Box<Crossfade>>,
    /// The executor's play clock, as it last published it.
    pub(crate) play: PlayReader,
    /// Where the executor says which ports lost events.
    pub(crate) losses: HeapCons<EventLoss>,
    /// Refusals the log and the ring could not name.
    pub(crate) unreported: Arc<AtomicU64>,
    /// The edges whose declick fade ended (`EdgeFade`), by the kind that did.
    pub(crate) edge_fades: HeapCons<(EdgeId, EdgeFade)>,
}

/// The executor's ends of the rings.
pub(crate) struct ExecutorEnds {
    pub(super) queue: HeapCons<Box<Commit>>,
    pub(super) back: HeapProd<Box<Commit>>,
    pub(super) fade_back: HeapProd<Box<Crossfade>>,
    pub(super) play: Arc<AtomicU64>,
    pub(super) losses: HeapProd<EventLoss>,
    pub(super) unreported: Arc<AtomicU64>,
    pub(super) edge_fades: HeapProd<(EdgeId, EdgeFade)>,
}

/// Build the rings: the editor's ends and the executor's.
pub(crate) fn channels() -> (Channels, ExecutorEnds) {
    let (tx, rx) = HeapRb::<Box<Commit>>::new(QUEUE_CAPACITY).split();
    let (back_tx, back_rx) = HeapRb::<Box<Commit>>::new(QUEUE_CAPACITY + 1).split();
    let (fade_tx, fade_rx) = HeapRb::<Box<Crossfade>>::new(FADE_CAPACITY).split();
    let play = Arc::new(AtomicU64::new(0));
    let unreported = Arc::new(AtomicU64::new(0));
    let (loss_tx, loss_rx) = HeapRb::<EventLoss>::new(crate::loss::RING_CAPACITY).split();
    let (fade_end_tx, fade_end_rx) = HeapRb::<(EdgeId, EdgeFade)>::new(EDGE_FADE_CAPACITY).split();
    (
        Channels {
            to_executor: tx,
            returned: back_rx,
            faded: fade_rx,
            play: PlayReader(Arc::clone(&play)),
            losses: loss_rx,
            unreported: Arc::clone(&unreported),
            edge_fades: fade_end_rx,
        },
        ExecutorEnds {
            queue: rx,
            back: back_tx,
            fade_back: fade_tx,
            play,
            losses: loss_tx,
            unreported,
            edge_fades: fade_end_tx,
        },
    )
}
