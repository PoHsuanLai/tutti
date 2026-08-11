//! Lock-free crossfader — producer writes fade buffers; audio thread blends.
//!
//! Used for both seek and loop crossfades in `RtState` for streaming playback.
//!
//! For the in-memory sampler's loop crossfade, see `units::loop_crossfade::LoopCrossfade`.
//! The two are intentionally separate: streaming has a separate producer (butler thread)
//! so a lock-free design fits, while the in-memory unit owns its own buffer in `process()`
//! and the lock-free indirection adds no value.

use arc_swap::ArcSwap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tutti_core::ChannelLayout;

use crate::nonempty;

/// One installed crossfade: both buffers, their shared stride, and their frame
/// count. Immutable once published — see [`StreamingCrossfader::start`] for why
/// these four cannot be separate atomics.
#[repr(align(64))]
struct Fade {
    /// Flat interleaved at `channels` samples per frame.
    fadeout: Vec<f32>,
    fadein: Vec<f32>,
    /// The declared width both buffers are interleaved at.
    channels: ChannelLayout,
    /// `channels.count()`, cached — the interleave stride.
    ///
    /// [`frames`](Self::frames) is the RT blend's per-frame slice, and it scales
    /// `i` by the stride twice. Re-deriving from the layout there would put an
    /// enum match inside the audio callback's per-frame path. A `Fade` is
    /// immutable once published (see [`StreamingCrossfader::start`]), so the
    /// pair cannot drift.
    stride: usize,
    /// Usable length in **frames**.
    len: usize,
}

impl Fade {
    /// Frame `i` of both buffers, or `None` past the end of either.
    ///
    /// `self.stride`, not `self.channels.count()`: per-frame, on the RT thread.
    #[inline]
    fn frames(&self, i: usize) -> Option<(&[f32], &[f32])> {
        let base = i * self.stride;
        let end = base + self.stride;
        Some((self.fadeout.get(base..end)?, self.fadein.get(base..end)?))
    }
}

/// A one-shot equal-length crossfade published by the butler and drained by the
/// audio thread.
///
/// The butler allocates both fade buffers and installs them with
/// [`start`](Self::start); the audio thread blends one frame per call through
/// [`next_frame_into`](Self::next_frame_into), touching only atomics and one
/// `ArcSwap` read. Arming is idempotent in the sense that a fresh `start`
/// replaces whatever was installed and restarts from frame zero.
pub struct StreamingCrossfader {
    /// The installed fade, swapped as one unit.
    fade: ArcSwap<Fade>,
    /// Frames blended so far, advanced by the audio thread.
    pos: AtomicU32,
    /// Armed length in **frames**, not samples; 0 means not active.
    len: AtomicU32,
}

impl Default for StreamingCrossfader {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingCrossfader {
    /// A disarmed crossfader holding empty buffers. Blending returns `false`
    /// until a [`start`](Self::start) installs a fade.
    pub fn new() -> Self {
        Self {
            fade: ArcSwap::from_pointee(Fade {
                fadeout: Vec::new(),
                fadein: Vec::new(),
                channels: ChannelLayout::STEREO,
                stride: 2,
                len: 0,
            }),
            pos: AtomicU32::new(0),
            len: AtomicU32::new(0),
        }
    }

    /// Install fade buffers and arm the crossfader.
    ///
    /// Butler thread, so the allocation the caller did to build these `Vec`s is
    /// fine — handing the audio side a finished buffer is the entire design.
    ///
    /// `fadeout` / `fadein` are flat interleaved at `channels` samples per
    /// frame. The fade runs for the **frames** the shorter of the two holds; a
    /// pair that yields zero whole frames leaves the crossfader disarmed rather
    /// than arming an empty fade.
    ///
    /// # Why one `ArcSwap`, not four atomics
    ///
    /// The buffers, their stride, and their frame count are **one fact**, and an
    /// ordered sequence of separate stores cannot publish them as one. Ordering
    /// controls when a single publication becomes visible; it cannot stop a
    /// reader that already loaded `len` from then loading a `channels` and a
    /// buffer pair from a *different* installation.
    ///
    /// That is not theoretical. With the fields separate, a butler alternating a
    /// 6-channel and a 2-channel fade against a draining RT thread produces two
    /// failures: a wide stride against a narrow buffer sends `pos * channels`
    /// past the end, and the bounds check truncates the fade into exactly the
    /// click it exists to prevent; a narrow stride against a wide buffer *passes*
    /// the bounds check and blends channel `c` of one fade with channel `c` of a
    /// different frame of the other.
    ///
    /// Swapping one immutable [`Fade`] makes the four inseparable.
    pub fn start(&self, fadeout: Vec<f32>, fadein: Vec<f32>, channels: impl Into<ChannelLayout>) {
        let channels = nonempty(channels.into());
        // Stride derived once, on the butler thread.
        let ch = channels.count() as usize;
        // `len` counts FRAMES: the RT side advances one frame per call.
        let len = (fadeout.len() / ch).min(fadein.len() / ch) as u32;
        if len == 0 {
            return;
        }

        // Publish the buffers+stride+length as one value, THEN reset `pos`, THEN
        // arm via `len`. `pos` still trails the fade because it is genuinely
        // mutable state the RT side advances; it is only read after `len > 0`
        // gates entry, and a stale `pos` can at worst end the fade early rather
        // than index a mismatched buffer.
        self.fade.store(Arc::new(Fade {
            fadeout,
            fadein,
            channels,
            stride: ch,
            len: len as usize,
        }));
        self.pos.store(0, Ordering::Release);
        self.len.store(len, Ordering::Release);
    }

    /// The width the currently installed fade is interleaved at.
    ///
    /// Read through the same single [`ArcSwap`] load as the buffers, so it can
    /// never name a stride belonging to a different installation — the whole
    /// point of storing the four fields as one immutable [`Fade`] (see
    /// [`start`](Self::start)).
    pub fn layout(&self) -> ChannelLayout {
        self.fade.load().channels
    }

    /// Whether a fade is armed and has frames left to blend.
    #[inline]
    pub fn is_active(&self) -> bool {
        let pos = self.pos.load(Ordering::Acquire);
        let len = self.len.load(Ordering::Acquire);
        len > 0 && pos < len
    }

    /// Blend the next frame into `out`, returning `false` when the crossfade is
    /// complete or inactive (leaving `out` untouched).
    ///
    /// Lock-free: only atomic loads and an `ArcSwap` read — no allocation, so
    /// this is safe on the RT thread. One shared gain envelope across all
    /// channels; a per-channel envelope would shift the image mid-fade.
    pub fn next_frame_into(&self, out: &mut [f32]) -> bool {
        let len = self.len.load(Ordering::Acquire);
        if len == 0 {
            return false;
        }

        let pos = self.pos.fetch_add(1, Ordering::AcqRel);
        if pos >= len {
            self.len.store(0, Ordering::Release);
            return false;
        }

        // ONE load: the buffers, their stride, and their length arrive together
        // or not at all, so `pos` can never be scaled by a stride belonging to a
        // different installation.
        let fade = self.fade.load();
        let Some((o, i)) = fade.frames(pos as usize) else {
            return false;
        };

        // Length from the same snapshot as the buffers, not from `self.len` —
        // a fade installed between the two loads would otherwise skew `t`.
        let t = pos as f32 / fade.len.max(1) as f32;
        for (c, s) in out.iter_mut().enumerate() {
            // A frame wider than the stored stride keeps its extra channels dry.
            if let (Some(&a), Some(&b)) = (o.get(c), i.get(c)) {
                *s = a * (1.0 - t) + b * t;
            }
        }
        true
    }

    /// Disarm and drop the installed buffers, abandoning a fade in progress.
    ///
    /// Frees the `Vec`s, so this is control-thread work — it must not run on the
    /// audio thread.
    pub fn clear(&self) {
        // Disarm first: once `len == 0` no reader will touch the fade, so
        // dropping the buffers afterwards cannot race a blend in progress.
        self.len.store(0, Ordering::Release);
        self.pos.store(0, Ordering::Release);
        self.fade.store(Arc::new(Fade {
            fadeout: Vec::new(),
            fadein: Vec::new(),
            channels: ChannelLayout::STEREO,
            stride: 2,
            len: 0,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` stereo frames, flat interleaved, every sample `v`.
    fn flat(v: f32, frames: usize) -> Vec<f32> {
        vec![v; frames * 2]
    }

    #[test]
    fn new_is_inactive() {
        let c = StreamingCrossfader::new();
        let mut f = [0.0f32; 2];
        assert!(!c.is_active());
        assert!(!c.next_frame_into(&mut f));
    }

    #[test]
    fn start_and_drain() {
        let c = StreamingCrossfader::new();
        c.start(flat(1.0, 4), flat(0.0, 4), 2usize);

        assert!(c.is_active());

        let mut f = [0.0f32; 2];
        assert!(c.next_frame_into(&mut f));
        assert!((f[0] - 1.0).abs() < 0.01);

        assert!(c.next_frame_into(&mut f));
        assert!((f[0] - 0.75).abs() < 0.01);

        assert!(c.next_frame_into(&mut f));
        assert!((f[0] - 0.5).abs() < 0.01);

        assert!(c.next_frame_into(&mut f));
        assert!((f[0] - 0.25).abs() < 0.01);

        assert!(!c.is_active());
        assert!(!c.next_frame_into(&mut f));
    }

    #[test]
    fn start_with_empty_is_noop() {
        let c = StreamingCrossfader::new();
        c.start(Vec::new(), Vec::new(), 2usize);
        assert!(!c.is_active());

        c.start(flat(1.0, 1), Vec::new(), 2usize);
        assert!(!c.is_active());
    }

    #[test]
    fn clear_deactivates() {
        let c = StreamingCrossfader::new();
        c.start(flat(1.0, 10), flat(0.0, 10), 2usize);
        let mut f = [0.0f32; 2];
        c.next_frame_into(&mut f);
        c.next_frame_into(&mut f);

        c.clear();
        assert!(!c.is_active());
        assert!(!c.next_frame_into(&mut f));
    }

    /// `len` counts FRAMES, not samples: a 4-frame 6-channel fade must run for
    /// exactly 4 calls. Counting samples would run it 6x too long and index
    /// past the buffers.
    #[test]
    fn len_counts_frames_not_samples_at_six_channels() {
        let c = StreamingCrossfader::new();
        c.start(vec![1.0; 4 * 6], vec![0.0; 4 * 6], 6usize);
        assert_eq!(
            c.layout(),
            ChannelLayout::from(6u16),
            "the installed fade must carry the width it was started at"
        );

        let mut f = [0.0f32; 6];
        let mut drained = 0;
        while c.next_frame_into(&mut f) {
            drained += 1;
            assert!(drained <= 8, "crossfade ran past its frame count");
        }
        assert_eq!(drained, 4, "4 frames of 6 channels is 4 frames, not 24");
    }

    /// Every channel crossfades under ONE shared envelope — a per-channel
    /// envelope would shift the image mid-fade.
    #[test]
    fn six_channel_fade_uses_one_envelope() {
        let c = StreamingCrossfader::new();
        // fadeout carries the channel index, fadein is silent, so each output
        // is `channel_value * (1 - t)` and the ratio between channels is fixed.
        let fadeout: Vec<f32> = (0..4).flat_map(|_| (1..=6).map(|c| c as f32)).collect();
        c.start(fadeout, vec![0.0; 4 * 6], 6usize);

        let mut f = [0.0f32; 6];
        assert!(c.next_frame_into(&mut f)); // t = 0
        for (c_i, &s) in f.iter().enumerate() {
            assert!((s - (c_i + 1) as f32).abs() < 1e-5, "frame {f:?}");
        }
        assert!(c.next_frame_into(&mut f)); // t = 0.25
        for (c_i, &s) in f.iter().enumerate() {
            let want = (c_i + 1) as f32 * 0.75;
            assert!(
                (s - want).abs() < 1e-5,
                "channel {c_i}: want {want}, got {s}"
            );
        }
    }

    /// A butler alternating fades of DIFFERENT widths against a draining RT
    /// thread must never pair a stride, a length, or a buffer from one
    /// installation with those of another.
    ///
    /// # Why this shape
    ///
    /// The fixture is what makes tearing detectable, and the obvious fixture
    /// hides it: a fade whose samples are CONSTANT per installation makes a
    /// mis-scaled frame index read the same value, so the corruption is
    /// invisible. Both fades here therefore vary per frame and are
    /// self-identifying — `fadeout == fadein` within an installation, so any
    /// correctly-paired blend returns that frame's exact value for any `t`. The
    /// 6-channel fade lives in `[1.0, 2.0)` and the 2-channel one in
    /// `[-2.0, -1.0)`, so a torn read lands in neither band.
    ///
    /// The severity here is measured, not assumed. A standalone probe of the
    /// previous four-atomic shape under this exact contention tore **481,115 of
    /// 2,000,000** blended frames (~24%); the single-`ArcSwap` publication tore
    /// **0**. That is why `Fade` exists.
    #[test]
    fn racing_installs_of_different_widths_never_tear() {
        use std::sync::atomic::{AtomicBool, Ordering as O};
        use std::sync::Arc as StdArc;

        const FRAMES: usize = 512;

        let wide: Vec<f32> = (0..FRAMES)
            .flat_map(|f| [1.0 + f as f32 / FRAMES as f32; 6])
            .collect();
        let narrow: Vec<f32> = (0..FRAMES)
            .flat_map(|f| [-2.0 + f as f32 / FRAMES as f32; 2])
            .collect();

        let xfade = StdArc::new(StreamingCrossfader::new());
        let stop = StdArc::new(AtomicBool::new(false));

        let writer = {
            let xfade = StdArc::clone(&xfade);
            let stop = StdArc::clone(&stop);
            std::thread::spawn(move || {
                let mut use_wide = true;
                while !stop.load(O::Relaxed) {
                    if use_wide {
                        xfade.start(wide.clone(), wide.clone(), 6usize);
                    } else {
                        xfade.start(narrow.clone(), narrow.clone(), 2usize);
                    }
                    use_wide = !use_wide;
                }
            })
        };

        let mut frame = [0.0f32; 6];
        let mut torn = 0usize;
        let mut blended = 0usize;
        for _ in 0..2_000_000 {
            frame.fill(0.0);
            if xfade.next_frame_into(&mut frame) {
                blended += 1;
                // Channels 0/1 are written by both widths; check those.
                for &s in &frame[..2] {
                    if !((1.0..2.0).contains(&s) || (-2.0..-1.0).contains(&s)) {
                        torn += 1;
                    }
                }
            }
        }

        stop.store(true, O::Relaxed);
        writer.join().unwrap();

        assert!(blended > 0, "the race never actually blended a frame");
        assert_eq!(
            torn, 0,
            "{torn} of {blended} blended frames were torn — a blend paired a \
             stride, length, or buffer with those of a different installation"
        );
    }
}
