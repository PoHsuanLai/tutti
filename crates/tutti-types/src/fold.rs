//! [`Fold`] — the one way a signal of one channel topology becomes another's.

use crate::{ChannelLayout, ChannelTopology, Interleaved, InterleavedMut, M3DB, Speaker};

/// Which end of a fold a [`FoldError`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldEnd {
    /// The topology being read.
    Source,
    /// The topology being written.
    Destination,
}

/// Why two topologies cannot be folded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldError {
    /// The topology has no channels.
    Empty(FoldEnd),
    /// Source channel `channel` (speaker `speaker`) has no place in the
    /// destination and no rule folds it elsewhere.
    Unplaced {
        /// The source channel index.
        channel: usize,
        /// The speaker it feeds.
        speaker: Speaker,
    },
    /// A channel is [`Unknown`](Speaker::Unknown) and the topologies differ, so
    /// nothing can be said about where it goes.
    Unnamed {
        /// Which topology holds it.
        end: FoldEnd,
        /// Its channel index.
        channel: usize,
    },
    /// A speaker appears twice, so which channel feeds it is ambiguous.
    Duplicate {
        /// Which topology holds it.
        end: FoldEnd,
        /// The repeated speaker.
        speaker: Speaker,
    },
    /// Discrete channels meet named ones (or a mix) at different widths. The
    /// only rule is a one to one relabelling of equal widths.
    Discrete {
        /// The source width.
        src: ChannelLayout,
        /// The destination width.
        dst: ChannelLayout,
    },
    /// [`Fold::assuming_smpte`] was given a width the engine has no order for.
    NoSmpteOrder(ChannelLayout),
}

impl core::fmt::Display for FoldError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty(end) => write!(f, "the {end:?} topology has no channels"),
            Self::Unplaced { channel, speaker } => write!(
                f,
                "source channel {channel} ({speaker:?}) has no place in the destination"
            ),
            Self::Unnamed { end, channel } => {
                write!(f, "{end:?} channel {channel} has an unknown position")
            }
            Self::Duplicate { end, speaker } => {
                write!(f, "{end:?} feeds {speaker:?} from two channels")
            }
            Self::Discrete { src, dst } => write!(
                f,
                "cannot fold {src:?} into {dst:?}: discrete channels only relabel at equal width"
            ),
            Self::NoSmpteOrder(layout) => write!(f, "{layout:?} has no SMPTE order"),
        }
    }
}

impl std::error::Error for FoldError {}

/// Frames per chunk of the planar group sum's stack scratch.
const CHUNK: usize = 64;

/// One product term of an output: `gain · (x[a] + x[b] + …)`.
#[derive(Debug, Clone, PartialEq)]
struct Term {
    gain: f32,
    srcs: Box<[u16]>,
}

/// A sum of terms, evaluated left to right as `((t0 + t1) + t2)…`.
#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Zero,
    Copy(u16),
    Sum(Box<[Term]>),
}

/// One output channel.
#[derive(Debug, Clone, PartialEq)]
enum Row {
    One(Expr),
    /// `(a + b) · scale`: the mono mix of a stereo pair.
    Mix {
        a: Expr,
        b: Expr,
        scale: f32,
    },
}

impl Expr {
    /// Value at one frame, `get(c)` reading source channel `c`.
    #[inline]
    fn at(&self, get: &impl Fn(usize) -> f32) -> f32 {
        match self {
            Self::Zero => 0.0,
            Self::Copy(c) => get(usize::from(*c)),
            Self::Sum(terms) => terms
                .iter()
                .map(|t| {
                    t.gain
                        * t.srcs
                            .iter()
                            .map(|&c| get(usize::from(c)))
                            .reduce(|a, b| a + b)
                            .unwrap_or(0.0)
                })
                .reduce(|a, b| a + b)
                .unwrap_or(0.0),
        }
    }

    /// The same values over a block, term by term. A lone-source term runs as
    /// a plain loop over whole slices; a summed group reads per frame.
    fn planar<'a>(&self, src: &impl Fn(usize) -> &'a [f32], out: &mut [f32]) {
        let n = out.len();
        match self {
            Self::Zero => out.fill(0.0),
            Self::Copy(c) => out.copy_from_slice(&src(usize::from(*c))[..n]),
            Self::Sum(terms) => {
                for (k, t) in terms.iter().enumerate() {
                    match &*t.srcs {
                        [c] => {
                            let x = &src(usize::from(*c))[..n];
                            if k == 0 {
                                out.iter_mut().zip(x).for_each(|(o, &x)| *o = t.gain * x);
                            } else {
                                out.iter_mut().zip(x).for_each(|(o, &x)| *o += t.gain * x);
                            }
                        }
                        group => {
                            // The group sum a chunk at a time, in a stack
                            // scratch, so every loop runs over whole slices.
                            let mut sum = [0.0f32; CHUNK];
                            for (n, o) in out.chunks_mut(CHUNK).enumerate() {
                                let at = n * CHUNK..n * CHUNK + o.len();
                                let sum = &mut sum[..o.len()];
                                let mut planes =
                                    group.iter().map(|&c| &src(usize::from(c))[at.clone()]);
                                if let Some(first) = planes.next() {
                                    sum.copy_from_slice(first);
                                }
                                for x in planes {
                                    sum.iter_mut().zip(x).for_each(|(s, &x)| *s += x);
                                }
                                if k == 0 {
                                    o.iter_mut().zip(&*sum).for_each(|(o, &s)| *o = t.gain * s);
                                } else {
                                    o.iter_mut().zip(&*sum).for_each(|(o, &s)| *o += t.gain * s);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Channel `ch` of an interleaved buffer of `width`-wide frames.
fn lane(samples: &[f32], ch: usize, width: usize) -> impl Iterator<Item = &f32> {
    samples.iter().skip(ch).step_by(width)
}

fn lane_mut(samples: &mut [f32], ch: usize, width: usize) -> impl Iterator<Item = &mut f32> {
    samples.iter_mut().skip(ch).step_by(width)
}

impl Expr {
    /// Output lane `ch` of `dst` over a block of interleaved frames, term by
    /// term like [`planar`](Self::planar): the same bits as [`at`](Self::at)
    /// per frame without dispatching on the expression for every frame.
    fn interleaved(&self, src: &[f32], sw: usize, dst: &mut [f32], dw: usize, ch: usize) {
        match self {
            Self::Zero => lane_mut(dst, ch, dw).for_each(|o| *o = 0.0),
            Self::Copy(c) => lane_mut(dst, ch, dw)
                .zip(lane(src, usize::from(*c), sw))
                .for_each(|(o, &x)| *o = x),
            Self::Sum(terms) => {
                for (k, t) in terms.iter().enumerate() {
                    let frames = src.chunks_exact(sw);
                    match &*t.srcs {
                        [c] if k == 0 => lane_mut(dst, ch, dw)
                            .zip(lane(src, usize::from(*c), sw))
                            .for_each(|(o, &x)| *o = t.gain * x),
                        [c] => lane_mut(dst, ch, dw)
                            .zip(lane(src, usize::from(*c), sw))
                            .for_each(|(o, &x)| *o += t.gain * x),
                        group => lane_mut(dst, ch, dw).zip(frames).for_each(|(o, fr)| {
                            let v = t.gain
                                * group
                                    .iter()
                                    .map(|&c| fr[usize::from(c)])
                                    .reduce(|a, b| a + b)
                                    .unwrap_or(0.0);
                            *o = if k == 0 { v } else { *o + v };
                        }),
                    }
                }
            }
        }
    }
}

impl Row {
    #[inline]
    fn at(&self, get: &impl Fn(usize) -> f32) -> f32 {
        match self {
            Self::One(e) => e.at(get),
            Self::Mix { a, b, scale } => (a.at(get) + b.at(get)) * scale,
        }
    }

    fn interleaved(&self, src: &[f32], sw: usize, dst: &mut [f32], dw: usize, ch: usize) {
        match self {
            Self::One(e) => e.interleaved(src, sw, dst, dw, ch),
            Self::Mix { .. } => lane_mut(dst, ch, dw)
                .zip(src.chunks_exact(sw))
                .for_each(|(o, fr)| *o = self.at(&|c| fr[c])),
        }
    }
}

/// How a signal of one [`ChannelTopology`] becomes another's.
///
/// Built on the control side by [`between`](Self::between) (it allocates) and
/// applied by [`frame`](Self::frame), [`interleaved`](Self::interleaved),
/// [`planar_channel`](Self::planar_channel) and
/// [`planar_to_interleaved`](Self::planar_to_interleaved), none of which
/// allocate, so they are safe on the audio thread.
///
/// An edge (a file, a device, a plugin bus) states the [`ChannelTopology`] it
/// has; the engine's graph keeps its own order. [`Fold::between`] is built once
/// on the control side from two topologies and then applied to blocks without
/// allocating. It places channels by *speaker*, never by width: a width alone
/// is not a placement (see [`ChannelTopology`]).
///
/// # The rules
///
/// - **Same speakers, any order:** a pure permutation. Gain exactly 1.0, the
///   samples copied bit for bit.
/// - **A speaker in both:** it goes straight across at gain 1.0.
/// - **A speaker the destination lacks** folds into the nearest ones the
///   destination has, at −3 dB ([`M3DB`]), by the table in `fallbacks`:
///   the centre into front left and right, a side or rear speaker into its
///   counterpart (side for rear and back) or else the front speaker on its
///   side, an overhead speaker into the front speaker on its side. The
///   LFE is omitted unless the destination has one. This is the ITU-R BS.775 /
///   Dolby consumer downmix for a stereo destination, the coefficients
///   [`fold_frame`](crate::fold_frame) always used. For any other destination
///   (5.1 to quad, 7.1.4 to 7.1) the same −3 dB table applies and the
///   coefficients are this crate's own: no standard fixes them.
///   A side or rear speaker whose counterpart the *source* lacks and the
///   destination has is moved there at gain 1.0 (a pure reposition, so 5.1 to
///   quad keeps its surrounds at full level).
/// - **A lone centre channel** (a mono source) feeds the destination's centre
///   if it has one, else its front left and right, each at gain 1.0.
/// - **A destination with one channel** is a mono mix, whatever it is called:
///   a one-channel source is copied, front left and right alone are averaged
///   (`(L + R) / 2`), and anything else is the stereo fold times −3 dB. The
///   two rules do not meet (`[0.1, 0.2]` gives `0.15`, `[0.1, 0.2, 0.0]` gives
///   `0.212`), as before.
/// - **Upmix zero-fills.** A destination speaker nothing feeds is silent;
///   nothing is invented.
/// - **Discrete channels** map one to one by index and never pass through a
///   matrix. Discrete to discrete is a copy (zero-filled when wider). Discrete
///   against named is a relabelling, allowed only when the widths are equal;
///   different widths is [`FoldError::Discrete`]. B-format is not speaker feeds
///   and has no [`Speaker`]: an edge that has it declares it discrete, never
///   quad.
/// - **Not silent.** A source channel the destination has no place for is
///   [`FoldError::Unplaced`]; an [`Unknown`](Speaker::Unknown) speaker is
///   [`FoldError::Unnamed`] (unless the two topologies are identical, which is
///   an identity), and a speaker listed twice is [`FoldError::Duplicate`].
///
/// The matrix is **not normalised**: full scale on every 5.1 channel gives
/// `1 + 2·`[`M3DB`]` = 2.414` per side. Headroom is the caller's.
///
/// # Bit-identity with the width folds
///
/// For the topologies [`ChannelTopology::smpte`] (and [`quad`](ChannelTopology::quad))
/// gives, a fold to one or two channels reproduces
/// [`fold_frame`](crate::fold_frame) to the bit, in their
/// order of operations: the direct channel, then each folded channel in source
/// order, `acc + gain·x`. With overhead speakers in the source (7.1.4) the −3 dB
/// channels of a side are summed first and scaled once, as the 12-wide fold
/// always did. Rounding differs between the two forms, which is why both exist.
///
/// # Examples
///
/// ```
/// use tutti_types::{ChannelLayout, ChannelTopology, Fold, M3DB};
///
/// let five_one = ChannelTopology::smpte(ChannelLayout::from(6u16)).unwrap();
/// let stereo = ChannelTopology::smpte(ChannelLayout::STEREO).unwrap();
/// let fold = Fold::between(&five_one, &stereo).unwrap();
///
/// // Only the centre active: both sides get it at −3 dB.
/// let mut out = [0.0f32; 2];
/// fold.frame(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], &mut out);
/// assert_eq!(out, [M3DB, M3DB]);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Fold {
    src: u16,
    rows: Box<[Row]>,
}

/// One source channel's contribution to one destination channel.
struct Hit {
    src: usize,
    dst: usize,
    gain: f32,
    /// The same speaker on both sides (or a lone mono fan-out), not a fold.
    direct: bool,
}

/// Where a speaker the destination lacks folds, in order of preference: the
/// first list whose speakers the destination all has. Every entry is at −3 dB
/// except a pure reposition (see [`Fold`]); the stereo destination uses only
/// the front pair, which is the ITU-R BS.775 / Dolby consumer downmix.
const fn fallbacks(missing: Speaker) -> &'static [&'static [Speaker]] {
    use Speaker::*;
    match missing {
        FrontCenter | TopCenter | TopFrontCenter | TopBackCenter => &[&[FrontLeft, FrontRight]],
        FrontLeftOfCenter | TopFrontLeft | TopBackLeft | TopSideLeft => &[&[FrontLeft]],
        FrontRightOfCenter | TopFrontRight | TopBackRight | TopSideRight => &[&[FrontRight]],
        SideLeft => &[&[BackLeft], &[FrontLeft]],
        SideRight => &[&[BackRight], &[FrontRight]],
        BackLeft => &[&[SideLeft], &[FrontLeft]],
        BackRight => &[&[SideRight], &[FrontRight]],
        BackCenter => &[
            &[BackLeft, BackRight],
            &[SideLeft, SideRight],
            &[FrontLeft, FrontRight],
        ],
        FrontLeft | FrontRight | LowFrequency | Unknown(_) | Discrete => &[],
    }
}

/// The surround speaker `sp` is exchanged with: side for back and back for
/// side.
const fn counterpart(sp: Speaker) -> Option<Speaker> {
    match sp {
        Speaker::SideLeft => Some(Speaker::BackLeft),
        Speaker::SideRight => Some(Speaker::BackRight),
        Speaker::BackLeft => Some(Speaker::SideLeft),
        Speaker::BackRight => Some(Speaker::SideRight),
        _ => None,
    }
}

const fn is_overhead(sp: Speaker) -> bool {
    use Speaker::*;
    matches!(
        sp,
        TopCenter
            | TopFrontLeft
            | TopFrontCenter
            | TopFrontRight
            | TopBackLeft
            | TopBackCenter
            | TopBackRight
            | TopSideLeft
            | TopSideRight
    )
}

fn index(of: &[Speaker], sp: Speaker) -> Option<usize> {
    of.iter().position(|&x| x == sp)
}

fn narrow(i: usize) -> u16 {
    u16::try_from(i).expect("a topology's channel count fits u16 by construction")
}

/// Every source channel's hits into `dst`, or the first channel with none.
fn hits(src: &[Speaker], dst: &[Speaker]) -> Result<Vec<Hit>, FoldError> {
    let lone_mono = src == [Speaker::FrontCenter];
    src.iter()
        .enumerate()
        .map(|(i, &sp)| {
            if let Some(j) = index(dst, sp) {
                return Ok(vec![Hit {
                    src: i,
                    dst: j,
                    gain: 1.0,
                    direct: true,
                }]);
            }
            if sp.is_lfe() {
                return Ok(Vec::new());
            }
            let unplaced = FoldError::Unplaced {
                channel: i,
                speaker: sp,
            };
            if lone_mono {
                return index(dst, Speaker::FrontLeft)
                    .zip(index(dst, Speaker::FrontRight))
                    .map(|(l, r)| {
                        [l, r]
                            .map(|j| Hit {
                                src: i,
                                dst: j,
                                gain: 1.0,
                                direct: true,
                            })
                            .into()
                    })
                    .ok_or(unplaced);
            }
            fallbacks(sp)
                .iter()
                .find_map(|targets| {
                    targets
                        .iter()
                        .map(|&t| index(dst, t))
                        .collect::<Option<Vec<_>>>()
                })
                .map(|js| {
                    js.into_iter()
                        .map(|j| {
                            let reposition =
                                counterpart(sp) == Some(dst[j]) && index(src, dst[j]).is_none();
                            Hit {
                                src: i,
                                dst: j,
                                gain: if reposition { 1.0 } else { M3DB },
                                direct: false,
                            }
                        })
                        .collect()
                })
                .ok_or(unplaced)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|v| v.into_iter().flatten().collect())
}

/// The expression of destination channel `j`: the direct channel, then the
/// folded ones in source order; with overhead speakers in the source the −3 dB
/// ones are summed first and scaled once.
fn expr(src: &[Speaker], hits: &[Hit], j: usize) -> Expr {
    let mine = || hits.iter().filter(move |h| h.dst == j);
    let single = |h: &Hit| Term {
        gain: h.gain,
        srcs: Box::new([narrow(h.src)]),
    };
    let folded = || mine().filter(|h| !h.direct);
    let grouped = src.iter().copied().any(is_overhead);
    let (summed, apart): (Vec<&Hit>, Vec<&Hit>) = if grouped {
        folded().partition(|h| h.gain == M3DB)
    } else {
        (Vec::new(), folded().collect())
    };
    let group = (!summed.is_empty()).then(|| Term {
        gain: M3DB,
        srcs: summed.iter().map(|h| narrow(h.src)).collect(),
    });
    let terms: Vec<Term> = mine()
        .filter(|h| h.direct)
        .map(single)
        .chain(apart.into_iter().map(single))
        .chain(group)
        .collect();
    match &*terms {
        [] => Expr::Zero,
        [Term { gain, srcs }] if *gain == 1.0 && srcs.len() == 1 => Expr::Copy(srcs[0]),
        _ => Expr::Sum(terms.into()),
    }
}

fn first_duplicate(of: &[Speaker]) -> Option<Speaker> {
    of.iter()
        .enumerate()
        .find(|&(i, sp)| !sp.is_discrete() && of[..i].contains(sp))
        .map(|(_, &sp)| sp)
}

impl Fold {
    /// Builds the fold from `src` into `dst`.
    ///
    /// Control side: it allocates. The rules are on [`Fold`].
    ///
    /// # Errors
    ///
    /// [`FoldError`] when a channel cannot be placed, never a silent drop.
    pub fn between(src: &ChannelTopology, dst: &ChannelTopology) -> Result<Self, FoldError> {
        let (s, d) = (src.positions(), dst.positions());
        for (end, side) in [(FoldEnd::Source, s), (FoldEnd::Destination, d)] {
            if side.is_empty() {
                return Err(FoldError::Empty(end));
            }
        }
        let rows = if s == d {
            (0..s.len())
                .map(|i| Row::One(Expr::Copy(narrow(i))))
                .collect()
        } else if s.iter().chain(d).any(|p| p.is_discrete()) {
            Self::discrete(src, dst)?
        } else {
            for (end, side) in [(FoldEnd::Source, s), (FoldEnd::Destination, d)] {
                if let Some(channel) = side.iter().position(|p| matches!(p, Speaker::Unknown(_))) {
                    return Err(FoldError::Unnamed { end, channel });
                }
                if let Some(speaker) = first_duplicate(side) {
                    return Err(FoldError::Duplicate { end, speaker });
                }
            }
            if let [_] = d {
                Self::mono(s)?
            } else {
                Self::placed(s, d)?
            }
        };
        Ok(Self {
            src: narrow(s.len()),
            rows,
        })
    }

    /// As [`between`](Self::between) for two bare widths, **assuming** each is
    /// the engine's SMPTE order ([`ChannelTopology::smpte`]).
    ///
    /// The name is the warning: a width is not a placement, and 6 may be six
    /// interface inputs. Use it only where the edge cannot say more. A width
    /// with no SMPTE order (4 is quad or B-format) is
    /// [`FoldError::NoSmpteOrder`].
    ///
    /// # Errors
    ///
    /// As [`between`](Self::between), and for a width with no SMPTE order.
    pub fn assuming_smpte(src: ChannelLayout, dst: ChannelLayout) -> Result<Self, FoldError> {
        let order = |w: ChannelLayout| ChannelTopology::smpte(w).ok_or(FoldError::NoSmpteOrder(w));
        Self::between(&order(src)?, &order(dst)?)
    }

    fn discrete(src: &ChannelTopology, dst: &ChannelTopology) -> Result<Box<[Row]>, FoldError> {
        let (s, d) = (src.positions().len(), dst.positions().len());
        let copy = |i: usize| {
            Row::One(if i < s {
                Expr::Copy(narrow(i))
            } else {
                Expr::Zero
            })
        };
        if src.is_discrete() && dst.is_discrete() {
            if s > d {
                return Err(FoldError::Unplaced {
                    channel: d,
                    speaker: Speaker::Discrete,
                });
            }
        } else if s != d {
            return Err(FoldError::Discrete {
                src: src.layout(),
                dst: dst.layout(),
            });
        }
        Ok((0..d).map(copy).collect())
    }

    fn placed(s: &[Speaker], d: &[Speaker]) -> Result<Box<[Row]>, FoldError> {
        let hits = hits(s, d)?;
        Ok((0..d.len()).map(|j| Row::One(expr(s, &hits, j))).collect())
    }

    fn mono(s: &[Speaker]) -> Result<Box<[Row]>, FoldError> {
        use Speaker::{FrontLeft, FrontRight};
        let row = match (s, index(s, FrontLeft), index(s, FrontRight)) {
            ([_], ..) => Row::One(Expr::Copy(0)),
            ([_, _], Some(l), Some(r)) => Row::Mix {
                a: Expr::Copy(narrow(l)),
                b: Expr::Copy(narrow(r)),
                scale: 0.5,
            },
            _ => {
                let pair = [FrontLeft, FrontRight];
                let hits = hits(s, &pair)?;
                Row::Mix {
                    a: expr(s, &hits, 0),
                    b: expr(s, &hits, 1),
                    scale: M3DB,
                }
            }
        };
        Ok(Box::new([row]))
    }

    /// The source width this fold reads.
    #[must_use]
    pub fn src_layout(&self) -> ChannelLayout {
        ChannelLayout::from_count(self.src)
    }

    /// The destination width this fold writes.
    #[must_use]
    pub fn dst_layout(&self) -> ChannelLayout {
        ChannelLayout::from_count(narrow(self.rows.len()))
    }

    /// Folds one frame: `src` has [`src_layout`](Self::src_layout) samples and
    /// `dst` [`dst_layout`](Self::dst_layout), and every `dst` sample is written.
    ///
    /// Allocation-free. Panics if a slice is the wrong length (checked in debug
    /// builds; a short `src` panics on the read in release).
    #[inline]
    pub fn frame(&self, src: &[f32], dst: &mut [f32]) {
        debug_assert_eq!(src.len(), usize::from(self.src));
        debug_assert_eq!(dst.len(), self.rows.len());
        // The one hot kernel: a flat zip, no per-channel arithmetic on indices.
        self.rows
            .iter()
            .zip(dst)
            .for_each(|(row, o)| *o = row.at(&|c| src[c]));
    }

    /// Folds a block of interleaved frames, one frame in `src` to one in `dst`.
    ///
    /// Allocation-free.
    ///
    /// # Panics
    ///
    /// If either buffer's width is not this fold's, or the frame counts differ.
    pub fn interleaved(&self, src: Interleaved<'_>, mut dst: InterleavedMut<'_>) {
        assert_eq!(src.layout(), self.src_layout(), "source width");
        assert_eq!(dst.layout(), self.dst_layout(), "destination width");
        assert_eq!(src.len(), dst.len(), "frame count");
        let (sw, dw) = (src.stride(), dst.stride());
        let (src, dst) = (src.samples(), dst.samples_mut());
        self.rows
            .iter()
            .enumerate()
            .for_each(|(ch, row)| row.interleaved(src, sw, dst, dw, ch));
    }

    /// Folds planar channels into an interleaved block: `src(c)` is source
    /// channel `c`, at least `dst.len()` frames long, and every sample of `dst`
    /// is written. The graph-to-device shape; a chunk of frames at a time
    /// through a stack scratch, so each channel runs over whole slices.
    ///
    /// Allocation-free.
    ///
    /// # Panics
    ///
    /// If `dst`'s width is not this fold's, or a source channel is shorter than
    /// `dst`.
    pub fn planar_to_interleaved<'a>(
        &self,
        src: impl Fn(usize) -> &'a [f32],
        mut dst: InterleavedMut<'_>,
    ) {
        assert_eq!(dst.layout(), self.dst_layout(), "destination width");
        let dw = dst.stride();
        let frames = dst.len();
        let samples = dst.samples_mut();
        let mut scratch = [0.0f32; CHUNK];
        for start in (0..frames).step_by(CHUNK) {
            let n = CHUNK.min(frames - start);
            let block = &mut samples[start * dw..(start + n) * dw];
            for ch in 0..dw {
                let out = &mut scratch[..n];
                self.planar_channel(|c| &src(c)[start..start + n], ch, out);
                lane_mut(block, ch, dw)
                    .zip(&*out)
                    .for_each(|(o, &x)| *o = x);
            }
        }
    }

    /// Folds planar channels into destination channel `dst_c`, `out.len()`
    /// frames long: `src(c)` is source channel `c`, at least that long. Every
    /// sample is the bit-identical value [`frame`](Self::frame) writes.
    ///
    /// Allocation-free.
    ///
    /// # Panics
    ///
    /// If `dst_c` is not below the destination width, or a source channel the
    /// fold reads is shorter than `out`.
    pub fn planar_channel<'a>(
        &self,
        src: impl Fn(usize) -> &'a [f32],
        dst_c: usize,
        out: &mut [f32],
    ) {
        match &self.rows[dst_c] {
            Row::One(e) => e.planar(&src, out),
            Row::Mix { a, b, scale } => {
                for (i, o) in out.iter_mut().enumerate() {
                    let get = |c: usize| src(c)[i];
                    *o = (a.at(&get) + b.at(&get)) * scale;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
