//! [`ChannelLayout`] — the engine's one answer to "mono, stereo, or how many?".
//!
//! Before this, the same idea was re-encoded a dozen ways: a private
//! `Mono`/`Stereo` enum in export, an `AudioPortType` in the CLAP host, a
//! `channels: usize`/`u8`/`u32` field on nearly every node and device, a
//! `net.outputs() >= 2` boolean, a `wave.channels() >= 2` branch. They all mean
//! the same thing. This is the shared type they collapse into.
//!
//! It does *not* try to model speaker topology (which channel is front-left vs
//! LFE). That is a separate, richer concern; a surround bus here is just a
//! count of 6 — a count, not a placement. The foreign layout types that *do*
//! carry placement (VST3 `SpeakerArrangement`, CLAP port tags) convert to and
//! from this at their crate boundary, losing the placement deliberately.
//!
//! # Why a newtype and not an enum
//!
//! This was `enum { Mono, Stereo, Quad, Multi(u16) }`, with construction folding
//! `1 → Mono` / `2 → Stereo` / `4 → Quad` so that equal counts compared equal.
//! That canonicalization was a *documented request*, not an invariant: `Multi`
//! was a public tuple variant, so `Multi(2)` was constructible, compared
//! unequal to `Stereo`, hashed differently, and silently missed the `Stereo` arm
//! of every match.
//!
//! The variants also earned nothing. Across the engine only two sites ever
//! matched on them, and both were count tests wearing a variant costume — one
//! read `Mono | Stereo | _`, the other `Stereo | Quad | Multi(6) | Multi(8) |
//! Multi(12) | _`, already reaching through the enum to raw numbers for half its
//! arms. Everything else asked [`count`](ChannelLayout::count).
//!
//! A newtype over the count makes the canonical form the *only* form — there is
//! no second way to spell 2 — and lets the type carry [`Ord`], which the enum
//! could not: a derived ordering there ranked `Quad` below `Multi(2)`. Here
//! "wider than" is just the count's ordering, and it is correct by construction.
//!
//! The named widths survive as associated constants ([`MONO`](ChannelLayout::MONO),
//! [`STEREO`](ChannelLayout::STEREO), [`QUAD`](ChannelLayout::QUAD),
//! [`EMPTY`](ChannelLayout::EMPTY)). Only the four widths that are genuinely
//! unambiguous get names — `5.1`/`7.1` do not, because naming them would imply a
//! speaker *order* this count-only type does not carry.
//!
//! # Constructing one
//!
//! **A named constant if the width has a name, [`From`]/[`Into`] otherwise.**
//! `ChannelLayout::STEREO`, `ChannelLayout::from(6u16)`, `width.into()`.
//!
//! The rule is worth stating because the codebase drifted without it. Two
//! spellings — `from_count(n)` and `From` — coexisted with no division of
//! labour: literals were built both ways (101 sites vs 27), runtime widths were
//! built both ways (21 vs 35), sometimes in the same file, once in the same
//! expression. Worse, `from_count`'s `u16` signature made callers cast *to* it —
//! `from_count(planes.len() as u16)`, `from_count(mChannelsPerFrame as u16)` —
//! so the explicit constructor was manufacturing the very truncating casts the
//! multi-type `From` exists to absorb.
//!
//! [`from_count`](ChannelLayout::from_count) remains, but as the `const`
//! primitive the `From` impls delegate to: `From::from` cannot be `const`, so a
//! `const` item still needs it. It is not call-site vocabulary.
//!
//! # No `Default`, deliberately
//!
//! This used to implement `Default` as `STEREO`. Nothing about a width has a
//! neutral value — stereo is a guess, and `EMPTY` is a different guess — and
//! the guess propagated invisibly through every `#[derive(Default)]` on a struct
//! holding one. That is how `Topology::default()` came to declare two global
//! inputs nobody asked for, so that a `Source::Global(1)` typo validated
//! instead of being rejected. A struct that wants a default now writes its own
//! and names the width it means, which puts the choice where a reader can see
//! it. The absence is pinned by a `compile_fail` doctest (mutation: re-add the
//! impl → the block compiles → the doctest fails):
//!
//! ```compile_fail
//! # use tutti_types::ChannelLayout;
//! // A width is always chosen, never defaulted.
//! let _ = ChannelLayout::default();
//! ```

/// How many audio channels a signal, node port, bus, wave, or device carries.
///
/// The shared "mono vs stereo vs N" vocabulary across the whole engine. Use the
/// named constants for the widths that have names and [`From`]/[`Into`] for
/// anything else (see this module header); read the count back with
/// [`count`](Self::count).
///
/// The inner count is private: every value is canonical, so two layouts of the
/// same width are always the same value (see this module header).
///
/// Ordering is by channel count, so `a > b` reads as "a is wider than b".
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
pub struct ChannelLayout(u16);

/// Names the common widths, so a diagnostic reads `Stereo` rather than
/// `ChannelLayout(2)`.
///
/// Hand-written rather than derived: the derive on a newtype prints the wrapper
/// and the number, and error messages depend on the name. `Recorder::start`
/// reports a source/sink width mismatch by `Debug`-formatting both layouts, and
/// "cannot record Stereo into 6 channels" is the line an author has to act on.
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

    /// The channel count this layout describes.
    pub const fn count(self) -> u16 {
        self.0
    }

    /// Whether this layout carries exactly one channel.
    ///
    /// The canonical "is there a second channel to read?" test, for the very
    /// common node shape that takes `left` and falls back to `left` for `right`
    /// when the input is mono.
    ///
    /// This exists because five call sites open-coded it as
    /// `matches!(l, Stereo | Quad | Multi(_))` — which was **wrong for the empty
    /// layout**: it has no second channel, yet it matched the `Multi(_)` arm and
    /// the node then read channel 1 of a zero-width input. Asking the count is
    /// right for every width, including the empty one.
    pub const fn is_mono(self) -> bool {
        self.0 == 1
    }

    /// Whether this layout carries two or more channels — the negation of
    /// [`is_mono`](Self::is_mono) for any non-empty layout.
    ///
    /// [`EMPTY`](Self::EMPTY) is neither mono nor multi: it has no channels at
    /// all. Both predicates report `false` for it.
    pub const fn is_multi(self) -> bool {
        self.0 > 1
    }

    /// Build a layout from a channel count, in `const` context.
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
/// the ~28 `impl Into<ChannelLayout>` parameters take them straight through,
/// instead of scattering `as u16` across the FFI boundaries where a silent
/// truncating cast would do the most damage.
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

    /// The bug the helper exists to prevent.
    ///
    /// Five call sites tested "is there a second channel?" as
    /// `matches!(l, Stereo | Quad | Multi(_))`. The empty layout matched that
    /// pattern, so an empty bus was treated as having a channel 1 to read.
    /// Both predicates must reject it: an empty layout is neither.
    #[test]
    fn an_empty_layout_is_neither_mono_nor_multi() {
        let empty = ChannelLayout::EMPTY;
        assert_eq!(empty.count(), 0);
        assert!(
            !empty.is_multi(),
            "the empty layout has no second channel, but the old `Multi(_)` pattern said it did"
        );
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
    /// Load-bearing, not cosmetic: `Recorder::start` builds its width-mismatch
    /// error by `Debug`-formatting both layouts, and a test there asserts the
    /// message names them. The derived newtype `Debug` printed
    /// `ChannelLayout(2)` and broke that.
    #[test]
    fn debug_names_the_common_widths() {
        assert_eq!(format!("{:?}", ChannelLayout::EMPTY), "Empty");
        assert_eq!(format!("{:?}", ChannelLayout::MONO), "Mono");
        assert_eq!(format!("{:?}", ChannelLayout::STEREO), "Stereo");
        assert_eq!(format!("{:?}", ChannelLayout::QUAD), "Quad");
        assert_eq!(format!("{:?}", ChannelLayout::from_count(6)), "6 channels");
    }

    /// Ordering is by width. The enum this replaced could not derive `Ord` at
    /// all: it ranked `Quad` below `Multi(2)`, because variant order won over
    /// channel count.
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
