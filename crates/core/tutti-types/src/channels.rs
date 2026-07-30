//! [`ChannelLayout`] — the engine's one answer to "mono, stereo, or how many?".
//!
//! Before this, the same idea was re-encoded a dozen ways: a private
//! `Mono`/`Stereo` enum in export, an `AudioPortType` in the CLAP host, a
//! `channels: usize`/`u8`/`u32` field on nearly every node and device, a
//! `net.outputs() >= 2` boolean, a `wave.channels() >= 2` branch. They all mean
//! the same thing. This is the shared type they collapse into.
//!
//! It does *not* try to model speaker topology (which channel is front-left vs
//! LFE). That is a separate, richer concern; a surround bus here is just
//! `Multi(6)` — a count, not a placement. The foreign layout types that *do*
//! carry placement (VST3 `SpeakerArrangement`, CLAP port tags) convert to and
//! from this at their crate boundary, losing the placement deliberately.
//!
//! # Canonical construction
//!
//! Always build from a count via [`ChannelLayout::from_count`] (or the `From`
//! impls), never `Multi(1)` / `Multi(2)` by hand: construction folds `1 → Mono`
//! and `2 → Stereo`, so two layouts with the same channel count are always the
//! same value. Without that, `Multi(2)` and `Stereo` would compare unequal
//! despite meaning the same thing — a footgun in every `match`.

/// How many audio channels a signal, node port, bus, wave, or device carries.
///
/// The shared "mono vs stereo vs N" vocabulary across the whole engine. Construct
/// from a count with [`from_count`](Self::from_count) or the [`From`] impls so the
/// value is always canonical (see [module docs](self)); read the count back with
/// [`count`](Self::count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ChannelLayout {
    /// One channel.
    Mono,
    /// Two channels.
    Stereo,
    /// Four channels (quad / ambisonic B-format). Named because it's the one
    /// surround width common enough — and unambiguous enough (4 is 4, no
    /// placement question) — to earn a variant. Higher surround widths stay
    /// `Multi(n)`: naming `5.1`/`7.1` would imply a speaker *order* this
    /// count-only type doesn't carry.
    Quad,
    /// Any other width — surround (`Multi(6)`), ambisonics, or the empty bus
    /// (`Multi(0)`). Never holds `1`, `2`, or `4`; those canonicalize to the
    /// named variants.
    Multi(u16),
}

impl ChannelLayout {
    /// The channel count this layout describes.
    pub const fn count(self) -> u16 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Quad => 4,
            Self::Multi(n) => n,
        }
    }

    /// Whether this layout carries exactly one channel.
    ///
    /// The canonical "is there a second channel to read?" test, for the very
    /// common node shape that takes `left` and falls back to `left` for `right`
    /// when the input is mono.
    ///
    /// This exists because five call sites open-coded it as
    /// `matches!(l, Stereo | Quad | Multi(_))` — which is **wrong for
    /// `Multi(0)`**: an empty bus has no second channel, yet it matches the
    /// `Multi(_)` arm and the node then reads channel 1 of a zero-width input.
    /// Asking the count is right for every width, including the empty one.
    pub const fn is_mono(self) -> bool {
        self.count() == 1
    }

    /// Whether this layout carries two or more channels — the negation of
    /// [`is_mono`](Self::is_mono) for any non-empty layout.
    ///
    /// `Multi(0)` is neither mono nor multi: it has no channels at all. Both
    /// predicates report `false` for it, which is what a caller branching on
    /// "is there a channel 1 to read?" needs.
    pub const fn is_multi(self) -> bool {
        self.count() > 1
    }

    /// Build a layout from a channel count, canonicalizing `1 → Mono`,
    /// `2 → Stereo`, `4 → Quad`. Use this (or the [`From`] impls) rather than
    /// `Multi(n)` directly, so equal counts are always equal values.
    pub const fn from_count(n: u16) -> Self {
        match n {
            1 => Self::Mono,
            2 => Self::Stereo,
            4 => Self::Quad,
            n => Self::Multi(n),
        }
    }
}

impl Default for ChannelLayout {
    /// Stereo — the overwhelmingly common edge default across the engine.
    fn default() -> Self {
        Self::Stereo
    }
}

macro_rules! count_conversions {
    ($($t:ty),*) => {$(
        impl From<$t> for ChannelLayout {
            fn from(n: $t) -> Self {
                Self::from_count(n as u16)
            }
        }
        impl From<ChannelLayout> for $t {
            fn from(l: ChannelLayout) -> Self {
                l.count() as $t
            }
        }
    )*};
}

count_conversions!(u8, u16, u32, usize);

#[cfg(test)]
mod tests {
    use super::*;

    /// Construction folds `1/2/4` onto the named variants, so two layouts with
    /// the same count are always the same value.
    #[test]
    fn construction_is_canonical() {
        assert_eq!(ChannelLayout::from_count(1), ChannelLayout::Mono);
        assert_eq!(ChannelLayout::from_count(2), ChannelLayout::Stereo);
        assert_eq!(ChannelLayout::from_count(4), ChannelLayout::Quad);
        assert_eq!(ChannelLayout::from_count(6), ChannelLayout::Multi(6));
        assert_eq!(ChannelLayout::from(2usize), ChannelLayout::Stereo);
    }

    #[test]
    fn is_mono_is_true_for_exactly_one_channel() {
        assert!(ChannelLayout::Mono.is_mono());
        assert!(!ChannelLayout::Stereo.is_mono());
        assert!(!ChannelLayout::Quad.is_mono());
        assert!(!ChannelLayout::Multi(6).is_mono());
    }

    /// The bug the helper exists to prevent.
    ///
    /// Five call sites tested "is there a second channel?" as
    /// `matches!(l, Stereo | Quad | Multi(_))`. `Multi(0)` matches that
    /// pattern, so an empty bus was treated as having a channel 1 to read.
    /// Both predicates must reject it: an empty layout is neither.
    #[test]
    fn an_empty_layout_is_neither_mono_nor_multi() {
        let empty = ChannelLayout::Multi(0);
        assert_eq!(empty.count(), 0);
        assert!(
            !empty.is_multi(),
            "Multi(0) has no second channel, but the old `Multi(_)` pattern said it did"
        );
        assert!(!empty.is_mono(), "Multi(0) has no channels at all");
    }

    /// For every non-empty width the two predicates partition exactly.
    #[test]
    fn the_predicates_partition_every_non_empty_width() {
        for n in 1..=12u16 {
            let layout = ChannelLayout::from_count(n);
            assert_ne!(
                layout.is_mono(),
                layout.is_multi(),
                "width {n} must be exactly one of mono/multi"
            );
        }
    }
}
