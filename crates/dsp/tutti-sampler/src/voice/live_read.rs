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
//! # Discontinuities: 0 frames, no drift, no step
//!
//! A position that jumps (a seek, a transport loop, a varispeed change, a PDC
//! change) is read at its new place; a loop or direction edit switches what
//! the ring holds at a position the butler publishes (`loops::RingMap`). The
//! memory tier cuts at either. The live reader crossfades, over the ring's
//! fade length, from the **continuation of what it was playing**, rendered
//! the moment the discontinuity arrives into a buffer of its own:
//!
//! - at a jump, the old position's continuation, read from the ring while its
//!   window still holds it;
//! - at a switch, the butler's record of what the old mapping would have
//!   played there (`loops::FadeRecord`);
//! - in the middle of a fade, the fade itself — its old side blended with its
//!   new one at its own weights — so fades chain (an edit during an edit, a
//!   jump during an edit, an edit during a jump) without a step.
//!
//! Rendered, not referenced: nothing published later (another edit's record,
//! another jump) can change it, and it keeps the rate it was rendered at. The
//! new side is the clock's position throughout, read from the ring as it
//! goes: when the ring does not hold it yet (the butler moves its window on
//! its next cycle, following `Ring::play`), the old side plays on alone, and
//! the fade starts once the new side sounds. So once a fade is done the output
//! is the memory tier's again. A continuation that runs out, and the ring
//! coming back after a frame it could not supply (an underrun), are ramped
//! over [`RAMP_FRAMES`], never cut.
//!
//! Nothing here allocates, locks or blocks: the buffers are sized at
//! construction, the map is read once per block (`RtPublish`), and the ring's
//! samples are atomics.
//!
//! # The one reader
//!
//! The ring has one `PosReader`, and a voice holds it. A clone of the voice
//! shares it, by a lock no block ever waits on: a block takes it with
//! `try_lock` or renders silence. That is for a host that renders a clone:
//! `Net::commit` hands its backend a clone of every node (the original, in
//! the frontend, never renders), and `a_gain_change_reaches_a_cloned_voice`
//! pins that such a clone plays. Since doc 013's PR 15 no engine runtime is a
//! `Net`, and Phase 5 deletes it; the sharing can go with it. Two copies
//! rendering at once — which no host does — would each read only the blocks
//! the other was not reading. A fork severs itself (`isolate` → [`LiveRead::sever`]) and
//! reads the file instead (`DiskVoice::isolate`), so it never touches the
//! live reader. A block that reads nothing tells the ring so
//! (`PosReader::idle`), so a paused voice holds no refill back.

use std::sync::{Arc, Mutex, TryLockError};

use tutti_core::{PosClaim, PosReader};

use crate::butler::{Arrangement, RingMap, RtState, SharedReader};
use crate::MAX_SAMPLER_CHANNELS;

use super::interp::interpolate_taps;
use super::loop_span::blend;

/// Output frames of continuation rendered at a discontinuity: the refill gap
/// plus the fade, with room to spare.
pub(crate) const OLD_FRAMES: usize = 2_048;

/// Output frames a continuation ramps out over as it runs out, and the ring
/// ramps back in over after an underrun.
pub(crate) const RAMP_FRAMES: usize = 64;

/// How far a frame's position may sit from the last one's continuation before
/// it counts as a jump. Under a frame: a seat re-anchored by the clock lands on
/// its continuation to within rounding.
const JUMP_FRAMES: f64 = 0.5;

/// A crossfade in progress.
#[derive(Clone, Copy, Debug)]
struct Fade {
    /// Output frames faded so far.
    k: usize,
    /// Output frames the fade lasts.
    frames: usize,
    /// The new side has sounded: until it does, the old side plays alone (the
    /// refill gap).
    started: bool,
}

impl Fade {
    /// The new side's weight at fade frame `k`.
    #[inline]
    fn weight(frames: usize, k: usize) -> f32 {
        if k >= frames {
            1.0
        } else {
            (k + 1) as f32 / (frames + 1) as f32
        }
    }
}

/// The reader's mutable state, apart from the ring it reads (so a block can
/// borrow the ring's map while it changes this).
#[derive(Clone, Debug)]
struct State {
    /// Output width.
    width: usize,
    /// The ring's width (the file's channels).
    src: usize,
    /// The continuation rendered at the last discontinuity, output frames of
    /// `width` samples (before gain), and a spare to render the next into.
    old: Vec<f32>,
    spare: Vec<f32>,
    old_len: usize,
    old_k: usize,
    fade: Option<Fade>,
    /// The map epoch whose switch this reader has crossed.
    applied_epoch: u64,
    /// The straight position of the last frame read, `None` after silence.
    last: Option<f64>,
    /// Frames sounded since the last underrun, while ramping back in.
    recover: Option<usize>,
    /// The highest straight position read (tests observe consumption).
    #[cfg(test)]
    read_to: f64,
}

/// A live voice's reader over its stream's ring.
#[derive(Clone, Debug)]
pub(crate) struct LiveRead {
    ring: SharedReader,
    /// The ring's one reader, shared with this voice's clones (see the module
    /// docs); `None` once severed, or for a ring whose reader was taken.
    reader: Option<Arc<Mutex<PosReader>>>,
    st: State,
}

impl LiveRead {
    /// A reader over `ring` through its one `reader` (`None`: silent), `width`
    /// wide (at most [`MAX_SAMPLER_CHANNELS`]).
    pub(crate) fn new(ring: SharedReader, reader: Option<PosReader>, width: usize) -> Self {
        let src = ring.channels().count().max(1) as usize;
        Self {
            ring,
            reader: reader.map(|r| Arc::new(Mutex::new(r))),
            st: State {
                width,
                src,
                old: vec![0.0; OLD_FRAMES * width],
                spare: vec![0.0; OLD_FRAMES * width],
                old_len: 0,
                old_k: 0,
                fade: None,
                // A switch already published is crossed like any other.
                applied_epoch: u64::MAX,
                last: None,
                recover: None,
                #[cfg(test)]
                read_to: 0.0,
            },
        }
    }

    /// The ring.
    pub(crate) fn ring(&self) -> &SharedReader {
        &self.ring
    }

    /// The highest straight position read (tests).
    #[cfg(test)]
    pub(crate) fn read_to(&self) -> f64 {
        self.st.read_to
    }

    /// Whether the reader holds no last position and no fade (tests).
    #[cfg(test)]
    pub(crate) fn forgot(&self) -> bool {
        self.st.last.is_none() && self.st.fade.is_none()
    }

    /// Forget where the last frame was: the next is an entry, not a jump.
    pub(crate) fn reset(&mut self) {
        self.st.last = None;
        self.st.fade = None;
    }

    /// A block that reads nothing (a stopped source): the ring's reader
    /// claims no range, so it holds no write back.
    #[inline]
    pub(crate) fn idle(&mut self) {
        if let Some(reader) = self.reader.as_deref() {
            if let Some(mut reader) = try_take(reader) {
                reader.idle();
            }
        }
    }

    /// Let go of the live reader: this copy (a fork) never touches the live
    /// ring again, and renders silence through it.
    pub(crate) fn sever(&mut self) {
        self.reader = None;
    }

    /// Render a block: frame `i` plays straight position `positions[i]`
    /// (`None`: silence), read at `rate` file frames per output frame, scaled
    /// by `gain`, handed to `emit(i, frame)` (`width` samples). Frames the ring
    /// cannot supply are counted as underruns on `state`.
    pub(crate) fn render(
        &mut self,
        positions: &[Option<f64>],
        rate: f64,
        gain: f32,
        state: &RtState,
        mut emit: impl FnMut(usize, &[f32]),
    ) {
        let ring = &self.ring;
        let st = &mut self.st;
        let w = st.width;
        let silent = [0.0f32; MAX_SAMPLER_CHANNELS];
        let Some(mut reader) = self.reader.as_deref().and_then(try_take) else {
            // Severed, or another copy reading this block: nothing to read
            // (not an underrun).
            for i in 0..positions.len() {
                emit(i, &silent[..w]);
            }
            return;
        };
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
            st.last = None;
            st.fade = None;
            reader.idle();
            return;
        };
        let jump = st
            .last
            .map(|last| last + rate)
            .filter(|expected| (first - expected).abs() > JUMP_FRAMES);
        // The old continuation's positions, for the claim.
        let old_range = jump.map_or((0, 0), |expected| {
            let a = (expected.max(0.0).floor() as u64).saturating_sub(3);
            let e = (expected + rate * OLD_FRAMES as f64).max(0.0).floor() as u64 + 5;
            (a, e)
        });
        let claim = reader.claim(first.max(0.0).floor() as u64, [(lo, hi), old_range]);
        let map = ring.map();
        let src = st.src;
        if let Some(expected) = jump {
            // The old position's continuation, read from the ring while the
            // window still holds it.
            st.continue_from(ring.fade_frames(), |j, out| {
                let pos = expected + rate * j as f64;
                read_ring(&claim, src, map.arrangement(pos), pos, out)
            });
        }

        let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
        for (i, pos) in positions.iter().enumerate() {
            let Some(pos) = *pos else {
                emit(i, &silent[..w]);
                st.last = None;
                st.fade = None;
                continue;
            };
            st.cross_switch(&map, pos, rate);
            let new = read_ring(&claim, src, map.arrangement(pos), pos, &mut frame[..w]);
            let sounded = st.mix(new, &mut frame[..w]);
            if sounded {
                if let Some(r) = st.recover {
                    let g = (r + 1) as f32 / (RAMP_FRAMES + 1) as f32;
                    frame[..w].iter_mut().for_each(|s| *s *= g);
                    st.recover = (r + 1 < RAMP_FRAMES).then_some(r + 1);
                }
            } else {
                state.report_underrun();
                frame[..w].fill(0.0);
                st.recover = Some(0);
            }
            for s in frame[..w].iter_mut() {
                *s *= gain;
            }
            emit(i, &frame[..w]);
            st.last = Some(pos);
            #[cfg(test)]
            {
                st.read_to = st.read_to.max(pos);
            }
        }
    }
}

impl State {
    /// Render the continuation of what is playing now into the old side and
    /// start a fade of `frames` from it: `source(j, out)` is what the
    /// discontinuity replaced, `j` output frames on (`false` where it has
    /// nothing); a fade in progress is blended into it at its own weights.
    fn continue_from(&mut self, frames: usize, mut source: impl FnMut(usize, &mut [f32]) -> bool) {
        let w = self.width;
        let mut s = [0.0f32; MAX_SAMPLER_CHANNELS];
        let mut n = 0;
        while n < OLD_FRAMES && source(n, &mut s[..w]) {
            let out = &mut self.spare[n * w..(n + 1) * w];
            match self.fade {
                Some(fade) if self.old_k + n < self.old_len => {
                    let t = Fade::weight(fade.frames, fade.k + n);
                    let o = &self.old[(self.old_k + n) * w..(self.old_k + n + 1) * w];
                    for ((d, &o), &s) in out.iter_mut().zip(o).zip(&s[..w]) {
                        *d = blend(o, s, t);
                    }
                }
                _ => out.copy_from_slice(&s[..w]),
            }
            n += 1;
        }
        std::mem::swap(&mut self.old, &mut self.spare);
        self.old_len = n;
        self.old_k = 0;
        self.fade = Some(Fade {
            k: 0,
            frames,
            started: false,
        });
    }

    /// Cross a published switch when `pos` first reaches it (two frames early:
    /// the taps reach two frames ahead): the old side becomes the record of
    /// the old mapping from here on, at this rate.
    fn cross_switch(&mut self, map: &RingMap, pos: f64, rate: f64) {
        let Some(record) = map.fade.as_ref() else {
            return;
        };
        if map.epoch == self.applied_epoch || pos < map.at.saturating_sub(2) as f64 {
            return;
        }
        self.applied_epoch = map.epoch;
        if pos >= (record.start + record.frames as u64) as f64 {
            return;
        }
        let (src, before) = (self.src, map.before);
        self.continue_from(record.fade_frames, |j, out| {
            read_frames(
                &record.data,
                record.start,
                record.frames,
                src,
                before,
                pos + rate * j as f64,
                out,
            )
        });
    }

    /// Mix one frame: `frame` holds the ring's read (`new`: it sounded). Plays
    /// the fade in progress, if any, into `frame`; `true` when the frame
    /// sounded.
    fn mix(&mut self, new: bool, frame: &mut [f32]) -> bool {
        let w = self.width;
        let Some(mut fade) = self.fade else {
            return new;
        };
        let left = self.old_len.saturating_sub(self.old_k);
        let old = (left > 0).then(|| &self.old[self.old_k * w..(self.old_k + 1) * w]);
        // Running out ramps: the old side's gain (alone) or weight (in a
        // fade) goes to the new side over its last `RAMP_FRAMES`.
        let out_ramp = (left as f32 / RAMP_FRAMES as f32).min(1.0);
        fade.started |= new;
        let sounded = match (fade.started, old) {
            (true, Some(o)) if fade.k < fade.frames => {
                let t = Fade::weight(fade.frames, fade.k).max(1.0 - out_ramp);
                for (n, &o) in frame.iter_mut().zip(o) {
                    *n = blend(o, *n, t);
                }
                fade.k += 1;
                self.old_k += 1;
                self.fade = Some(fade);
                true
            }
            (true, _) => {
                self.fade = None;
                true
            }
            (false, Some(o)) => {
                for (n, &o) in frame.iter_mut().zip(o) {
                    *n = o * out_ramp;
                }
                self.old_k += 1;
                self.fade = Some(fade);
                true
            }
            (false, None) => {
                self.fade = None;
                false
            }
        };
        sounded
    }
}

/// The live reader, if no other copy of the voice is reading it now. Never
/// waits.
#[inline]
fn try_take(reader: &Mutex<PosReader>) -> Option<std::sync::MutexGuard<'_, PosReader>> {
    match reader.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Read position `pos` from the block's claim of the ring (`src` channels)
/// into `out`: `true` when it sounded or is silent by the arrangement, `false`
/// when the window lacks a tap.
#[inline]
fn read_ring(
    claim: &PosClaim<'_>,
    src: usize,
    arrangement: Arrangement,
    pos: f64,
    out: &mut [f32],
) -> bool {
    let Some((taps, frac)) = arrangement.taps(pos) else {
        out.fill(0.0);
        return true;
    };
    if !taps.iter().all(|&t| claim.holds(t)) {
        return false;
    }
    let frames = taps.map(|t| claim.frame(t));
    interpolate_taps(src, frac, out, |c, t| frames[t].get(c));
    true
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
