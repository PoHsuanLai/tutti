//! The project meter a plugin node reads its time signature and bar with.

use std::sync::Arc;

use arc_swap::{ArcSwapOption, Guard};
use tutti_core::{MeterMap, RtPublish};

/// A plugin node's meter cell: the project meter it reads once per block for
/// the time-signature and bar fields of the transport it tells the plugin.
///
/// Every plugin node has one, shared with the node: the out-of-process node's
/// [`PluginControls::meter`](super::PluginControls::meter), and the in-process
/// VST2 node's controls (what inserting it hands back). A clone is another
/// handle on the same cell, so an install through any handle reaches the node
/// the audio thread runs, lock-free and with no graph commit. Empty until a
/// host installs a meter; the node then tells the plugin 4/4 from bar 0.
#[derive(Clone, Default)]
pub struct PluginMeter(Arc<ArcSwapOption<RtPublish<MeterMap>>>);

impl PluginMeter {
    /// Give the node the project meter. Pass the cell the host publishes
    /// meter edits into, and an edit reaches the running plugin with nothing
    /// re-installed. Replaces any meter installed before.
    pub fn set_meter(&self, meter: Arc<RtPublish<MeterMap>>) {
        self.0.store(Some(meter));
    }

    /// Drop the meter; the node tells the plugin 4/4 from bar 0.
    pub fn clear_meter(&self) {
        self.0.store(None);
    }

    /// Whether a meter is installed.
    pub fn has_meter(&self) -> bool {
        self.0.load().is_some()
    }

    /// The installed meter's cell, for the node's once-per-block read:
    /// lock-free, no allocation.
    pub(crate) fn load(&self) -> Guard<Option<Arc<RtPublish<MeterMap>>>> {
        self.0.load()
    }

    /// The meter installed now, to give a fork (whose cell is its own).
    pub(crate) fn load_full(&self) -> Option<Arc<RtPublish<MeterMap>>> {
        self.0.load_full()
    }

    /// A handle on a new cell holding `meter`: an in-process VST2 fork's.
    #[cfg(feature = "vst2")]
    pub(crate) fn holding(meter: Option<Arc<RtPublish<MeterMap>>>) -> Self {
        Self(Arc::new(ArcSwapOption::new(meter)))
    }
}

impl std::fmt::Debug for PluginMeter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginMeter")
            .field("installed", &self.has_meter())
            .finish()
    }
}
