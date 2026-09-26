//! What a stream's ring holds, position by position, and how it changes.
//!
//! A live stream's ring (`prefetch::Ring`) is indexed by **straight
//! position**: file frames counted as if the file played straight on, the
//! position a placed voice's gate and seat give (`interp::Seat`), less the
//! channel's PDC preroll. What the slot for straight position `s` holds is the
//! stream's [`Mapping`]: the file frame a loop places `s` on, its fade blended
//! toward the lead-in (`LoopSpan`'s sequence, the one the memory tier and a
//! forked disk voice read), or the frame a reversed read mirrors it to. The
//! butler writes the mapping ([`Mapping::fill`]); the audio thread reads the
//! four taps a position needs by the same rule the memory tier takes them
//! ([`Arrangement::taps`]). Neither side keeps a loop's state: the mapping is a
//! function of the position.
//!
//! # A change of mapping is a switch at a straight position
//!
//! A loop edit or a direction change leaves the ring holding the old mapping
//! ahead of the reader. [`apply_mapping`] finds the first position the old and
//! new mappings disagree on (`D`). A change that disagrees nowhere in the
//! window is free: later writes use the new mapping and nothing is heard until
//! the reader reaches frames the old one never wrote. Otherwise the ring is
//! rewritten from a switch point `x`: `D` itself when that is far enough ahead
//! of the block the reader is in, so the reader goes from the old sequence to
//! the new one where they part, exactly as the memory tier does; else a guard
//! past that block, and then the switch is a jump the memory tier made at the
//! edit, so the reader crossfades there from a record of what the old mapping
//! would have played ([`FadeRecord`]), at the read rate, consuming the ring.
//! The record also covers the frames the butler has not rewritten yet, so a
//! reader that arrives early plays the old sequence rather than a gap.
//!
//! This replaced a FIFO ring the butler flushed and refilled: see doc 013,
//! "The live disk reposition (after #48)".

use std::sync::Arc;

use super::io::wave_io::WaveIn;
use super::prefetch::RegionOut;
use tutti_core::{ChannelLayout, SampleRate};
use tutti_io::Wave;

use crate::nonempty;
use crate::voice::interp::{split_position, tap_indices};
use crate::voice::loop_span::{blend, LoopSpan};

/// A loop at most this many frames long keeps its body (`[resume, end)`)
/// resident, so a streamed refill does not seek the decoder once per wrap.
const BODY_MAX_FRAMES: usize = 1 << 16;

/// How far past the in-flight block a rewrite may start, in frames: room for
/// the butler to rewrite before the reader gets there.
pub(crate) const GUARD_FRAMES: u64 = 256;

/// Frames of old material a [`FadeRecord`] carries past the fade itself: what
/// the reader plays if it reaches the switch before the rewrite has.
const GAP_COVER_FRAMES: usize = 2048;

/// How the reader turns a position into four straight taps: the parts of a
/// [`Mapping`] the audio thread needs (no sample data).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Arrangement {
    reverse: bool,
    /// The file's length in frames.
    len: usize,
    /// The loop, going forward; `None` unlooped (and always in reverse).
    span: Option<LoopSpan>,
}

impl Arrangement {
    /// An unlooped forward stream of a file `len` frames long. `usize::MAX`
    /// for a length not known (a test's bare ring), which never falls silent.
    pub(crate) fn plain(len: usize) -> Self {
        Self {
            reverse: false,
            len,
            span: None,
        }
    }

    /// The four straight positions whose ring frames the kernel interpolates
    /// `pos` (a straight position) from, and the fraction — the frames the
    /// memory tier reads for the same position (`MemorySource::read_placed_into`),
    /// by the same tap layout: [`tap_indices`] unlooped and mirrored in
    /// reverse, `LoopSpan`'s placement on a loop. `None` where the memory tier
    /// is silent: at and past the file's end, unlooped or reversed.
    ///
    /// On a loop the taps are the straight frames around the position: slot
    /// `s` holds what position `s` places on, so the frames either side of it
    /// are the loop's sequence through any wrap (`LoopSpan::taps`' back and
    /// forward wrap, without its flag). The memory tier interpolates the
    /// *placed* position, and that is the same fraction and the same whole
    /// frame: placement is exact in `f64`. `pos - end` is a multiple of `pos`'s
    /// ulp no larger than `pos`, so representable; `rem_euclid` is exact; and
    /// `resume + r` is again such a multiple, no larger than `end ≤ pos`.
    #[inline]
    pub(crate) fn taps(&self, pos: f64) -> Option<([u64; 4], f32)> {
        let len = self.len;
        if len == 0 {
            return None;
        }
        if self.reverse {
            if pos >= len as f64 {
                return None;
            }
            let (taps, frac) = tap_indices(len, (len as f64 - 1.0 - pos).max(0.0));
            return Some((taps.map(|f| (len - 1 - f) as u64), frac));
        }
        match self.span {
            None => {
                if pos >= len as f64 {
                    return None;
                }
                let (taps, frac) = tap_indices(len, pos);
                Some((taps.map(|f| f as u64), frac))
            }
            Some(_) => {
                let (idx, frac) = split_position(pos);
                Some((
                    [idx.saturating_sub(1), idx, idx + 1, idx + 2].map(|s| s as u64),
                    frac,
                ))
            }
        }
    }
}

/// A switch the reader crosses: the old mapping's frames around it, so the
/// reader can fade from what it would have played to what the ring now holds.
#[derive(Debug)]
pub(crate) struct FadeRecord {
    /// The straight position of `data`'s first frame.
    pub(crate) start: u64,
    /// Frames in `data`.
    pub(crate) frames: usize,
    /// The old mapping's frames for `[start, start + frames)`, flat interleaved
    /// at the ring's width.
    pub(crate) data: Box<[f32]>,
    /// Output frames the fade lasts; 0 cuts (after any gap the record covers).
    pub(crate) fade_frames: usize,
}

/// What the reader needs to read the ring, published by the butler
/// (`RtPublish`, read once per block): the arrangement before and from a
/// switch point, and the fade across it.
#[derive(Debug)]
pub(crate) struct RingMap {
    pub(crate) before: Arrangement,
    pub(crate) after: Arrangement,
    /// The switch: positions from here read `after`.
    pub(crate) at: u64,
    pub(crate) fade: Option<FadeRecord>,
    /// Bumped with every publish, so the reader starts a record's fade once.
    pub(crate) epoch: u64,
}

impl RingMap {
    /// One arrangement everywhere.
    pub(crate) fn plain(arrangement: Arrangement, epoch: u64) -> Self {
        Self {
            before: arrangement,
            after: arrangement,
            at: 0,
            fade: None,
            epoch,
        }
    }

    /// The arrangement position `pos` reads by.
    #[inline]
    pub(crate) fn arrangement(&self, pos: f64) -> Arrangement {
        if pos >= self.at as f64 {
            self.after
        } else {
            self.before
        }
    }
}

/// A stream's loop as its refill writes it: the span, the frames its fade
/// blends toward, and — for a short loop — its body, captured once when the
/// loop is set so no refill re-reads them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RingLoop {
    span: LoopSpan,
    /// The `span.fade()` frames leading into `span.resume()` (the fade's
    /// lead-in, `[resume - fade, resume)`), flat interleaved at the ring's
    /// width. Shared, so a clone (a parallel refill's work item) holds it
    /// without a copy.
    lead_in: Arc<[f32]>,
    /// `[resume, end)`, raw, when the loop is at most [`BODY_MAX_FRAMES`]: a
    /// streamed refill copies each wrap from here instead of seeking the
    /// decoder back once per wrap.
    body: Option<Arc<[f32]>>,
}

/// Why a loop plays hard although a fade was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeadIn {
    /// Read, or not needed (a hard loop).
    Read,
    /// The file could not be read there: the loop plays hard, and says so.
    Unreadable,
}

impl RingLoop {
    /// The loop `[range.0, range.1)` with a `crossfade_frames` fade, on a file
    /// `len` frames long (its end clamped to the file, as every tier clamps
    /// it), writing a ring `channels` wide, reading only the frames it needs
    /// through `read` (`read(frame, out)` fills `out` from file frame `frame`
    /// on, returning whether it could). `None` for a range with nothing in it,
    /// which does not loop.
    ///
    /// The lead-in is read here, `span.fade()` frames, and the body when the
    /// loop is short; nothing else. A lead-in that cannot be read leaves the
    /// loop hard ([`LeadIn::Unreadable`]): its span is rebuilt with no fade,
    /// so the ring is never written with a fade toward silence.
    pub(crate) fn capture(
        range: (u64, u64),
        crossfade_frames: usize,
        len: usize,
        channels: impl Into<ChannelLayout>,
        read: &mut dyn FnMut(usize, &mut [f32]) -> bool,
    ) -> Option<(Self, LeadIn)> {
        let ch = nonempty(channels.into()).count() as usize;
        let (start, end) = (range.0 as usize, range.1 as usize);
        let mut span = LoopSpan::new(start, end, crossfade_frames, len)?;
        let mut lead_in = vec![0.0f32; span.fade() * ch];
        let mut outcome = LeadIn::Read;
        if span.fade() > 0 && !read(span.resume() - span.fade(), &mut lead_in) {
            span = LoopSpan::new(start, end, 0, len)?;
            lead_in.clear();
            outcome = LeadIn::Unreadable;
        }
        let cycle = span.end() - span.resume();
        let body = (cycle <= BODY_MAX_FRAMES)
            .then(|| {
                let mut body = vec![0.0f32; cycle * ch];
                read(span.resume(), &mut body).then(|| Arc::from(body))
            })
            .flatten();
        Some((
            Self {
                span,
                lead_in: lead_in.into(),
                body,
            },
            outcome,
        ))
    }

    /// The span the ring is written by.
    #[cfg(test)]
    pub(crate) fn span(&self) -> &LoopSpan {
        &self.span
    }

    /// Blend the frames of a run that fall in the fade toward their lead-in,
    /// as `interp::read_looped_frame` blends a tap: `run` holds file frames
    /// `from..`, `ch` wide.
    fn blend_run(&self, from: usize, run: &mut [f32], ch: usize) {
        let fade = self.span.fade();
        let fade_start = self.span.end() - fade;
        let frames = run.len() / ch;
        if fade == 0 || from + frames <= fade_start {
            return;
        }
        let first = fade_start.saturating_sub(from);
        for (k, frame) in run.chunks_exact_mut(ch).enumerate().skip(first) {
            let Some((lead, weight)) = self.span.fade_at(from + k) else {
                continue;
            };
            let at = (lead + fade - self.span.resume()) * ch;
            for (s, &l) in frame.iter_mut().zip(&self.lead_in[at..at + ch]) {
                *s = blend(*s, l, weight);
            }
        }
    }

    /// Fill `run` (file frames `at..`, all inside the loop's repeating part)
    /// from the resident body, when there is one.
    fn body_run(&self, at: usize, run: &mut [f32], ch: usize) -> bool {
        let Some(body) = self.body.as_ref() else {
            return false;
        };
        if at < self.span.resume() {
            return false;
        }
        let from = (at - self.span.resume()) * ch;
        run.copy_from_slice(&body[from..from + run.len()]);
        true
    }
}

/// What a stream's straight positions hold: a direction, the file's length,
/// and (going forward) a loop.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Mapping {
    pub(crate) reverse: bool,
    pub(crate) len: usize,
    pub(crate) ring_loop: Option<RingLoop>,
}

/// One slot's content, compared to find where two mappings part.
#[derive(Debug, PartialEq)]
enum Key {
    /// A file frame, and inside a fade its lead-in and weight (as bits).
    Frame(usize, Option<(usize, u32)>),
    /// Nothing is ever read here (past the end of an unlooped or reversed
    /// stream).
    Unread,
}

impl Mapping {
    /// The file straight on, forward, unlooped.
    pub(crate) fn plain(len: usize) -> Self {
        Self {
            reverse: false,
            len,
            ring_loop: None,
        }
    }

    /// The loop, where it applies (forward only).
    fn forward_loop(&self) -> Option<&RingLoop> {
        self.ring_loop.as_ref().filter(|_| !self.reverse)
    }

    /// What the reader needs of this mapping.
    pub(crate) fn arrangement(&self) -> Arrangement {
        Arrangement {
            reverse: self.reverse,
            len: self.len,
            span: self.forward_loop().map(|l| l.span),
        }
    }

    /// One past the last straight position worth writing: the file's length,
    /// or never for a forward loop.
    pub(crate) fn end(&self) -> u64 {
        match self.forward_loop() {
            Some(_) => u64::MAX,
            None => self.len as u64,
        }
    }

    fn key(&self, s: u64) -> Key {
        if let Some(ring_loop) = self.forward_loop() {
            let f = ring_loop.span.place_frame(s as usize);
            return Key::Frame(f, ring_loop.span.fade_at(f).map(|(l, w)| (l, w.to_bits())));
        }
        if s >= self.len as u64 {
            return Key::Unread;
        }
        let s = s as usize;
        Key::Frame(if self.reverse { self.len - 1 - s } else { s }, None)
    }

    /// How many positions from `s` on hold consecutive file frames outside
    /// any fade, and which way they run: `(run, step)`, `step` +1 forward, -1
    /// reversed, 0 past the end (nothing read). A key equal at `s` stays equal
    /// for the shorter of two runs that step alike.
    fn run(&self, s: u64) -> (u64, i8) {
        if let Some(ring_loop) = self.forward_loop() {
            let f = ring_loop.span.place_frame(s as usize);
            let fade_start = ring_loop.span.end() - ring_loop.span.fade();
            return ((fade_start.saturating_sub(f) as u64).max(1), 1);
        }
        if s >= self.len as u64 {
            return (u64::MAX, 0);
        }
        (self.len as u64 - s, if self.reverse { -1 } else { 1 })
    }

    /// The first straight position in `[from, to)` whose slot this mapping
    /// and `other` fill differently: compared a run at a time, so an edit's
    /// search over a 30 s ring costs a few comparisons per wrap and fade frame,
    /// not one per position.
    fn parts_from(&self, other: &Mapping, from: u64, to: u64) -> Option<u64> {
        let mut s = from;
        while s < to {
            if self.key(s) != other.key(s) {
                return Some(s);
            }
            let ((a, sa), (b, sb)) = (self.run(s), other.run(s));
            s = s.saturating_add(if sa == sb { a.min(b) } else { 1 });
        }
        None
    }

    /// Fill `out` (flat interleaved, `ch` wide) with what straight positions
    /// `pos..` hold, reading runs of file frames through `read(frame, run)`:
    /// forward, the file (or on a loop the looped sequence, fade blended as it
    /// lands, and a short loop's wraps from its resident body); reversed, the
    /// file mirrored, frame order reversed within each run and channels kept.
    /// Positions past what [`end`](Self::end) writes are zeroed; nothing reads
    /// them.
    pub(crate) fn fill(
        &self,
        pos: u64,
        out: &mut [f32],
        ch: usize,
        read: &mut dyn FnMut(usize, &mut [f32]),
    ) {
        let frames = out.len() / ch;
        let writable = (self.end().saturating_sub(pos) as usize).min(frames);
        out[writable * ch..].fill(0.0);
        let out = &mut out[..writable * ch];
        if self.reverse {
            // Positions `pos..pos + n` hold file frames `len - 1 - pos` down.
            let first = self.len - pos as usize - writable;
            read(first, out);
            for i in 0..writable / 2 {
                let (a, b) = (i * ch, (writable - 1 - i) * ch);
                for c in 0..ch {
                    out.swap(a + c, b + c);
                }
            }
            return;
        }
        let Some(ring_loop) = self.forward_loop() else {
            read(pos as usize, out);
            return;
        };
        let span = ring_loop.span;
        let mut done = 0;
        while done < writable {
            let at = span.place_frame(pos as usize + done);
            let run = (span.end() - at).min(writable - done);
            let slice = &mut out[done * ch..(done + run) * ch];
            if !ring_loop.body_run(at, slice, ch) {
                read(at, slice);
            }
            ring_loop.blend_run(at, slice, ch);
            done += run;
        }
    }
}

/// A writer's content: `before` for positions below `switch_at`, `current`
/// from it on (and for every later write).
#[derive(Clone, Debug)]
pub(crate) struct Content {
    pub(crate) before: Mapping,
    pub(crate) current: Mapping,
    pub(crate) switch_at: u64,
    /// The epoch of the last [`RingMap`] published.
    pub(crate) epoch: u64,
}

impl Content {
    /// One mapping everywhere.
    pub(crate) fn new(mapping: Mapping) -> Self {
        Self {
            before: mapping.clone(),
            current: mapping,
            switch_at: 0,
            epoch: 0,
        }
    }

    /// The mapping position `s` holds.
    fn at(&self, s: u64) -> &Mapping {
        if s < self.switch_at {
            &self.before
        } else {
            &self.current
        }
    }

    /// The first position in `[from, to)` whose slot differs from `new`'s.
    fn parts_from(&self, new: &Mapping, from: u64, to: u64) -> Option<u64> {
        let split = self.switch_at.clamp(from, to);
        self.before
            .parts_from(new, from, split)
            .or_else(|| self.current.parts_from(new, split, to))
    }
}

/// What a mapping change did (for tests and logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Edit {
    /// The new mapping is the one in place: nothing happened.
    Same,
    /// The two agree on every written slot: later writes use the new one.
    Free,
    /// The ring is rewritten from `at`; `fade` when the reader crossfades there.
    Switch { at: u64, fade: bool },
}

/// Change `writer`'s mapping to `new` (see the module docs), fading over
/// `fade_frames` output frames at read rate `rate` where the switch is a jump.
///
/// Butler thread; reads the file only for a fade's record.
pub(crate) fn apply_mapping(
    writer: &mut RegionOut,
    new: Mapping,
    fade_frames: usize,
    rate: f64,
) -> Edit {
    let content = writer.content().clone();
    if content.switch_at == 0 && content.current == new {
        return Edit::Same;
    }
    let ring = std::sync::Arc::clone(writer.ring());
    let play = writer.play();
    let (from, to) = writer.window();
    let lo = from.max(play.saturating_sub(4));
    let epoch = content.epoch + 1;
    // Behind the reader, the ring keeps the old mapping; a jump back there
    // (a transport cycle) must not play it. Drop what differs, so such a jump
    // finds the window without it and moves the window instead.
    if from < lo && content.parts_from(&new, from, lo).is_some() {
        writer.raise_from(lo);
    }
    let parts = if lo < to {
        content.parts_from(&new, lo, to)
    } else {
        None
    };
    let Some(parts) = parts else {
        ring.publish_map(RingMap::plain(new.arrangement(), epoch));
        let mut next = Content::new(new);
        next.epoch = epoch;
        writer.set_content(next);
        return Edit::Free;
    };
    // The switch: where they part, or past the block in flight. The record's
    // disk read takes time, during which the reader moves on: take where it
    // is again afterwards, and choose again if it has reached the switch.
    // Not covered by a test: the live tests step the reader and the butler by
    // hand, so the reader never moves during the butler's read; a reader that
    // did would still be caught by the record's fade (it crosses the switch
    // with the old side in hand), only at a later `at`.
    let mut busy = writer.in_flight_end();
    let mut tries = 0;
    let (at, fade, record) = loop {
        tries += 1;
        let mut at = parts.max(busy + GUARD_FRAMES);
        if content.switch_at > busy {
            at = at.min(content.switch_at);
        }
        let at = at.min(to).max(busy.min(to)).max(lo);
        let fade = at > parts;
        let record = fade.then(|| {
            let ch = writer.channels().count() as usize;
            let start = at.saturating_sub(3);
            let frames =
                (fade_frames as f64 * rate.max(1.0)).ceil() as usize + GAP_COVER_FRAMES + 8;
            let mut data = vec![0.0f32; frames * ch];
            // The old content piecewise, as the reader would have heard it.
            let split = (content.switch_at.clamp(start, start + frames as u64) - start) as usize;
            let (a, b) = data.split_at_mut(split * ch);
            writer.fill_with(&content.before, start, a);
            writer.fill_with(&content.current, start + split as u64, b);
            FadeRecord {
                start,
                frames,
                data: data.into(),
                fade_frames,
            }
        });
        let now = writer.in_flight_end();
        // Bounded: a reader that keeps pace with the reads is past the ring's
        // end soon anyway, and the fade covers the rest.
        if now + 2 < at || now >= to || tries == 4 {
            break (at, fade, record);
        }
        busy = now;
    };
    let old = content.at(at.saturating_sub(1)).clone();
    ring.publish_map(RingMap {
        before: old.arrangement(),
        after: new.arrangement(),
        at,
        fade: record,
        epoch,
    });
    writer.retract_to(at);
    writer.set_content(Content {
        before: old,
        current: new,
        switch_at: at,
        epoch,
    });
    Edit::Switch { at, fade }
}

/// Read file frames `at..` of a resident `wave` into `out` (`channels` wide),
/// zero past its end: the whole-file source a region without a decoder reads.
pub(crate) fn read_wave(wave: &Wave, at: usize, out: &mut [f32], channels: ChannelLayout) {
    WaveIn::new(wave, at, channels).fill_interleaved(out);
}

/// Ring capacity in **frames** for a file of `file_length_samples` frames at
/// `sample_rate`.
///
/// Buys buffering depth in seconds of audio, tapering as the file grows: a small
/// file is held whole up to a 30 s cap, then 10 s, 5 s and 3 s as the estimated
/// size crosses 50 MB, 200 MB and 500 MB. Floored at 4096 frames, which is what
/// keeps a very short file from producing a ring too small to absorb one block.
///
/// The size estimate assumes stereo `f32`. At a wider width it under-estimates,
/// which only picks a slightly more generous buffer — the heuristic chooses a
/// capacity, never a correctness boundary.
pub(crate) fn buffer_size_for_file(
    file_length_samples: u64,
    sample_rate: impl Into<SampleRate>,
) -> usize {
    let sample_rate = sample_rate.into().get();
    // Rough byte estimate for the buffer-size heuristic. Assumes stereo f32;
    // at a wider width it under-estimates, which only makes the chosen buffer
    // slightly generous — it never affects correctness.
    let file_size_bytes = file_length_samples * 2 * 4;
    let file_size_mb = file_size_bytes as f64 / (1024.0 * 1024.0);

    let buffer_seconds = if file_size_mb < 50.0 {
        (file_length_samples as f64 / sample_rate).min(30.0)
    } else if file_size_mb < 200.0 {
        10.0
    } else if file_size_mb < 500.0 {
        5.0
    } else {
        3.0
    };

    let buffer_capacity = (buffer_seconds * sample_rate) as usize;
    buffer_capacity.max(4096)
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_wave(samples: &[(f32, f32)]) -> Wave {
        let mut wave = Wave::new(2, 48000.0);
        for (l, r) in samples {
            wave.push_frame(&[*l, *r]);
        }
        wave
    }

    fn make_mono_wave(samples: &[f32]) -> Wave {
        let mut wave = Wave::new(1, 48000.0);
        for s in samples {
            wave.push_frame(&[*s]);
        }
        wave
    }

    /// A reader over `wave`, at `ch` wide, as `RingLoop::capture` and
    /// `Mapping::fill` take one; counts its reads.
    fn reader<'w>(
        wave: &'w Wave,
        ch: usize,
        reads: &'w std::cell::Cell<usize>,
    ) -> impl FnMut(usize, &mut [f32]) -> bool + 'w {
        move |at, out| {
            reads.set(reads.get() + 1);
            read_wave(wave, at, out, ChannelLayout::from(ch));
            true
        }
    }

    fn ring_loop(wave: &Wave, range: (u64, u64), fade: usize) -> RingLoop {
        let reads = std::cell::Cell::new(0);
        let mut read = reader(wave, 1, &reads);
        let captured = RingLoop::capture(range, fade, wave.len(), 1usize, &mut read);
        captured.expect("a loop").0
    }

    fn fill(mapping: &Mapping, wave: &Wave, pos: u64, frames: usize, ch: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; frames * ch];
        let reads = std::cell::Cell::new(0);
        let mut read = reader(wave, ch, &reads);
        mapping.fill(pos, &mut out, ch, &mut |at, run| {
            read(at, run);
        });
        out
    }

    fn looped(wave: &Wave, range: (u64, u64), fade: usize) -> Mapping {
        Mapping {
            reverse: false,
            len: wave.len(),
            ring_loop: Some(ring_loop(wave, range, fade)),
        }
    }

    /// **The loop's lead-in is what leads into where the wrap resumes**:
    /// `[start - fade, start)` when there is room before the start, else the
    /// loop's own head `[start, start + fade)` (the wrap then resuming after
    /// it), the fade at most half the loop there — `LoopSpan`'s rule, which
    /// every tier reads by. A lead-in that cannot be read leaves the loop
    /// hard and says so, rather than fading toward silence.
    ///
    /// Mutation (run): the lead-in read from `resume` (the old head replay) →
    /// `[5.0, 6.0]` → fails. Mutation (run): the head mode removed from
    /// `LoopSpan::new` → a 2-frame fade before frame 2 → fails. Mutation
    /// (run): `capture` keeping the fade when the read fails → fails.
    #[test]
    fn a_loop_fades_in_from_what_leads_into_its_resume() {
        let wave = make_mono_wave(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        let lead_in = |range, fade| ring_loop(&wave, range, fade).lead_in.to_vec();
        assert_eq!(lead_in((5, 9), 2), [3.0, 4.0]);
        assert_eq!(ring_loop(&wave, (5, 7), 4).span().fade(), 2);
        assert_eq!(ring_loop(&wave, (2, 9), 4).span().fade(), 3);
        assert_eq!(lead_in((2, 9), 4), [2.0, 3.0, 4.0]);
        assert_eq!(ring_loop(&wave, (0, 9), 4).span().fade(), 4);
        assert_eq!(lead_in((0, 9), 4), [0.0, 1.0, 2.0, 3.0]);
        assert!(lead_in((0, 9), 0).is_empty());
        let (blind, outcome) =
            RingLoop::capture((5, 9), 2, 10, 1usize, &mut |_, _| false).expect("a loop");
        assert_eq!(
            (blind.span().fade(), blind.lead_in.len(), outcome),
            (0, 0, LeadIn::Unreadable)
        );
    }

    /// **A fill writes the loop as the sequence `LoopSpan` defines**: from
    /// frame 0, a crossfaded loop `[5, 9)` writes the file up to 5, blends
    /// 7 and 8 toward 3 and 4 as it writes them, and wraps to 5; a fill that
    /// starts past the loop's end (a straight position) places it on the loop
    /// first; and a loop whose fade goes into its head resumes after it. A
    /// short loop's wraps come from its resident body: the file is read only
    /// for the first pass.
    ///
    /// Mutation (run): the blend dropped from `Mapping::fill` → frame 7 reads
    /// 7.0 → fails. Mutation (run): `place_frame` wrapping to `start` in head
    /// mode → fails. Mutation (run): the run not cut at the loop's end → 9.0
    /// after 8 → fails. Mutation (run): `body_run` never used → the read count
    /// → fails.
    #[test]
    fn a_fill_writes_the_looped_sequence() {
        let wave = make_mono_wave(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        let lead = looped(&wave, (5, 9), 2);
        let blended = |tail: f32, lead: f32, k: f32| tail * (1.0 - k / 3.0) + lead * (k / 3.0);
        let (b7, b8) = (blended(7.0, 3.0, 1.0), blended(8.0, 4.0, 2.0));
        assert_eq!(
            fill(&lead, &wave, 0, 13, 1),
            [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, b7, b8, 5.0, 6.0, b7, b8]
        );
        // Straight 11 is the loop's third frame, 7, on its second pass.
        assert_eq!(fill(&lead, &wave, 11, 4, 1), [b7, b8, 5.0, 6.0]);

        // Head mode: `[1, 9)` fading 3 into its own head resumes at 4.
        let head = looped(&wave, (1, 9), 3);
        let b = |tail: f32, lead: f32, k: f32| tail * (1.0 - k / 4.0) + lead * (k / 4.0);
        assert_eq!(
            fill(&head, &wave, 5, 8, 1),
            [
                5.0,
                b(6.0, 1.0, 1.0),
                b(7.0, 2.0, 2.0),
                b(8.0, 3.0, 3.0),
                4.0,
                5.0,
                b(6.0, 1.0, 1.0),
                b(7.0, 2.0, 2.0)
            ]
        );

        // Many wraps of a short loop read the file once: the first pass.
        let reads = std::cell::Cell::new(0);
        let mut out = vec![0.0f32; 400];
        let mut read = reader(&wave, 1, &reads);
        lead.fill(0, &mut out, 1, &mut |at, run| {
            read(at, run);
        });
        assert_eq!(reads.get(), 1, "one read, for frames 0..9");
    }

    /// **A six-channel loop wraps at its full FRAME length**: frame 100 of a
    /// 100-frame loop is its frame 0, frame 150 its frame 50 — the failure
    /// `CLAUDE.md` names for this boundary ("a 6-channel looped clip wraps at a
    /// sixth of its length").
    ///
    /// Mutation (run): the fill's frame count taken in samples (`out.len()`
    /// rather than `out.len() / ch`) → fails.
    #[test]
    fn a_six_channel_loop_wraps_at_its_frame_length() {
        const CH: usize = 6;
        let mut wave = Wave::zero(CH, 48_000.0, 100.0 / 48_000.0);
        for i in 0..100 {
            for c in 0..CH {
                wave.set(c, i, (i * CH + c) as f32 / 1000.0);
            }
        }
        let reads = std::cell::Cell::new(0);
        let mut read = reader(&wave, CH, &reads);
        let captured = RingLoop::capture((0, 100), 0, 100, CH, &mut read);
        let mapping = Mapping {
            reverse: false,
            len: 100,
            ring_loop: Some(captured.expect("a loop").0),
        };
        let out = fill(&mapping, &wave, 0, 151, CH);
        for (k, want) in [(99, 99), (100, 0), (150, 50)] {
            for c in 0..CH {
                assert_eq!(
                    out[k * CH + c],
                    (want * CH + c) as f32 / 1000.0,
                    "frame {k}"
                );
            }
        }
    }

    /// **A reversed mapping holds the file mirrored**: straight position `s`
    /// holds file frame `len - 1 - s`, every channel in its place (a flat
    /// reverse would swap each frame's channels), and nothing past the file.
    ///
    /// Mutation (run): the frames reversed as a flat slice → channels swap →
    /// fails. Mutation (run): the run read from `len - pos` → off by one →
    /// fails.
    #[test]
    fn a_reversed_mapping_holds_the_file_mirrored() {
        let wave = make_test_wave(&[(1.0, -1.0), (2.0, -2.0), (3.0, -3.0), (4.0, -4.0)]);
        let mapping = Mapping {
            reverse: true,
            ..Mapping::plain(4)
        };
        assert_eq!(
            fill(&mapping, &wave, 1, 5, 2),
            [3.0, -3.0, 2.0, -2.0, 1.0, -1.0, 0.0, 0.0, 0.0, 0.0]
        );
    }

    /// **A position's taps read what the memory tier reads there, bit for
    /// bit**: slots filled by each mapping (a crossfaded loop, one fading into
    /// its head, a hard one, unlooped and reversed), read at the four straight
    /// taps `Arrangement::taps` names through the one kernel, equal the memory
    /// tier's read at the same position (`read_looped_frame` on the placed
    /// position, `read_frame`, the reverse mirror) — at fractional positions
    /// on a non-dyadic grid, through the fade, across many wraps, and past the
    /// file's end (silent). This is the property the live tier's bit-identity
    /// rests on.
    ///
    /// Mutation (run): the looped taps one frame late (`idx .. idx + 3`) →
    /// fails. Mutation (run): the reverse taps not mirrored → fails. (Taking
    /// the fraction from the placed position instead is no mutation at all:
    /// placement is exact, `Arrangement::taps`.)
    #[test]
    fn a_positions_taps_read_what_the_memory_tier_reads() {
        use crate::voice::interp::{interpolate_taps, read_frame, read_looped_frame};
        let len = 3_000usize;
        let mut w = Wave::new(1, 48_000.0);
        for i in 0..len {
            w.push_frame(&[((i as f64 * 0.013).sin() + i as f64 * 1e-4) as f32]);
        }
        let wave = Arc::new(w);
        let mappings = [
            looped(&wave, (1_000, 2_001), 300),
            looped(&wave, (0, 1_201), 300),
            looped(&wave, (500, 900), 0),
            Mapping::plain(len),
            Mapping {
                reverse: true,
                ..Mapping::plain(len)
            },
        ];
        let span_of = |m: &Mapping| m.ring_loop.as_ref().map(|l| *l.span());
        for mapping in &mappings {
            let slots = fill(mapping, &wave, 0, 12_000, 1);
            let arrangement = mapping.arrangement();
            for k in 0..9_000u32 {
                let pos = k as f64 * 1.318_359_375 * 0.91875;
                let mut memory = [0.0f32];
                if mapping.reverse {
                    if pos < len as f64 {
                        read_frame(&wave, (len as f64 - 1.0 - pos).max(0.0), &mut memory);
                    }
                } else if let Some(span) = span_of(mapping) {
                    let (p, looped) = span.place(pos);
                    read_looped_frame(&wave, &span, p, looped, &mut memory);
                } else if pos < len as f64 {
                    read_frame(&wave, pos, &mut memory);
                }
                let mut live = [0.0f32];
                if let Some((taps, frac)) = arrangement.taps(pos) {
                    interpolate_taps(1, frac, &mut live, |_, t| slots[taps[t] as usize]);
                }
                assert_eq!(
                    live[0].to_bits(),
                    memory[0].to_bits(),
                    "{mapping:?} at {pos}: live {} memory {}",
                    live[0],
                    memory[0]
                );
            }
        }
    }

    /// **Two mappings part where their slots first differ**: the same loop
    /// nowhere, a longer loop at the old end, a reversal at once, and a
    /// content split at its switch is compared piecewise.
    ///
    /// Mutation (run): the fade left out of the key → a fade change parts
    /// nowhere → fails.
    #[test]
    fn mappings_part_where_their_slots_first_differ() {
        let wave = make_mono_wave(&(0..100).map(|i| i as f32).collect::<Vec<_>>());
        let a = looped(&wave, (20, 60), 0);
        assert_eq!(a.parts_from(&looped(&wave, (20, 60), 0), 0, 1_000), None);
        assert_eq!(
            a.parts_from(&looped(&wave, (20, 70), 0), 0, 1_000),
            Some(60)
        );
        assert_eq!(
            a.parts_from(&looped(&wave, (20, 60), 8), 0, 1_000),
            Some(52)
        );
        let reversed = Mapping {
            reverse: true,
            ..Mapping::plain(100)
        };
        assert_eq!(Mapping::plain(100).parts_from(&reversed, 10, 90), Some(10));
        let content = Content {
            before: Mapping::plain(100),
            current: a.clone(),
            switch_at: 70,
            epoch: 0,
        };
        // Below 70 it is the file; from 70 the loop, which `a` matches.
        assert_eq!(content.parts_from(&a, 0, 1_000), Some(60));
        assert_eq!(content.parts_from(&a, 65, 1_000), Some(65));
        assert_eq!(content.parts_from(&a, 70, 1_000), None);
    }

    #[test]
    fn test_buffer_size_small_file() {
        // Small file: 1 second at 48kHz = 48000 samples
        // file_size_bytes = 48000 * 2 * 4 = 384000 bytes = 0.37 MB
        // buffer_seconds = min(1.0, 30.0) = 1.0
        let size = buffer_size_for_file(48000, 48000.0);
        assert_eq!(size, 48000); // 1 second buffer
    }

    #[test]
    fn test_buffer_size_medium_file() {
        // Medium file: 100MB = 100 * 1024 * 1024 bytes
        // file_size_bytes = file_length * 2 * 4 = file_length * 8
        // For 100MB: file_length = 100 * 1024 * 1024 / 8 = 13,107,200 samples
        let file_length = 100 * 1024 * 1024 / 8;
        let size = buffer_size_for_file(file_length, 48000.0);

        // 100MB is in 50-200MB range, so buffer_seconds = 10.0
        let expected = (10.0 * 48000.0) as usize;
        assert_eq!(size, expected);
    }

    #[test]
    fn test_buffer_size_large_file() {
        // Large file: 300MB
        let file_length = 300 * 1024 * 1024 / 8;
        let size = buffer_size_for_file(file_length, 48000.0);

        // 300MB is in 200-500MB range, so buffer_seconds = 5.0
        let expected = (5.0 * 48000.0) as usize;
        assert_eq!(size, expected);
    }

    #[test]
    fn test_buffer_size_very_large_file() {
        // Very large file: 1GB
        let file_length = 1024 * 1024 * 1024 / 8;
        let size = buffer_size_for_file(file_length, 48000.0);

        // 1GB > 500MB, so buffer_seconds = 3.0
        let expected = (3.0 * 48000.0) as usize;
        assert_eq!(size, expected);
    }

    #[test]
    fn test_buffer_size_minimum() {
        // Tiny file should still have minimum buffer
        let size = buffer_size_for_file(100, 48000.0);
        assert!(size >= 4096, "Buffer should be at least 4096 samples");
    }

    #[test]
    fn test_buffer_size_small_file_capped_at_30s() {
        // File that would need more than 30 seconds should be capped
        // 60 seconds at 48kHz = 2,880,000 samples
        // file_size = 2,880,000 * 8 = 23MB (< 50MB, so uses file duration)
        // But capped at 30 seconds
        let file_length = 60 * 48000; // 60 seconds
        let size = buffer_size_for_file(file_length, 48000.0);

        let expected = (30.0 * 48000.0) as usize; // Capped at 30s
        assert_eq!(size, expected);
    }
}
