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

use tutti_graph::MAX_ROOT_CHANNELS;
use tutti_types::{ChannelLayout, Fold, FoldError};

/// The widest device an engine can render into without being told
/// ([`Engine::support_output`](crate::Engine::support_output)).
pub(crate) const DEFAULT_DEVICE_WIDTHS: usize = 16;

/// The folds from each root width, `1..=MAX_ROOT_CHANNELS`, to each device
/// width supported.
#[derive(Debug, Clone)]
pub(crate) struct OutputFolds {
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
            by_device: Vec::new(),
        };
        for device in 1..=widest {
            let _ = folds.support(ChannelLayout::from(device));
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
                .map(|w| Fold::widths(ChannelLayout::from(w), device).ok())
                .collect();
            self.by_device[slot] = Some(by_root);
        }
        Ok(())
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
