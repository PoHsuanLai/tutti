//! [`ChannelLayout`] — the engine's one answer to "mono, stereo, or how many?".
//!
//! A newtype over the count rather than an enum of named widths, so there is
//! exactly one way to spell each width: two layouts of the same width are
//! always the same value, hash the same, and match the same arm. `from_count`
//! stays as the `const` primitive the `From` impls delegate to, because
//! `From::from` cannot be `const`.

/// How many audio channels a signal, node port, bus, wave, or device carries.
///
/// The shared "mono vs stereo vs N" vocabulary across the whole engine, in
/// place of a private enum or a bare `channels: usize` per subsystem. Read the
/// count back with [`count`](Self::count).
///
/// It is a count, not a placement: a surround bus is a width of 6, with no
/// statement about which channel is front-left or LFE. That is
/// [`ChannelTopology`](crate::ChannelTopology)'s job. Foreign layout types that
/// carry placement (VST3 `SpeakerArrangement`, CLAP port tags) convert to this
/// at their crate boundary.
///
/// # Constructing one
///
/// A named constant if the width has a name ([`MONO`](Self::MONO),
/// [`STEREO`](Self::STEREO), [`QUAD`](Self::QUAD), [`EMPTY`](Self::EMPTY)),
/// [`From`]/[`Into`] from any of `u8`, `u16`, `u32` or `usize` otherwise.
/// 5.1 and 7.1 get no names, because a name would imply a speaker *order* this
/// count-only type does not carry. A count above `u16::MAX` is truncated.
///
/// Ordering is by channel count, so `a > b` reads as "a is wider than b".
///
/// # No `Default`
///
/// Nothing about a width has a neutral value (stereo is a guess, and `EMPTY`
/// is a different guess), so a struct holding one names the width it means:
///
/// ```compile_fail
/// # use tutti_types::ChannelLayout;
/// // A width is always chosen, never defaulted.
/// let _ = ChannelLayout::default();
/// ```
///
/// # Examples
///
/// ```
/// use tutti_types::ChannelLayout;
///
/// let surround = ChannelLayout::from(6usize);
/// assert_eq!(surround.count(), 6);
/// assert!(surround > ChannelLayout::STEREO);
/// assert!(ChannelLayout::STEREO.is_multi());
/// assert!(!ChannelLayout::EMPTY.is_mono() && !ChannelLayout::EMPTY.is_multi());
/// assert_eq!(format!("{:?}", ChannelLayout::from(2u32)), "Stereo");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
pub struct ChannelLayout(u16);

/// Names the common widths, so a diagnostic reads `Stereo` rather than
/// `ChannelLayout(2)`.
///
/// Hand-written rather than derived: the derive on a newtype prints the wrapper
/// and the number, and error messages depend on the name. "cannot record
/// Stereo into 6 channels" is the line an author has to act on.
impl core::fmt::Debug for ChannelLayout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            0 => f.write_str("Empty"),
            1 => f.write_str("Mono"),
            2 => f.write_str("Stereo"),
            4 => f.write_str("Quad"),
            n => write!(f, "{n} channels"),
        }
    }
}

impl ChannelLayout {
    /// No channels at all — the empty bus.
    ///
    /// Neither mono nor multi: both predicates report `false`, which is what a
    /// caller branching on "is there a channel to read?" needs.
    pub const EMPTY: Self = Self(0);

    /// One channel.
    pub const MONO: Self = Self(1);

    /// Two channels.
    pub const STEREO: Self = Self(2);

    /// Four channels (quad / ambisonic B-format). Named because it's the one
    /// surround width common enough — and unambiguous enough (4 is 4, no
    /// placement question) — to earn a name.
    pub const QUAD: Self = Self(4);

    /// Returns the channel count this layout describes.
    pub const fn count(self) -> u16 {
        self.0
    }

    /// Returns whether this layout carries exactly one channel.
    ///
    /// The canonical "is there a second channel to read?" test, for the very
    /// common node shape that takes `left` and falls back to `left` for `right`
    /// when the input is mono. [`EMPTY`](Self::EMPTY) is not mono.
    pub const fn is_mono(self) -> bool {
        self.0 == 1
    }

    /// Returns whether this layout carries two or more channels — the negation of
    /// [`is_mono`](Self::is_mono) for any non-empty layout.
    ///
    /// [`EMPTY`](Self::EMPTY) is neither mono nor multi: it has no channels at
    /// all. Both predicates report `false` for it.
    pub const fn is_multi(self) -> bool {
        self.0 > 1
    }

    /// Creates a layout from a channel count, in `const` context.
    ///
    /// **Prefer [`From`]/[`Into`] at call sites** — `ChannelLayout::from(n)` or
    /// `n.into()`. This exists because `From::from` cannot be `const`, so a
    /// `const` item or an associated constant has no other way to spell a width
    /// that isn't one of the named ones. The `From` impls delegate here.
    pub const fn from_count(n: u16) -> Self {
        Self(n)
    }
}

/// Build a layout from any integer width.
///
/// Multi-type on purpose: every upstream audio API reports a channel count in a
/// different integer type — CoreAudio's `mChannelsPerFrame` is `u32`, CPAL's is
/// `u16`, `Vec::len` and VST3's bus counts are `usize`. Accepting each one lets
/// `impl Into<ChannelLayout>` parameters take them straight through, instead
/// of scattering `as u16` casts across the FFI boundaries.
///
/// There is deliberately **no reverse impl**. `ChannelLayout → u8` would
/// truncate above 255 channels, silently, inside a trait that promises not to
/// lose data; and the wider ones were never used. Read the width with
/// [`count`](ChannelLayout::count) and cast explicitly if you need another type.
macro_rules! count_conversions {
    ($($t:ty),*) => {$(
        impl From<$t> for ChannelLayout {
            fn from(n: $t) -> Self {
                Self::from_count(n as u16)
            }
        }
    )*};
}

count_conversions!(u8, u16, u32, usize);

#[cfg(test)]
mod tests {
    use super::*;

    /// Every count has exactly one representation, so equal counts are always
    /// equal values — including for the widths that have names, and whichever
    /// of the two constructors built them.
    #[test]
    fn construction_is_canonical() {
        assert_eq!(ChannelLayout::from(1u16), ChannelLayout::MONO);
        assert_eq!(ChannelLayout::from(2u16), ChannelLayout::STEREO);
        assert_eq!(ChannelLayout::from(4u16), ChannelLayout::QUAD);
        assert_eq!(ChannelLayout::from(0u16), ChannelLayout::EMPTY);
        assert_eq!(ChannelLayout::from(6u16).count(), 6);

        // The `const` primitive and the `From` impls must not diverge — the
        // latter delegate to the former, and call sites mix both.
        for n in 0..=12u16 {
            assert_eq!(ChannelLayout::from_count(n), ChannelLayout::from(n));
        }
    }

    /// Every integer width converts, whatever type the upstream API reports it
    /// in — the reason the `From` impls are multi-type rather than `u16`-only.
    #[test]
    fn every_integer_width_converts() {
        assert_eq!(ChannelLayout::from(2u8), ChannelLayout::STEREO);
        assert_eq!(ChannelLayout::from(2u16), ChannelLayout::STEREO);
        assert_eq!(ChannelLayout::from(2u32), ChannelLayout::STEREO);
        assert_eq!(ChannelLayout::from(2usize), ChannelLayout::STEREO);

        // `.into()` resolves the same way, which is the spelling call sites use
        // when the target type is already known.
        let inferred: ChannelLayout = 6usize.into();
        assert_eq!(inferred.count(), 6);
    }

    /// The named constants report the widths their names claim.
    #[test]
    fn the_named_widths_have_the_counts_they_say() {
        assert_eq!(ChannelLayout::EMPTY.count(), 0);
        assert_eq!(ChannelLayout::MONO.count(), 1);
        assert_eq!(ChannelLayout::STEREO.count(), 2);
        assert_eq!(ChannelLayout::QUAD.count(), 4);
    }

    #[test]
    fn is_mono_is_true_for_exactly_one_channel() {
        assert!(ChannelLayout::MONO.is_mono());
        assert!(!ChannelLayout::STEREO.is_mono());
        assert!(!ChannelLayout::QUAD.is_mono());
        assert!(!ChannelLayout::from_count(6).is_mono());
    }

    /// An empty layout has no second channel to read, and no first one either:
    /// both predicates must reject it.
    #[test]
    fn an_empty_layout_is_neither_mono_nor_multi() {
        let empty = ChannelLayout::EMPTY;
        assert_eq!(empty.count(), 0);
        assert!(!empty.is_multi(), "the empty layout has no second channel");
        assert!(!empty.is_mono(), "the empty layout has no channels at all");
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

    /// `Debug` names the common widths.
    ///
    /// Load-bearing, not cosmetic: width-mismatch errors (`Recorder::start`)
    /// `Debug`-format both layouts, and a derived newtype `Debug` would print
    /// `ChannelLayout(2)`.
    #[test]
    fn debug_names_the_common_widths() {
        assert_eq!(format!("{:?}", ChannelLayout::EMPTY), "Empty");
        assert_eq!(format!("{:?}", ChannelLayout::MONO), "Mono");
        assert_eq!(format!("{:?}", ChannelLayout::STEREO), "Stereo");
        assert_eq!(format!("{:?}", ChannelLayout::QUAD), "Quad");
        assert_eq!(format!("{:?}", ChannelLayout::from_count(6)), "6 channels");
    }

    /// Ordering is by width.
    #[test]
    fn ordering_is_by_channel_count() {
        assert!(ChannelLayout::MONO < ChannelLayout::STEREO);
        assert!(ChannelLayout::STEREO < ChannelLayout::QUAD);
        assert!(ChannelLayout::QUAD < ChannelLayout::from_count(6));
        assert!(ChannelLayout::EMPTY < ChannelLayout::MONO);

        let mut widths = [
            ChannelLayout::from_count(6),
            ChannelLayout::MONO,
            ChannelLayout::QUAD,
            ChannelLayout::STEREO,
        ];
        widths.sort();
        assert_eq!(
            widths.map(ChannelLayout::count),
            [1, 2, 4, 6],
            "sorting layouts must order them by width"
        );
    }
}
