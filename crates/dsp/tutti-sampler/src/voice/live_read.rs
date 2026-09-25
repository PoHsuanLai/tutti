//! The audio thread's read of a live stream's ring.
//!
//! A live disk voice reads the butler's ring (`butler::prefetch::Ring`) by
//! **position**: its seat gives the straight position each output frame plays
//! (the memory tier's, less the channel's PDC preroll), the ring's published
//! map says which four slots that position's taps are (`Arrangement::taps`, the
//! memory tier's tap layout), and the one kernel interpolates them. So what it
//! plays is what the memory tier plays at the same clock frame, bit for bit,
//! at any read rate — the ring holds the same frames the memory tier indexes.
//!
//! # Jumps: 0 frames, no drift
//!
//! A position that jumps (a seek, a transport loop, a varispeed change, a PDC
//! change) is simply read at its new place. When the ring does not hold it
//! yet, the butler moves its window there on its next cycle (it follows
//! [`Ring::play`]); meanwhile the reader plays on from where it was, out of a
//! copy of the old continuation it takes from the ring at the jump (the
//! scratch), then crossfades to the new position over the ring's fade length,
//! at the read rate, reading the ring as it goes. The new position is the
//! clock's throughout, so nothing lags: once the fade is done the output is the
//! memory tier's again. A loop or direction edit is the same crossfade from a
//! record the butler publishes of what the old mapping would have played
//! (`loops::FadeRecord`).
//!
//! Nothing here allocates, locks or blocks: the scratch is sized at
//! construction, the map is read once per block (`RtPublish`), and the ring's
//! samples are atomics.

use std::sync::Arc;

use crate::butler::{Arrangement, RingMap, RtState, SharedReader, Window};
use crate::MAX_SAMPLER_CHANNELS;

use super::interp::interpolate_taps;
use super::loop_span::blend;

/// Frames of the old continuation the reader copies at a jump: the refill gap
/// plus the fade, at the read rate.
const SCRATCH_FRAMES: usize = 4096;

/// How far a frame's position may sit from the last one's continuation before
/// it counts as a jump. Under a frame: a seat re-anchored by the clock lands on
/// its continuation to within rounding.
const JUMP_FRAMES: f64 = 0.5;

/// Where a fade's old side comes from.
#[derive(Clone, Copy, Debug)]
enum Old {
    /// The reader's own copy, taken at a jump: the old position is the new
    /// one plus `offset`, read by the arrangement it had.
    Scratch {
        offset: f64,
        arrangement: Arrangement,
    },
    /// The butler's record of the old mapping across a switch, at the same
    /// position, read by the map's `before` arrangement.
    Record,
}

/// A crossfade in progress.
#[derive(Clone, Copy, Debug)]
struct Fade {
    old: Old,
    /// Output frames faded so far.
    k: usize,
    /// Output frames the fade lasts.
    frames: usize,
    /// The new side has sounded: until it does, the old side plays alone (the
    /// refill gap).
    started: bool,
}

/// A live voice's reader over its stream's ring.
pub(crate) struct LiveRead {
    ring: SharedReader,
    /// Output width.
    width: usize,
    /// The ring's width (the file's channels).
    src: usize,
    /// The old continuation copied at a jump, flat at `src`.
    scratch: Vec<f32>,
    scratch_start: u64,
    scratch_frames: usize,
    fade: Option<Fade>,
    /// The map epoch whose record's fade this reader has started (or passed).
    applied_epoch: u64,
    /// The straight position of the last frame read, `None` after silence.
    last: Option<f64>,
    /// The highest straight position read (tests observe consumption).
    #[cfg(test)]
    pub(crate) read_to: f64,
}

impl Clone for LiveRead {
    fn clone(&self) -> Self {
        Self {
            ring: Arc::clone(&self.ring),
            width: self.width,
            src: self.src,
            scratch: self.scratch.clone(),
            scratch_start: self.scratch_start,
            scratch_frames: self.scratch_frames,
            fade: self.fade,
            applied_epoch: self.applied_epoch,
            last: self.last,
            #[cfg(test)]
            read_to: self.read_to,
        }
    }
}

impl std::fmt::Debug for LiveRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveRead")
            .field("width", &self.width)
            .field("last", &self.last)
            .field("fade", &self.fade)
            .finish_non_exhaustive()
    }
}

impl LiveRead {
    /// A reader over `ring`, `width` wide (at most [`MAX_SAMPLER_CHANNELS`]).
    pub(crate) fn new(ring: SharedReader, width: usize) -> Self {
        let src = ring.channels().count().max(1) as usize;
        Self {
            ring,
            width,
            src,
            scratch: vec![0.0; SCRATCH_FRAMES * src],
            scratch_start: 0,
            scratch_frames: 0,
            fade: None,
            // A record already published is crossed like any other.
            applied_epoch: u64::MAX,
            last: None,
            #[cfg(test)]
            read_to: 0.0,
        }
    }

    /// The ring.
    pub(crate) fn ring(&self) -> &SharedReader {
        &self.ring
    }

    /// Whether the reader holds no last position and no fade (tests).
    #[cfg(test)]
    pub(crate) fn forgot(&self) -> bool {
        self.last.is_none() && self.fade.is_none()
    }

    /// Forget where the last frame was: the next is an entry, not a jump.
    pub(crate) fn reset(&mut self) {
        self.last = None;
        self.fade = None;
    }

    /// Render a block: frame `i` plays straight position `positions[i]`
    /// (`None`: silence), read at `rate` file frames per output frame, scaled
    /// by `gain`, handed to `emit(i, frame)` (`width` samples). Frames the ring
    /// cannot supply are silent and counted as underruns on `state`.
    pub(crate) fn render(
        &mut self,
        positions: &[Option<f64>],
        rate: f64,
        gain: f32,
        state: &RtState,
        mut emit: impl FnMut(usize, &[f32]),
    ) {
        let w = self.width;
        let silent = [0.0f32; MAX_SAMPLER_CHANNELS];
        let (mut lo, mut hi) = (u64::MAX, 0u64);
        for p in positions.iter().flatten() {
            let f = p.max(0.0).floor() as u64;
            lo = lo.min(f.saturating_sub(3));
            hi = hi.max(f + 5);
        }
        let Some(first) = positions.iter().flatten().next().copied() else {
            for i in 0..positions.len() {
                emit(i, &silent[..w]);
            }
            self.last = None;
            self.fade = None;
            return;
        };
        let jump = self
            .last
            .map(|last| last + rate)
            .filter(|expected| (first - expected).abs() > JUMP_FRAMES);
        let scratch_range = jump.map_or((0, 0), |expected| {
            let a = (expected.max(0.0).floor() as u64).saturating_sub(3);
            (a, a + SCRATCH_FRAMES as u64)
        });
        // A handle of its own, so the map's borrow is not of `self`.
        let ring = Arc::clone(&self.ring);
        let window = ring.claim(first.max(0.0).floor() as u64, [(lo, hi), scratch_range]);
        let map = ring.map();
        if let Some(expected) = jump {
            self.take_scratch(&window, scratch_range.0);
            // Even with nothing copied: an old continuation past the file's
            // end is silence by its arrangement, which covers the gap too.
            self.fade = Some(Fade {
                old: Old::Scratch {
                    offset: expected - first,
                    arrangement: map.arrangement(expected),
                },
                k: 0,
                frames: self.ring.fade_frames(),
                started: false,
            });
        }

        let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
        let mut old = [0.0f32; MAX_SAMPLER_CHANNELS];
        for (i, pos) in positions.iter().enumerate() {
            let Some(pos) = *pos else {
                emit(i, &silent[..w]);
                self.last = None;
                self.fade = None;
                continue;
            };
            self.cross_switch(&map, pos);
            let new = self.read_ring(&window, map.arrangement(pos), pos, &mut frame[..w]);
            let sounded = match self.fade {
                None => new,
                Some(mut fade) => {
                    let had_old = match fade.old {
                        Old::Scratch {
                            offset,
                            arrangement,
                        } => read_frames(
                            &self.scratch,
                            self.scratch_start,
                            self.scratch_frames,
                            self.src,
                            arrangement,
                            pos + offset,
                            &mut old[..w],
                        ),
                        Old::Record => map.fade.as_ref().is_some_and(|r| {
                            read_frames(
                                &r.data,
                                r.start,
                                r.frames,
                                self.src,
                                map.before,
                                pos,
                                &mut old[..w],
                            )
                        }),
                    };
                    fade.started |= new;
                    if fade.started {
                        if had_old && fade.k < fade.frames {
                            let t = (fade.k + 1) as f32 / (fade.frames + 1) as f32;
                            for (n, &o) in frame[..w].iter_mut().zip(&old[..w]) {
                                *n = blend(o, *n, t);
                            }
                            fade.k += 1;
                            self.fade = Some(fade);
                        } else {
                            self.fade = None;
                        }
                        true
                    } else if had_old {
                        frame[..w].copy_from_slice(&old[..w]);
                        self.fade = Some(fade);
                        true
                    } else {
                        self.fade = None;
                        false
                    }
                }
            };
            if !sounded {
                state.report_underrun();
                frame[..w].fill(0.0);
            }
            for s in frame[..w].iter_mut() {
                *s *= gain;
            }
            emit(i, &frame[..w]);
            self.last = Some(pos);
            #[cfg(test)]
            {
                self.read_to = self.read_to.max(pos);
            }
        }
    }

    /// Start a published record's fade when `pos` first reaches its switch
    /// (two frames early: the taps reach two frames ahead).
    fn cross_switch(&mut self, map: &RingMap, pos: f64) {
        let Some(record) = map.fade.as_ref() else {
            return;
        };
        if map.epoch == self.applied_epoch || pos < map.at.saturating_sub(2) as f64 {
            return;
        }
        self.applied_epoch = map.epoch;
        if pos < (record.start + record.frames as u64) as f64 {
            self.fade = Some(Fade {
                old: Old::Record,
                k: 0,
                frames: record.fade_frames,
                started: false,
            });
        }
    }

    /// Copy the old continuation from `start` on while the ring holds it.
    fn take_scratch(&mut self, window: &Window, start: u64) {
        let mut n = 0;
        while n < SCRATCH_FRAMES && window.holds(start + n as u64) {
            for c in 0..self.src {
                self.scratch[n * self.src + c] = self.ring.sample(start + n as u64, c);
            }
            n += 1;
        }
        self.scratch_start = start;
        self.scratch_frames = n;
    }

    /// Read position `pos` from the ring into `out`: `true` when it sounded
    /// or is silent by the arrangement, `false` when the ring lacks a tap.
    #[inline]
    fn read_ring(
        &self,
        window: &Window,
        arrangement: Arrangement,
        pos: f64,
        out: &mut [f32],
    ) -> bool {
        let Some((taps, frac)) = arrangement.taps(pos) else {
            out.fill(0.0);
            return true;
        };
        if !taps.iter().all(|&t| window.holds(t)) {
            return false;
        }
        let ring = &self.ring;
        interpolate_taps(self.src, frac, out, |c, t| ring.sample(taps[t], c));
        true
    }
}

/// Read position `pos` by `arrangement` from `data` (straight positions
/// `start..start + frames`, flat at `src`) into `out`; `false` when a tap
/// lies outside it.
#[inline]
fn read_frames(
    data: &[f32],
    start: u64,
    frames: usize,
    src: usize,
    arrangement: Arrangement,
    pos: f64,
    out: &mut [f32],
) -> bool {
    let Some((taps, frac)) = arrangement.taps(pos) else {
        out.fill(0.0);
        return true;
    };
    let end = start + frames as u64;
    if !taps.iter().all(|&t| t >= start && t < end) {
        return false;
    }
    interpolate_taps(src, frac, out, |c, t| {
        data[(taps[t] - start) as usize * src + c]
    });
    true
}
