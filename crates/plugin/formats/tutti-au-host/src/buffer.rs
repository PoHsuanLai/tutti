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
/// The real C struct ends with a flexible-array member, so we allocate a
/// correctly-sized, correctly-aligned slab and reinterpret it.
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
                audio_buf.mDataByteSize = frames * std::mem::size_of::<f32>() as u32;
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

/// Render scratch area: output buffers, input buffers, and a monotonically
/// advancing sample position used for timestamps.
pub(crate) struct RenderScratch {
    list: RenderBufferList,
    pub outputs: Vec<Vec<f32>>,
    pub inputs: Vec<Vec<f32>>,
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

    /// Return the pre-advance sample position and move the cursor forward by
    /// `frames`. The pre-advance value is what AudioToolbox expects for the
    /// current block's timestamp.
    pub fn advance(&mut self, frames: u32) -> f64 {
        let prev = self.sample_position;
        self.sample_position += frames as f64;
        prev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AU-C1 regression. The size math must be driven by
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

        // The header contribution is the *offset*, which is strictly larger
        // than `size_of::<u32>()` — that inequality IS the bug.
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
            ChannelLayout::Mono,
            ChannelLayout::Stereo,
            ChannelLayout::Quad,
            ChannelLayout::Multi(8),
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

    /// Every `mData` the bind loop writes must land inside the slab — the exact
    /// store that was out of bounds before.
    #[test]
    fn bind_writes_stay_inside_the_slab() {
        for layout in [
            ChannelLayout::Mono,
            ChannelLayout::Stereo,
            ChannelLayout::Multi(6),
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
