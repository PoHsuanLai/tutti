//! `AudioIn`/`AudioOut` adapters for the butler's cold-path disk refill.
//!
//! The butler moves frames from a resident (whole-file) [`Wave`] into a
//! per-region ring buffer. Both ends speak the cold-path vocabulary here:
//!
//! - [`WaveIn`] is the one place the planar `wave.at(0,i)/at(1,i)` unpack lives
//!   — an [`AudioIn`] over a `Wave` with a cursor, mono up-mix, optional loop
//!   wrap, and zero-pad past end.
//! - [`RegionOut`] implements [`AudioOut`]: `write` pushes frames into the
//!   bounded ring until it fills (recording how many landed so the caller can
//!   advance the file cursor); `finalize` is a no-op since the ring is a live
//!   SPSC channel, never closed.
//!
//! The butler ring stores `(f32, f32)` frames (the shape the RT sampler reads);
//! the vocabulary is `[f32; 2]`. The `[l, r] <-> (l, r)` conversion is confined
//! to these adapters and [`RegionOut::push_frames`].

use super::super::prefetch::RegionOut;
use tutti_core::io::{AudioIn, AudioOut};
use tutti_core::Wave;

/// The canonical planar→stereo-frame unpack: left = `at(0,i)`, right =
/// `at(1,i)` (or the left sample duplicated for mono), zero past the end.
#[inline]
pub(crate) fn wave_frame(wave: &Wave, channels: usize, idx: usize) -> [f32; 2] {
    if idx >= wave.len() {
        return [0.0, 0.0];
    }
    let left = wave.at(0, idx);
    let right = if channels > 1 { wave.at(1, idx) } else { left };
    [left, right]
}

/// Wrap `pos` into the half-open loop range if it has run past the end.
/// `loop_range` is `(start, end)` in samples; `None` (or an empty range) is the
/// identity. The single source of truth for the loop-wrap arithmetic shared by
/// [`WaveIn`] and the streaming/whole-file forward refills.
#[inline]
pub(crate) fn wrap_position(pos: usize, loop_range: Option<(u64, u64)>) -> usize {
    if let Some((start, end)) = loop_range {
        let (start, end) = (start as usize, end as usize);
        if end > start && pos >= end {
            let loop_len = end - start;
            return start + ((pos - start) % loop_len);
        }
    }
    pos
}

/// A forward [`AudioIn`] over a resident `Wave`, reading from an internal
/// cursor with optional loop wrap. Past the end (with no loop) it yields
/// silence, so it is an *unbounded* source — the caller bounds the transfer by
/// the size of the scratch buffer it fills (`poll_into` always fills the whole
/// buffer, mirroring the old zero-pad-to-`chunk_size` fill).
pub(crate) struct WaveIn<'w> {
    wave: &'w Wave,
    channels: usize,
    cursor: usize,
    /// `(start, end)` half-open loop bounds in samples, if looping.
    loop_bounds: Option<(usize, usize)>,
}

impl<'w> WaveIn<'w> {
    pub(crate) fn new(wave: &'w Wave, start: usize, loop_range: Option<(u64, u64)>) -> Self {
        let loop_bounds = loop_range.and_then(|(start, end)| {
            let start = start as usize;
            let end = end as usize;
            (end > start).then_some((start, end))
        });
        Self {
            wave,
            channels: wave.channels(),
            cursor: start,
            loop_bounds,
        }
    }

    #[inline]
    fn wrap(&self, pos: usize) -> usize {
        if let Some((loop_start, loop_end)) = self.loop_bounds {
            let loop_len = loop_end - loop_start;
            if pos >= loop_end {
                return loop_start + ((pos - loop_start) % loop_len);
            }
        }
        pos
    }
}

impl AudioIn for WaveIn<'_> {
    fn poll_into(&mut self, out: &mut [[f32; 2]]) -> usize {
        for slot in out.iter_mut() {
            self.cursor = self.wrap(self.cursor);
            *slot = wave_frame(self.wave, self.channels, self.cursor);
            self.cursor += 1;
        }
        self.cursor = self.wrap(self.cursor);
        out.len()
    }
}

impl AudioOut for RegionOut {
    fn write(&mut self, frames: &[[f32; 2]]) {
        let n = self.push_frames(frames);
        self.record_accepted(n);
    }

    fn finalize(self) -> std::io::Result<()> {
        // The region ring is a live SPSC channel, never closed by the writer.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::command::RegionId;
    use super::super::super::prefetch::RegionBuffer;
    use super::*;
    use std::path::PathBuf;
    use tutti_core::io::pump;

    fn test_wave(samples: &[(f32, f32)]) -> Wave {
        let mut wave = Wave::new(2, 48000.0);
        for &(l, r) in samples {
            wave.push((l, r));
        }
        wave
    }

    #[test]
    fn wave_frame_reads_stereo() {
        let wave = test_wave(&[(0.1, 0.2), (0.3, 0.4)]);
        assert_eq!(wave_frame(&wave, 2, 0), [0.1, 0.2]);
        assert_eq!(wave_frame(&wave, 2, 1), [0.3, 0.4]);
    }

    #[test]
    fn wave_frame_pads_zeros_past_end() {
        let wave = test_wave(&[(0.5, 0.6)]);
        assert_eq!(wave_frame(&wave, 2, 1), [0.0, 0.0]);
    }

    #[test]
    fn wave_frame_duplicates_left_for_mono() {
        // channels=1 → right takes the left sample.
        let wave = test_wave(&[(0.7, -0.7)]);
        assert_eq!(wave_frame(&wave, 1, 0), [0.7, 0.7]);
    }

    #[test]
    fn wave_in_polls_forward_frames() {
        let wave = test_wave(&[(0.1, 0.1), (0.2, 0.2), (0.3, 0.3)]);
        let mut src = WaveIn::new(&wave, 0, None);
        let mut out = [[0.0f32; 2]; 3];
        assert_eq!(src.poll_into(&mut out), 3);
        assert_eq!(out, [[0.1, 0.1], [0.2, 0.2], [0.3, 0.3]]);
    }

    #[test]
    fn wave_in_zero_pads_past_end() {
        let wave = test_wave(&[(0.2, 0.2)]);
        let mut src = WaveIn::new(&wave, 1, None);
        let mut out = [[9.0f32; 2]; 2];
        src.poll_into(&mut out);
        assert_eq!(out, [[0.0, 0.0], [0.0, 0.0]]);
    }

    #[test]
    fn wave_in_wraps_within_loop() {
        // samples 0..4, loop [1,3): after index 2 the next read wraps to 1.
        let wave = test_wave(&[(0.0, 0.0), (1.0, 1.0), (2.0, 2.0), (3.0, 3.0)]);
        let mut src = WaveIn::new(&wave, 1, Some((1, 3)));
        let mut out = [[0.0f32; 2]; 5];
        src.poll_into(&mut out);
        // 1,2 then wrap → 1,2 then 1
        assert_eq!(
            out,
            [[1.0, 1.0], [2.0, 2.0], [1.0, 1.0], [2.0, 2.0], [1.0, 1.0]]
        );
    }

    #[test]
    fn wrap_position_identity_without_loop() {
        assert_eq!(wrap_position(250, None), 250);
    }

    #[test]
    fn wrap_position_wraps_past_loop_end() {
        assert_eq!(wrap_position(200, Some((100, 200))), 100);
        assert_eq!(wrap_position(250, Some((100, 200))), 150);
        assert_eq!(wrap_position(150, Some((100, 200))), 150); // before end: identity
    }

    #[test]
    fn region_writer_audio_out_accepts_frames() {
        let (mut writer, mut reader) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::from("t.wav"), 8);
        let frames = [[0.1f32, 0.2], [0.3, 0.4]];
        AudioOut::write(&mut writer, &frames);
        assert_eq!(writer.take_accepted(), 2);
        // Frames land in the ring in order, converted to (f32, f32).
        assert_eq!(reader.read(), Some((0.1, 0.2)));
        assert_eq!(reader.read(), Some((0.3, 0.4)));
    }

    #[test]
    fn pump_moves_wave_into_region() {
        let wave = test_wave(&[(0.1, 0.1), (0.2, 0.2), (0.3, 0.3), (0.4, 0.4)]);
        let (mut writer, mut reader) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::from("t.wav"), 8);

        // Bound the transfer by making the source finite: a wrapper that stops.
        struct Finite<'w>(WaveIn<'w>, usize);
        impl AudioIn for Finite<'_> {
            fn poll_into(&mut self, out: &mut [[f32; 2]]) -> usize {
                let n = self.1.min(out.len());
                if n == 0 {
                    return 0;
                }
                self.0.poll_into(&mut out[..n]);
                self.1 -= n;
                n
            }
        }

        let mut src = Finite(WaveIn::new(&wave, 0, None), 4);
        let mut buf = [[0.0f32; 2]; 2];
        let moved = pump(&mut src, &mut writer, &mut buf);
        assert_eq!(moved, 2, "pump moves one buffer's worth per call");
        // Drive to exhaustion.
        let mut total = moved;
        while pump(&mut src, &mut writer, &mut buf) != 0 {
            total += 2;
        }
        assert_eq!(total, 4);
        AudioOut::finalize(writer).unwrap();

        for expected in [(0.1, 0.1), (0.2, 0.2), (0.3, 0.3), (0.4, 0.4)] {
            assert_eq!(reader.read(), Some(expected));
        }
    }
}
