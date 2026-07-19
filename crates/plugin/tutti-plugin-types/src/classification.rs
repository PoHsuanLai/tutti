//! Shared plugin-classification vocabulary.
//!
//! Each native format reports a category/type taxonomy. These neutral mirrors
//! live here so both the format host crate (which maps its native SDK enum into
//! one, e.g. `tutti-vst2-host` from the `vst` crate's `Category`) and the
//! catalog/wire layer in `tutti-plugin` (which embeds it in `PluginClass`)
//! speak one type instead of each keeping a private copy plus a hand-written
//! cross-walk.
//!
//! Distinct from [`crate::metadata`], which holds the *engine-wiring* load data
//! (bus widths, latency); this is the *classification* taxonomy.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The plugin's declared VST2 category — a neutral mirror of the `vst` crate's
/// `Category`, owned here so neither the host crate's public API nor the wire
/// vocab leaks the `vst` dependency. `tutti-vst2-host` maps the native value
/// into this; `tutti-plugin` embeds it in `PluginClass::Vst2`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Vst2Category {
    #[default]
    Unknown,
    Effect,
    Synth,
    Analysis,
    Mastering,
    Spacializer,
    RoomFx,
    SurroundFx,
    Restoration,
    OfflineProcess,
    Shell,
    Generator,
}
