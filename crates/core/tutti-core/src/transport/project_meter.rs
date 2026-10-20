//! The project meter, and the cells that follow it.
//!
//! The metronome and every hosted plugin read the same musical meter, once per
//! block. Each reader reads **one** cell whose identity is fixed when the node
//! is built: the metronome the project's own ([`ProjectMeter`]), a plugin node
//! its own [`MeterSlot`]. A slot follows at most one project meter, and a
//! [`ProjectMeter::set_meter`] publishes into the project's cell and into
//! every following slot before it returns.
//!
//! # Why a slot follows rather than holding the project's cell
//!
//! A plugin is given the project meter after it is built, and can be given
//! another or none later. When the node read the project's cell through a
//! slot holding it (a cell of cells), installing a meter swapped which cell
//! the node read; a reader registered on a cell (the coming `arc-lend`
//! `Reader`) cannot be built on the audio thread for a cell it has never
//! seen. So the node reads its own cell, built with it, and installing or
//! clearing a meter is a publish into that cell, never a swap of it.
//!
//! # Agreement
//!
//! `set_meter` publishes the project's cell and then each follower's, under
//! one lock, before it returns. A block that starts after it returned reads
//! the new meter in the metronome and in every plugin; a block that starts
//! while it runs can read the old meter in some and the new in others, which
//! a single shared cell allowed too (a publish landing between two nodes'
//! reads of one block).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use tutti_types::meter::MeterMap;
use tutti_types::ArcLend;

/// What a following node reads: the project's meter, or none (the node then
/// reads 4/4 from bar 0).
pub type MeterSlotCell = ArcLend<Option<Arc<MeterMap>>>;

/// The project meter: the metronome's cell, and the slots that follow it.
///
/// A handle; a clone is the same meter. [`set_meter`](Self::set_meter) is a
/// control-thread call that reaches the metronome and every following slot
/// (see the module docs).
#[derive(Clone)]
pub struct ProjectMeter(Arc<ProjectInner>);

struct ProjectInner {
    /// What the metronome reads once per block. Its identity is fixed for the
    /// meter's life, so a node reading it builds its reader once.
    cell: Arc<ArcLend<MeterMap>>,
    /// The control side's view: the meter last published, and the slots it
    /// is published to. The audio thread never takes this lock.
    state: Mutex<ProjectState>,
}

struct ProjectState {
    current: Arc<MeterMap>,
    /// Weak, so a slot whose node and handles are gone is dropped from the
    /// list at the next publish rather than kept alive by it.
    followers: Vec<Weak<MeterSlotCell>>,
}

impl ProjectMeter {
    /// A project meter holding `meter`, followed by nothing yet.
    pub fn new(meter: MeterMap) -> Self {
        let current = Arc::new(meter);
        Self(Arc::new(ProjectInner {
            cell: Arc::new(ArcLend::from_arc(Arc::clone(&current))),
            state: Mutex::new(ProjectState {
                current,
                followers: Vec::new(),
            }),
        }))
    }

    fn state(&self) -> MutexGuard<'_, ProjectState> {
        // A poisoned lock still holds a whole state: `current` and the list
        // are each replaced in one assignment or push.
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Publishes `meter` to the metronome and to every following slot, before
    /// returning. Control thread: allocates, and frees the values it
    /// replaces here.
    pub fn set_meter(&self, meter: Arc<MeterMap>) {
        let mut state = self.state();
        state.current = Arc::clone(&meter);
        self.0.cell.publish(Arc::clone(&meter));
        state.followers.retain(|follower| match follower.upgrade() {
            Some(cell) => {
                cell.publish(Arc::new(Some(Arc::clone(&meter))));
                true
            }
            None => false,
        });
    }

    /// The meter last published. Control side: a lock and an `Arc` clone,
    /// never a read of the cell the audio thread reads.
    pub fn current(&self) -> Arc<MeterMap> {
        Arc::clone(&self.state().current)
    }

    /// Whether `self` and `other` are the same project meter.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// The cell the metronome reads, for the node built over it.
    pub(crate) fn cell(&self) -> Arc<ArcLend<MeterMap>> {
        Arc::clone(&self.0.cell)
    }

    /// Start publishing to `slot`, with the meter in force now. Idempotent:
    /// a slot already listed is listed once, so a re-follow of the same meter
    /// does not make each later `set_meter` publish into it twice.
    fn attach(&self, slot: &Arc<MeterSlotCell>) {
        let mut state = self.state();
        slot.publish(Arc::new(Some(Arc::clone(&state.current))));
        let me = Arc::downgrade(slot);
        state
            .followers
            .retain(|f| f.strong_count() > 0 && !Weak::ptr_eq(f, &me));
        state.followers.push(me);
    }

    /// Stop publishing to `slot`, leaving what it holds: for a slot moving to
    /// another meter, whose `attach` replaces the value in one publish.
    fn forget(&self, slot: &Arc<MeterSlotCell>) {
        let mut state = self.state();
        let gone = Arc::downgrade(slot);
        state
            .followers
            .retain(|f| f.strong_count() > 0 && !Weak::ptr_eq(f, &gone));
    }

    /// How many live slots follow this meter (tests).
    #[cfg(test)]
    fn follower_count(&self) -> usize {
        self.state()
            .followers
            .iter()
            .filter(|f| f.strong_count() > 0)
            .count()
    }
}

impl Default for ProjectMeter {
    /// 4/4 from bar 0.
    fn default() -> Self {
        Self::new(MeterMap::default())
    }
}

impl std::fmt::Debug for ProjectMeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectMeter")
            .field("current", &self.current())
            .finish_non_exhaustive()
    }
}

/// A node's own meter cell: empty, or following one [`ProjectMeter`].
///
/// The node holds [`cell`](Self::cell), built with it and never swapped, and
/// reads it once per block. A host gives the node the project meter with
/// [`follow`](Self::follow) and takes it away with
/// [`unfollow`](Self::unfollow): each a publish into that cell. A clone is
/// another handle on the same slot.
#[derive(Clone)]
pub struct MeterSlot(Arc<SlotInner>);

struct SlotInner {
    cell: Arc<MeterSlotCell>,
    /// The project meter followed, for the control side
    /// ([`following`](MeterSlot::following)). The audio thread never takes
    /// this lock.
    follows: Mutex<Option<ProjectMeter>>,
}

impl Default for MeterSlot {
    /// Following nothing.
    fn default() -> Self {
        Self(Arc::new(SlotInner {
            cell: Arc::new(ArcLend::new(None)),
            follows: Mutex::new(None),
        }))
    }
}

impl MeterSlot {
    /// A slot following `meter`, or nothing.
    pub fn following_meter(meter: Option<ProjectMeter>) -> Self {
        let slot = Self::default();
        if let Some(meter) = meter {
            slot.follow(meter);
        }
        slot
    }

    fn follows(&self) -> MutexGuard<'_, Option<ProjectMeter>> {
        // A poisoned lock still holds a whole `Option`, set in one assignment.
        self.0
            .follows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Follow `meter` from the node's next block: its meter now, and every
    /// one it publishes after. Replaces any meter followed before, in one
    /// publish: the node goes from the old meter's map straight to the new
    /// one's, never through "no meter" (4/4) in between. Following the meter
    /// already followed changes nothing. Control thread.
    pub fn follow(&self, meter: ProjectMeter) {
        let mut follows = self.follows();
        if let Some(old) = follows.take() {
            if !old.ptr_eq(&meter) {
                old.forget(&self.0.cell);
            }
        }
        meter.attach(&self.0.cell);
        *follows = Some(meter);
    }

    /// Follow nothing: the node reads no meter from its next block. Control
    /// thread.
    pub fn unfollow(&self) {
        let mut follows = self.follows();
        if let Some(old) = follows.take() {
            old.forget(&self.0.cell);
        }
        self.0.cell.publish(Arc::new(None));
    }

    /// The project meter followed, if any. Control side: a lock, never a read
    /// of the node's cell.
    pub fn following(&self) -> Option<ProjectMeter> {
        self.follows().clone()
    }

    /// Whether a project meter is followed.
    pub fn is_following(&self) -> bool {
        self.follows().is_some()
    }

    /// The cell the node reads, once per block: for the node, at its build.
    pub fn cell(&self) -> Arc<MeterSlotCell> {
        Arc::clone(&self.0.cell)
    }
}

impl std::fmt::Debug for MeterSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeterSlot")
            .field("following", &self.is_following())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::meter::{MeterChange, TimeSignature};
    use tutti_types::value::Beat;

    fn meter_of(n: u32) -> Arc<MeterMap> {
        Arc::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            TimeSignature::from_parts(n, 4),
        )]))
    }

    /// What a node reading `cell` sees this block.
    fn read(cell: &MeterSlotCell) -> Option<Arc<MeterMap>> {
        Option::clone(&cell.lend())
    }

    /// **One `set_meter` reaches the metronome and every following slot
    /// before it returns**, so a block after it reads the same meter in each,
    /// as one shared cell did. A slot that stops following, or follows
    /// another meter, no longer hears it.
    ///
    /// Mutations (run): `set_meter` not publishing to the followers → the
    /// slots read the old meter, fails; `forget` not removing the slot from
    /// the list → the unfollowed slot reads 5/4, fails; `follow` not
    /// forgetting the meter it followed before → the moved slot reads 5/4,
    /// fails.
    ///
    /// Not covered here: that a moved slot goes from the old meter's map to
    /// the new one's in one publish, never through `None`. The two would be
    /// consecutive publishes inside one `follow` call, which a reader on one
    /// thread cannot land between, and `ArcLend` exposes no publish count; it
    /// is kept by `follow` calling `forget` (which publishes nothing) rather
    /// than `unfollow`'s publish of `None`.
    #[test]
    fn set_meter_reaches_every_follower_before_it_returns() {
        let project = ProjectMeter::new(MeterMap::default());
        let click = project.cell();
        let slots: Vec<MeterSlot> = (0..3).map(|_| MeterSlot::default()).collect();
        for slot in &slots {
            assert!(read(&slot.cell()).is_none(), "a new slot follows nothing");
            slot.follow(project.clone());
        }
        assert_eq!(project.follower_count(), 3);

        let seven = meter_of(7);
        project.set_meter(Arc::clone(&seven));
        assert!(
            std::ptr::eq(&*click.lend(), &*seven),
            "the metronome's cell"
        );
        for slot in &slots {
            let seen = read(&slot.cell()).expect("a follower holds the meter");
            assert!(Arc::ptr_eq(&seen, &seven), "the same meter, not a copy");
        }

        // One stops following, one moves to another project meter.
        slots[0].unfollow();
        let other = ProjectMeter::new(MeterMap::default());
        slots[1].follow(other.clone());
        project.set_meter(meter_of(5));
        assert!(read(&slots[0].cell()).is_none(), "unfollowed reads none");
        assert_eq!(
            read(&slots[1].cell()).unwrap().changes()[0].signature,
            TimeSignature::from_parts(4, 4),
            "a moved slot reads its new meter"
        );
        assert_eq!(
            read(&slots[2].cell()).unwrap().changes()[0].signature,
            TimeSignature::from_parts(5, 4)
        );
        assert!(slots[1].following().unwrap().ptr_eq(&other));
        assert!(!slots[0].is_following());
    }

    /// **Following the same meter again lists the slot once**, so a host
    /// that re-installs the project meter (each `PluginControls::set_meter`)
    /// does not make every later `set_meter` publish into the node N times.
    ///
    /// Mutation (run): `attach` pushing without first removing the slot's
    /// own entry (`!Weak::ptr_eq(f, &me)` dropped from its `retain`) → five
    /// follows list five followers, fails.
    #[test]
    fn following_the_same_meter_again_lists_the_slot_once() {
        let project = ProjectMeter::default();
        let slot = MeterSlot::default();
        for _ in 0..5 {
            slot.follow(project.clone());
        }
        assert_eq!(project.follower_count(), 1);
        project.set_meter(meter_of(6));
        assert_eq!(
            read(&slot.cell()).unwrap().changes()[0].signature,
            TimeSignature::from_parts(6, 4),
            "still following"
        );
    }

    /// A slot whose node and handles are gone is dropped from the project's
    /// list, not kept alive or published to.
    ///
    /// Mutation (run): `set_meter` keeping a follower whose slot is gone
    /// (`None => true` in its `retain`) → the dead slot stays listed, fails.
    #[test]
    fn a_dropped_slot_leaves_the_list() {
        let project = ProjectMeter::default();
        let kept = MeterSlot::following_meter(Some(project.clone()));
        let dropped = MeterSlot::following_meter(Some(project.clone()));
        let weak = Arc::downgrade(&dropped.cell());
        drop(dropped);
        assert!(weak.upgrade().is_none(), "the list holds no strong count");
        project.set_meter(meter_of(3));
        assert_eq!(project.state().followers.len(), 1);
        assert!(kept.is_following());
    }
}
