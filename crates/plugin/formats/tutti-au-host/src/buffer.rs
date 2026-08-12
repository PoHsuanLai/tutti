//! Render-time buffer management.
//!
//! Allocates the variable-length `AudioBufferList` backing storage and the
//! per-channel scratch buffers used to marshal samples in/out of AudioToolbox.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;

use tutti_plugin_types::ChannelLayout;

use crate::stream::AuBusLayout;
use crate::types::*;

/// Byte size of an `AudioBufferList` carrying `n` trailing `AudioBuffer`s.
///
/// `AudioBufferList` is `{ UInt32 mNumberBuffers; AudioBuffer mBuffers[1]; }`,
/// and `AudioBuffer` contains a pointer, so the struct is 8-aligned and
/// `mBuffers` starts at offset **8**, not 4 — there are 4 bytes of tail padding
/// after `mNumberBuffers`. Sizing off `size_of::<u32>()` under-allocates by
/// exactly those 4 bytes and makes the last channel's `mData` write land past
/// the end of the slab. Measured on macOS/arm64 against the real SDK header:
/// `sizeof(AudioBuffer) == 16`, `offsetof(AudioBufferList, mBuffers) == 8`.
pub(crate) const fn buffer_list_bytes(n: usize) -> usize {
    std::mem::offset_of!(AudioBufferList, mBuffers) + n * std::mem::size_of::<AudioBuffer>()
}

/// 8-byte-aligned allocation unit for the `AudioBufferList` slab.
///
/// `Box<[u8]>` is only 1-byte aligned; `AudioBufferList` needs
/// `align_of::<AudioBufferList>()` (8 on every Apple platform). Backing the
/// slab with `u64` gives that alignment for free without a manual
/// `alloc`/`dealloc` pair.
type AblWord = u64;

const _: () = assert!(std::mem::align_of::<AblWord>() >= std::mem::align_of::<AudioBufferList>());

/// Backing storage for an `AudioBufferList` plus its trailing `AudioBuffer`s.
///
/// The real C struct ends with a flexible-array member, so this allocates a
/// correctly-sized, correctly-aligned slab and reinterprets it.
pub(crate) struct RenderBufferList {
    storage: Box<[AblWord]>,
    channels: ChannelLayout,
}

impl RenderBufferList {
    pub fn new(channels: ChannelLayout) -> Self {
        let bytes = buffer_list_bytes(channels.count() as usize);
        let words = bytes.div_ceil(std::mem::size_of::<AblWord>());
        Self {
            storage: vec![0 as AblWord; words].into_boxed_slice(),
            channels,
        }
    }

    /// Point the list at `buffers` (one `Vec<f32>` per channel) and return a
    /// pointer suitable for `AudioUnitRender` or the input render callback.
    pub fn bind(&mut self, buffers: &mut [Vec<f32>], frames: u32) -> *mut AudioBufferList {
        // `storage` is 8-aligned (see `AblWord`) and `buffer_list_bytes` long,
        // so this cast and every trailing `mBuffers[ch]` write below stay in
        // bounds for `ch < channels.count()`.
        let ptr = self.storage.as_mut_ptr() as *mut AudioBufferList;
        unsafe {
            (*ptr).mNumberBuffers = self.channels.count() as u32;
            for (ch, buf) in buffers
                .iter_mut()
                .take(self.channels.count() as usize)
                .enumerate()
            {
                let audio_buf = &mut *((&mut (*ptr).mBuffers[0] as *mut AudioBuffer).add(ch));
                audio_buf.mNumberChannels = 1;
                // Multiply in `usize`, then narrow — `as` binds tighter than
                // `*`, so `frames * size_of::<f32>() as u32` multiplies two
                // u32s and wraps for `frames > u32::MAX / 4`, handing the
                // plugin a byte size far smaller than the buffer it is given.
                // `AuInstance::process` rejects `num_frames > block_size`
                // before reaching here, so this is not reachable today; `bind`
                // is public and should not depend on a caller's guard for it.
                // Same form as `instance.rs`'s output-size report.
                audio_buf.mDataByteSize =
                    u32::try_from(frames as usize * std::mem::size_of::<f32>()).unwrap_or(u32::MAX);
                audio_buf.mData = buf.as_mut_ptr() as *mut c_void;
            }
        }
        ptr
    }
}

/// Iterate the `AudioBuffer`s inside a raw `AudioBufferList`.
///
/// # Safety
/// `abl` must be a valid, well-formed `AudioBufferList` with at least
/// `number_buffers` trailing `AudioBuffer`s in contiguous memory.
pub(crate) unsafe fn iter_buffers_mut<'a>(
    abl: *mut AudioBufferList,
) -> impl Iterator<Item = &'a mut AudioBuffer> {
    let count = (*abl).mNumberBuffers as usize;
    let base = &mut (*abl).mBuffers[0] as *mut AudioBuffer;
    (0..count).map(move |i| &mut *base.add(i))
}

/// Render scratch area: output buffers, input buffers, and the sample position
/// used for timestamps.
pub(crate) struct RenderScratch {
    list: RenderBufferList,
    pub outputs: Vec<Vec<f32>>,
    pub inputs: Vec<Vec<f32>>,
    /// Advances by one block per render and returns to zero on
    /// [`reset_position`](Self::reset_position); see that method for why a
    /// discontinuity restarts it rather than seeking it.
    sample_position: f64,
}

impl RenderScratch {
    pub fn new(layout: AuBusLayout, block_size: u32) -> Self {
        let out_ch = layout.outputs.count() as usize;
        let in_ch = if layout.has_input {
            layout.inputs.count() as usize
        } else {
            0
        };
        let size = block_size as usize;

        let outputs = (0..out_ch).map(|_| vec![0.0f32; size]).collect();
        // Allocate at least as many input channels as outputs so effects that
        // report 0 input channels but still pull stereo input don't OOB.
        let inputs = (0..in_ch.max(out_ch)).map(|_| vec![0.0f32; size]).collect();

        Self {
            list: RenderBufferList::new(layout.outputs),
            outputs,
            inputs,
            sample_position: 0.0,
        }
    }

    pub fn bind_output(&mut self, frames: u32) -> *mut AudioBufferList {
        self.list.bind(&mut self.outputs, frames)
    }

    pub fn stage_input(&mut self, input: &[&[f32]], frames: u32) {
        for (ch, src) in input.iter().enumerate() {
            if let Some(dst) = self.inputs.get_mut(ch) {
                let len = (frames as usize).min(src.len()).min(dst.len());
                dst[..len].copy_from_slice(&src[..len]);
            }
        }
    }

    pub fn emit_output(&self, output: &mut [&mut [f32]], frames: u32) {
        for (ch, dst) in output.iter_mut().enumerate() {
            if let Some(src) = self.outputs.get(ch) {
                let len = (frames as usize).min(dst.len()).min(src.len());
                dst[..len].copy_from_slice(&src[..len]);
            }
        }
    }

    /// [`stage_input`](Self::stage_input) for an f64 caller, narrowing to the
    /// f32 the AU renders in.
    ///
    /// AUv2 has no f64 render entry point — `AudioUnitRender` takes an
    /// `AudioBufferList` of `Float32` and there is no `…F64` twin the way VST 2.4
    /// has `processReplacingF64`. The conversion is therefore unavoidable for an
    /// f64 caller, and the narrowing is the AU's own precision ceiling rather
    /// than a loss this layer chose. Converting *here*, against the scratch that
    /// already exists, is what keeps it free of per-block allocation.
    ///
    /// Writes into the same `inputs` buffers as the f32 path, so a caller cannot
    /// mix widths within one block — nor does anything ask to: the width is
    /// picked per block by the pipeline's `SampleFormat` and applies to both
    /// directions.
    pub fn stage_input_f64(&mut self, input: &[&[f64]], frames: u32) {
        for (ch, src) in input.iter().enumerate() {
            if let Some(dst) = self.inputs.get_mut(ch) {
                let len = (frames as usize).min(src.len()).min(dst.len());
                for (d, &s) in dst[..len].iter_mut().zip(&src[..len]) {
                    *d = s as f32;
                }
            }
        }
    }

    /// [`emit_output`](Self::emit_output) for an f64 caller, widening back from
    /// the f32 the AU rendered. Bounded on both sides for the same reason.
    pub fn emit_output_f64(&self, output: &mut [&mut [f64]], frames: u32) {
        for (ch, dst) in output.iter_mut().enumerate() {
            if let Some(src) = self.outputs.get(ch) {
                let len = (frames as usize).min(dst.len()).min(src.len());
                for (d, &s) in dst[..len].iter_mut().zip(&src[..len]) {
                    *d = s as f64;
                }
            }
        }
    }

    /// Return the pre-advance sample position and move the cursor forward by
    /// `frames`. The pre-advance value is what AudioToolbox expects for the
    /// current block's timestamp.
    pub fn advance(&mut self, frames: u32) -> f64 {
        let prev = self.sample_position;
        self.sample_position += frames as f64;
        prev
    }

    /// Send the render cursor back to zero, so the next block's `mSampleTime`
    /// starts a fresh run rather than continuing the previous one.
    ///
    /// Paired with [`AuInstance::reset`](crate::instance::AuInstance::reset):
    /// that call flushes the AU's signal history, and this discards the clock
    /// the flushed history was measured against. Leaving the cursor running
    /// hands an AU that phases anything off `mSampleTime` — an LFO, a
    /// look-ahead window, a block-to-block delta — a timestamp saying the block
    /// after a locate is contiguous with the block before it, which is exactly
    /// what the flush just said it is not.
    ///
    /// Zero rather than a host-supplied playhead. `mSampleTime` is documented
    /// as the stamp that lets an AU "determine without doubt that this is the
    /// same render operation" (`AUComponent.h`, `AudioUnitRender`) — a
    /// per-instance render clock, not a position on the project timeline. The
    /// timeline has its own channel, `outCurrentSampleInTimeLine` on the
    /// transport callbacks, which an AU asks for separately and which
    /// [`TransportState::set_transport`](crate::transport::TransportState::set_transport)
    /// publishes. Writing a playhead here would answer that question twice, in
    /// two places, with no way for an AU to tell which clock it received.
    pub fn reset_position(&mut self) {
        self.sample_position = 0.0;
    }

    /// The position the next block will be stamped with.
    #[cfg(test)]
    pub fn position(&self) -> f64 {
        self.sample_position
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The size math must be driven by
    /// `offset_of!(AudioBufferList, mBuffers)`, never by `size_of::<u32>()`.
    /// The two differ by the 4 bytes of tail padding after `mNumberBuffers`
    /// (the struct is 8-aligned because `AudioBuffer` holds a pointer), and
    /// sizing off `u32` makes the LAST channel's `mData` store run past the end
    /// of the slab on every render — mono included. It only ever "worked"
    /// because malloc rounds 36 up to a 48-byte bucket.
    #[test]
    fn buffer_list_size_matches_the_c_abi() {
        // Ground truth, measured on macOS/arm64 against the real SDK header.
        assert_eq!(std::mem::size_of::<AudioBuffer>(), 16);
        assert_eq!(std::mem::offset_of!(AudioBufferList, mBuffers), 8);
        assert_eq!(std::mem::size_of::<AudioBufferList>(), 24);

        // The header contribution is the *offset*, strictly larger than
        // `size_of::<u32>()` because of the alignment padding after
        // `mNumberBuffers`. Sizing from the field width instead of the offset
        // under-allocates by exactly that padding, so this inequality is what
        // makes the two formulas distinguishable at all.
        assert!(
            std::mem::offset_of!(AudioBufferList, mBuffers) > std::mem::size_of::<u32>(),
            "if these were equal the old formula would have been correct"
        );

        // A 1-buffer list must be at least as large as the declared struct.
        assert!(buffer_list_bytes(1) >= std::mem::size_of::<AudioBufferList>());

        for n in 0..=8usize {
            assert_eq!(
                buffer_list_bytes(n),
                std::mem::offset_of!(AudioBufferList, mBuffers)
                    + n * std::mem::size_of::<AudioBuffer>(),
            );
        }
        // The concrete numbers the audit computed: 40 for stereo, not 36.
        assert_eq!(buffer_list_bytes(2), 40);
        assert_eq!(buffer_list_bytes(1), 24);
    }

    /// The slab must be big enough AND aligned for `AudioBufferList`. A
    /// `Box<[u8]>` is only 1-byte aligned, which is UB to reinterpret as an
    /// 8-aligned struct even where it happens to work.
    ///
    /// The required size is recomputed here straight from `offset_of!` rather
    /// than reusing `buffer_list_bytes`, which is the function under test —
    /// checking it against itself would pass with any formula.
    #[test]
    fn slab_is_large_enough_and_aligned() {
        for layout in [
            ChannelLayout::MONO,
            ChannelLayout::STEREO,
            ChannelLayout::QUAD,
            ChannelLayout::from(8u16),
        ] {
            let list = RenderBufferList::new(layout);
            let n = layout.count() as usize;
            let required = std::mem::offset_of!(AudioBufferList, mBuffers)
                + n * std::mem::size_of::<AudioBuffer>();
            let bytes = std::mem::size_of_val(&*list.storage);
            assert!(
                bytes >= required,
                "slab {bytes} bytes < required {required} for {n} channels"
            );
            assert_eq!(
                list.storage.as_ptr() as usize % std::mem::align_of::<AudioBufferList>(),
                0,
                "slab must be aligned for AudioBufferList"
            );
        }
    }

    /// The render cursor advances by one block per call, returns the block's
    /// *start* frame, and goes back to zero on `reset_position`.
    ///
    /// The narrow, allocation-free half of what `tests/au_render_clock.rs`
    /// asserts against a real AU: that suite proves the timestamp the plugin
    /// receives, this one proves the arithmetic underneath it. Kept separate
    /// because the failure modes differ — a wrong `advance` and a `reset` wired
    /// to the wrong place look identical from outside.
    #[test]
    fn the_render_cursor_advances_and_restarts() {
        let mut scratch = RenderScratch::new(
            AuBusLayout {
                inputs: ChannelLayout::STEREO,
                outputs: ChannelLayout::STEREO,
                has_input: true,
            },
            64,
        );

        assert_eq!(scratch.position(), 0.0, "a fresh cursor starts at zero");
        assert_eq!(
            scratch.advance(64),
            0.0,
            "the first block starts at frame 0"
        );
        assert_eq!(scratch.advance(64), 64.0, "the second starts one block on");
        assert_eq!(scratch.position(), 128.0);

        scratch.reset_position();
        assert_eq!(scratch.position(), 0.0);
        // And it advances again from there rather than sticking — the wrong fix
        // (zeroing on every block) would pass the line above and fail here.
        assert_eq!(scratch.advance(64), 0.0);
        assert_eq!(scratch.advance(64), 64.0);
    }

    /// A stereo scratch with `block` frames per channel.
    fn stereo_scratch(block: u32) -> RenderScratch {
        RenderScratch::new(
            AuBusLayout {
                inputs: ChannelLayout::STEREO,
                outputs: ChannelLayout::STEREO,
                has_input: true,
            },
            block,
        )
    }

    /// The f64 staging pair must carry a block through unchanged apart from the
    /// f32 narrowing the AU forces.
    ///
    /// Round-tripping through `outputs` directly (rather than through a real
    /// `AudioUnitRender`) is what makes this a pure test of the conversion: an AU
    /// in the loop would make a failure ambiguous between the staging and the
    /// plugin.
    #[test]
    fn f64_staging_round_trips_through_the_f32_scratch() {
        let mut scratch = stereo_scratch(4);

        let l: [f64; 4] = [0.25, -0.5, 0.75, -1.0];
        let r: [f64; 4] = [1.0, 0.5, -0.25, 0.125];
        scratch.stage_input_f64(&[&l[..], &r[..]], 4);

        // Every value above is exactly representable in f32, so the staging must
        // be bit-exact — no epsilon needed, and a comparison that needed one
        // would be reporting a bug rather than float noise.
        assert_eq!(scratch.inputs[0][..4], [0.25f32, -0.5, 0.75, -1.0]);
        assert_eq!(scratch.inputs[1][..4], [1.0f32, 0.5, -0.25, 0.125]);

        // Stand in for the AU's render: whatever lands in `outputs` is what
        // `emit_output_f64` must widen back out.
        scratch.outputs[0][..4].copy_from_slice(&[0.25, -0.5, 0.75, -1.0]);
        scratch.outputs[1][..4].copy_from_slice(&[1.0, 0.5, -0.25, 0.125]);

        let (mut out_l, mut out_r) = ([0.0f64; 4], [0.0f64; 4]);
        let mut outs: Vec<&mut [f64]> = vec![&mut out_l, &mut out_r];
        scratch.emit_output_f64(&mut outs, 4);

        assert_eq!(out_l, [0.25, -0.5, 0.75, -1.0]);
        assert_eq!(out_r, [1.0, 0.5, -0.25, 0.125]);
    }

    /// Precision past f32 does not survive, and that is the documented contract
    /// rather than a defect — AUv2 renders in f32 and has no f64 entry point.
    ///
    /// Pinned so the loss is a stated property with a test behind it. If AUv3 or
    /// a future path ever renders natively at f64, this test failing is the
    /// correct alarm: the doc on `process_f64` would then be wrong.
    #[test]
    fn f64_staging_narrows_to_the_aus_own_precision() {
        // Needs 53 bits of mantissa: exact in f64, rounds in f32.
        const EXACT: f64 = 1.0 + f64::EPSILON;
        assert_ne!(
            EXACT as f32 as f64, EXACT,
            "the fixture is vacuous unless this value actually rounds in f32"
        );

        let mut scratch = stereo_scratch(4);
        let src = [EXACT; 4];
        scratch.stage_input_f64(&[&src[..]], 4);

        assert_eq!(
            scratch.inputs[0][0], EXACT as f32,
            "staged at the AU's precision, not the caller's"
        );
    }

    /// Both f64 halves are bounded on the caller's side and the scratch's, so a
    /// caller slice shorter OR longer than the block cannot panic.
    ///
    /// The f32 pair already clamps with a three-way `min`; these were written to
    /// match rather than to fix a live crash, and this pins that they do. A
    /// staging pair that indexed `[..frames]` on the caller directly would panic
    /// on the short case, which is the mirror of a fault fixed in VST2's scratch.
    #[test]
    fn f64_staging_is_bounded_on_both_sides() {
        let mut scratch = stereo_scratch(8);

        // Caller supplies fewer frames than the block asks for.
        let short = [1.0f64; 3];
        scratch.stage_input_f64(&[&short[..]], 8);
        assert_eq!(&scratch.inputs[0][..3], &[1.0f32; 3]);

        // Caller supplies more than the scratch holds.
        let long = [2.0f64; 32];
        scratch.stage_input_f64(&[&long[..]], 8);
        assert_eq!(&scratch.inputs[0][..8], &[2.0f32; 8]);

        // And the emit half, both directions.
        for ch in scratch.outputs.iter_mut() {
            ch.fill(0.5);
        }

        let mut out_short = [0.0f64; 3];
        let mut outs: Vec<&mut [f64]> = vec![&mut out_short];
        scratch.emit_output_f64(&mut outs, 8);
        assert_eq!(out_short, [0.5f64; 3], "filled as far as the caller goes");

        let mut out_long = [-1.0f64; 12];
        let mut outs: Vec<&mut [f64]> = vec![&mut out_long];
        scratch.emit_output_f64(&mut outs, 8);
        assert_eq!(&out_long[..8], &[0.5f64; 8]);
        assert!(
            out_long[8..].iter().all(|&v| v == -1.0),
            "past the block stays untouched"
        );
    }

    /// More caller channels than the scratch holds are skipped, not indexed into.
    #[test]
    fn f64_staging_ignores_channels_the_scratch_lacks() {
        let mut scratch = RenderScratch::new(
            AuBusLayout {
                inputs: ChannelLayout::MONO,
                outputs: ChannelLayout::MONO,
                has_input: true,
            },
            4,
        );

        let (a, b) = ([1.0f64; 4], [2.0f64; 4]);
        scratch.stage_input_f64(&[&a[..], &b[..]], 4);
        assert_eq!(&scratch.inputs[0][..4], &[1.0f32; 4]);

        scratch.outputs[0][..4].fill(0.25);
        let (mut out_a, mut out_b) = ([0.0f64; 4], [-1.0f64; 4]);
        let mut outs: Vec<&mut [f64]> = vec![&mut out_a, &mut out_b];
        scratch.emit_output_f64(&mut outs, 4);

        assert_eq!(out_a, [0.25f64; 4]);
        assert_eq!(out_b, [-1.0f64; 4], "channel 1 has no scratch behind it");
    }

    /// Every `mData` the bind loop writes must land inside the slab — the exact
    /// store that was out of bounds before.
    #[test]
    fn bind_writes_stay_inside_the_slab() {
        for layout in [
            ChannelLayout::MONO,
            ChannelLayout::STEREO,
            ChannelLayout::from(6u16),
        ] {
            let n = layout.count() as usize;
            let mut list = RenderBufferList::new(layout);
            let lo = list.storage.as_ptr() as usize;
            let hi = lo + std::mem::size_of_val(&*list.storage);

            let mut chans: Vec<Vec<f32>> = (0..n).map(|_| vec![0.0f32; 64]).collect();
            let abl = list.bind(&mut chans, 64);

            unsafe {
                assert_eq!((*abl).mNumberBuffers as usize, n);
                let base = &raw const (*abl).mBuffers[0];
                for (ch, chan) in chans.iter_mut().enumerate().take(n) {
                    let b = base.add(ch);
                    // The whole AudioBuffer, including its trailing mData
                    // field, must sit within the allocation.
                    let start = b as usize;
                    let end = start + std::mem::size_of::<AudioBuffer>();
                    assert!(
                        start >= lo && end <= hi,
                        "AudioBuffer[{ch}] spans {start}..{end}, slab is {lo}..{hi}"
                    );
                    assert_eq!((*b).mNumberChannels, 1);
                    assert_eq!((*b).mDataByteSize, 64 * 4);
                    assert_eq!((*b).mData, chan.as_mut_ptr() as *mut c_void);
                }
            }
        }
    }
}
