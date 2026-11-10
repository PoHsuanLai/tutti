//! The fold from the graph root to the device: built once on the control side,
//! lent to the audio thread.
//!
//! The root renders at the width of its plan (at most
//! [`MAX_ROOT_CHANNELS`]) and the device
//! callback hands over a buffer of its own width. [`Fold`]
//! allocates when built, and a plan arrives on the audio thread (a commit
//! installed at a block's start), so the folds are built for *every* root width
//! and every device width up to [`DEFAULT_DEVICE_WIDTHS`] ahead of time, and a
//! block picks the one for the root it finds and the buffer it is given. A plan
//! change then needs no rebuild; a wider device is added on the control side
//! ([`Engine::support_output`](crate::Engine::support_output)).
//!
//! # What the widths mean
//!
//! A device states its channel count and nothing else (`cpal` has no channel
//! map), and so does the graph root. Each is the engine's order for its width
//! where it has one ([`ChannelTopology::assumed`](tutti_types::ChannelTopology::assumed):
//! mono, stereo, quad, 5.1, 7.1, 7.1.4), so a 5.1 root plays folded on a stereo
//! device and a 7.1 root plays on a 5.1 one by position. Where either has none
//! (3, 5, 7, 9 to 11, 13 and up) no speaker can be named, and channels go by
//! index ([`Fold::by_index`](tutti_types::Fold::by_index)): zero-filled when the
//! device is wider, and a pair that would drop a channel has no fold, so that
//! root renders a silent, counted block into that device.

use std::sync::{Arc, Mutex, PoisonError};
use tutti_graph::MAX_ROOT_CHANNELS;
use tutti_rt::ArcLend;
use tutti_types::{ChannelLayout, ChannelTopology, Fold, FoldError};

/// The widest device an engine can render into without being told
/// ([`Engine::support_output`](crate::Engine::support_output)).
pub(crate) const DEFAULT_DEVICE_WIDTHS: usize = 16;

/// The folds from each root width, `1..=MAX_ROOT_CHANNELS`, to each device
/// width supported.
#[derive(Debug, Clone)]
pub(crate) struct OutputFolds {
    /// The root topologies declared ([`OutputControl::declare_root`]), at most
    /// one per root width, sorted by width: the one last declared at each
    /// width, so a plan of that width is folded from it. Every other width is
    /// a bare count.
    ///
    /// **Why a table by width, and its limit.** The audio thread installs a
    /// plan some blocks after a declaration, and the engine cannot see when,
    /// so the fold has to follow the plan and not the newest declaration. A
    /// plan carries a width and not the topology it was compiled under, so the
    /// width is the key: declaring at another width leaves the entries at the
    /// widths already held, and a plan still rendering at one of them keeps
    /// its fold through any number of declarations. The limit is what a width
    /// cannot tell: a declaration at the *same* width as the plan in flight
    /// replaces its entry at once (the plan is the one it describes, only its
    /// reading changes), and undeclaring (`None`) drops every entry at once,
    /// including a plan still in flight at another width, which reads as a
    /// bare count until it is replaced. Exact, per plan, would carry the
    /// topology in `tutti_graph::Topology` and its `Plan`.
    roots: Vec<ChannelTopology>,
    /// Index `d - 1` holds the folds into a device of `d` channels, `None` for
    /// a width not supported; inside, index `w - 1` folds a root of `w`.
    by_device: Vec<Option<Box<[Option<Fold>]>>>,
}

impl OutputFolds {
    /// The folds into devices of `1..=widest` channels. Control side: it
    /// allocates.
    ///
    /// Every bare width places (`every_root_width_places_into_any_device`), so
    /// none is skipped; one that did not would be an unsupported width, and a
    /// silent block, never a wrong one.
    pub(crate) fn up_to(widest: usize) -> Self {
        let mut folds = Self {
            roots: Vec::new(),
            by_device: Vec::new(),
        };
        for device in 1..=widest {
            let device = ChannelLayout::try_from(device)
                .expect("a device width the engine holds ahead is small");
            let _ = folds.support(device);
        }
        folds
    }

    /// Whether the folds into a device of `device` channels are held.
    pub(crate) fn supports(&self, device: ChannelLayout) -> bool {
        usize::from(device.count())
            .checked_sub(1)
            .and_then(|slot| self.by_device.get(slot))
            .is_some_and(Option::is_some)
    }

    /// Adds the folds into a device of `device` channels (a no-op if present).
    ///
    /// # Errors
    ///
    /// [`FoldError::Empty`] for a device of no channels.
    pub(crate) fn support(&mut self, device: ChannelLayout) -> Result<(), FoldError> {
        let Some(slot) = usize::from(device.count()).checked_sub(1) else {
            return Err(FoldError::Empty(tutti_types::FoldEnd::Destination));
        };
        if self.by_device.len() <= slot {
            self.by_device.resize(slot + 1, None);
        }
        if self.by_device[slot].is_none() {
            // A pair that cannot be placed (a narrowing with an unordered end)
            // has no fold: that root width renders a silent, counted block
            // into this device. The builders refuse such a pair up front
            // (`SessionBuilder`), so only a plan that changes the root width
            // later meets it.
            let by_root = (1..=MAX_ROOT_CHANNELS)
                .map(|w| self.root_fold(w, device).ok())
                .collect();
            self.by_device[slot] = Some(by_root);
        }
        Ok(())
    }

    /// The fold of a root of `width` channels into `device`: from the declared
    /// topology when the root is as wide as the declaration, otherwise from the
    /// engine's order for the bare width.
    fn root_fold(&self, width: usize, device: ChannelLayout) -> Result<Fold, FoldError> {
        let declared = self
            .roots
            .iter()
            .find(|root| usize::from(root.layout().count()) == width);
        match declared {
            Some(root) => Fold::declared(root, device),
            None => Fold::widths(
                ChannelLayout::try_from(width).expect("a root width is within MAX_ROOT_CHANNELS"),
                device,
            ),
        }
    }

    /// The fold of a root of `root` channels into a device of `device`, or
    /// `None` for a root of no channels (or more than the engine renders), a
    /// device not supported, or a pair that cannot be placed.
    pub(crate) fn get(&self, device: usize, root: usize) -> Option<&Fold> {
        self.by_device
            .get(device.checked_sub(1)?)?
            .as_ref()?
            .get(root.checked_sub(1)?)?
            .as_ref()
    }

    /// Whether a device of `device` channels is held (its pairs may still be
    /// unplaceable).
    pub(crate) fn has_device(&self, device: usize) -> bool {
        device
            .checked_sub(1)
            .and_then(|slot| self.by_device.get(slot))
            .is_some_and(Option::is_some)
    }
}

/// The control side of the engine's output folds: what the session keeps when
/// the [`Engine`](crate::Engine) goes to the audio thread, to say what the
/// graph root is and which devices it can take.
///
/// Cloneable; every clone publishes into the one cell the engine lends from.
/// A publish is a read-modify-write, so clones serialize on a lock (control
/// side only; the audio thread never touches it).
#[derive(Debug, Clone)]
pub struct OutputControl {
    cell: Arc<ArcLend<OutputFolds>>,
    writer: Arc<Mutex<()>>,
}

impl OutputControl {
    pub(crate) fn new(folds: OutputFolds) -> Self {
        Self {
            cell: Arc::new(ArcLend::new(folds)),
            writer: Arc::default(),
        }
    }

    pub(crate) fn cell(&self) -> &Arc<ArcLend<OutputFolds>> {
        &self.cell
    }

    /// Publishes the current set as `edit` leaves it, unless `edit` finds
    /// nothing to do (`Ok(false)`). Nothing is published on an error.
    fn update(
        &self,
        edit: impl FnOnce(&mut OutputFolds) -> Result<bool, FoldError>,
    ) -> Result<(), FoldError> {
        let _writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let mut folds = (*self.cell.current()).clone();
        if edit(&mut folds)? {
            self.cell.publish(folds);
            self.cell.reclaim();
        }
        Ok(())
    }

    /// As [`Engine::support_output`](crate::Engine::support_output).
    ///
    /// # Errors
    ///
    /// [`FoldError::Empty`] for a device of no channels.
    pub fn support(&self, device: ChannelLayout) -> Result<(), FoldError> {
        if self.cell.current().supports(device) {
            return Ok(());
        }
        self.update(|folds| folds.support(device).map(|()| true))
    }

    /// Declares what the graph root's channels are, for `device`: the engine
    /// folds a root of that many channels from `root` instead of from the
    /// engine's order for the bare width, here and into every other device.
    /// `None` takes the declaration back.
    ///
    /// A declaration of independent buses
    /// ([`ChannelTopology::discrete`]) stays by index: a 4-wide one into a
    /// 6-wide device keeps channels 2 and 3 at 2 and 3 (undeclared, a 4-wide
    /// root is quad, and they land on the side pair).
    ///
    /// Control side: it allocates, and publishes the new set for the next
    /// block. A callback mid-block keeps the folds it lent.
    ///
    /// # Errors
    ///
    /// The [`FoldError`] when the declaration cannot be placed on `device`
    /// (a discrete root wider than it, or an unnamed channel); nothing is
    /// published. Other devices the engine holds a fold for that cannot take
    /// it render a silent, counted block, as an unplaceable bare width does.
    pub fn declare_root(
        &self,
        root: Option<ChannelTopology>,
        device: ChannelLayout,
    ) -> Result<(), FoldError> {
        if let Some(root) = &root {
            Fold::declared(root, device)?;
        }
        self.update(|folds| {
            let mut next = folds.roots.clone();
            match &root {
                Some(new) => {
                    next.retain(|old| old.layout() != new.layout());
                    next.push(new.clone());
                    next.sort_by_key(|t| t.layout().count());
                }
                None => next.clear(),
            }
            if next == folds.roots {
                return Ok(false);
            }
            folds.roots = next;
            // Every held device is rebuilt against the new declaration.
            let held = (1..=folds.by_device.len())
                .filter(|d| folds.has_device(*d))
                .map(|d| ChannelLayout::try_from(d).map_err(|_| FoldError::TooWide(d)))
                .collect::<Result<Vec<_>, _>>()?;
            folds.by_device.clear();
            for device in held {
                folds.support(device)?;
            }
            Ok(true)
        })
    }

    /// As [`Engine::reclaim`](crate::Engine::reclaim).
    pub fn reclaim(&self) -> usize {
        self.cell.reclaim()
    }
}
