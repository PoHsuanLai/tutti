//! What the plugin does, written out. The figures a case expects are here and
//! nowhere else, and they are not imported from the plugin: a case that asked
//! the plugin what to expect could not fail when the plugin is wrong. The
//! plugin's own copy is the spec in `tutti-shared-plugin`'s `core` module.

use tutti_plugin_types::{Normalized, ParamId, Samples};

/// A parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Param {
    /// Persistent, default 0.5; the level the Level mode renders.
    Level,
    /// Persistent, default 0.25.
    Aux,
    /// The mode, read at activation.
    Mode,
}

impl Param {
    /// Every parameter the plugin lists, with its default.
    pub const ALL: [(Param, f64); 3] =
        [(Param::Level, 0.5), (Param::Aux, 0.25), (Param::Mode, 0.0)];

    /// The id every format lists it under.
    #[must_use]
    pub const fn id(self) -> ParamId {
        ParamId::new(self as u32)
    }
}

/// An id the plugin does not list.
pub const UNLISTED: ParamId = ParamId::new(77);

/// What a block renders. `tag` is `bus * 1000 + channel + 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `in + tag`.
    Tag,
    /// `in` delayed by [`LATENCY`], plus `tag`; reports [`LATENCY`].
    Delay,
    /// The level, as it stands at each sample.
    Level,
    /// `key + 1000 * channel` while a note is held, else 0.
    Notes,
    /// Writes nothing.
    Silent,
}

impl Mode {
    /// The number a state blob stores.
    #[must_use]
    pub const fn index(self) -> u32 {
        self as u32
    }

    /// The normalized value of the mode parameter that selects it.
    #[must_use]
    pub fn value(self) -> Normalized {
        Normalized::new(f64::from(self.index()) / 4.0)
    }
}

/// The Delay mode's latency.
pub const LATENCY: Samples = Samples(137);
/// The tail, in every mode.
pub const TAIL: Samples = Samples(8191);

/// The tag on channel `channel` of the main bus.
#[must_use]
pub fn tag(channel: usize) -> f32 {
    (channel + 1) as f32
}
