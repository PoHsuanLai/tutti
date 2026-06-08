//! Pre-allocated per-bus FFI scratch that marshals the host's flat,
//! deinterleaved channel pointers into VST3's `AudioBusBuffers` array shape.
//!
//! [`Vst3Instance`](super::instance::Vst3Instance) holds one [`BusBuffers`] per
//! process direction and refreshes it each block via [`BusBuffers::prepare`];
//! the realtime `process` path stays allocation-free because every table here
//! is sized once at construction.

use smallvec::SmallVec;

/// Minimum pointer-table width per bus. A defensive over-read of a stereo
/// table on a mono bus then stays in-bounds (the extra slot points at `aux`).
pub(super) const MIN_PTR_COUNT: usize = 2;

/// Build an `AudioBusBuffers` from a channel count and a raw pointer-array.
///
/// The `channelBuffers32`/`channelBuffers64` union members are the same
/// machine pointer; the plugin selects which to read off
/// `ProcessData::symbolicSampleSize`, so we always write the `channelBuffers32`
/// slot regardless of sample type.
fn make_audio_bus(
    num_channels: usize,
    channel_ptrs: *mut *mut std::ffi::c_void,
) -> vst3::Steinberg::Vst::AudioBusBuffers {
    let mut bus: vst3::Steinberg::Vst::AudioBusBuffers = unsafe { std::mem::zeroed() };
    bus.numChannels = num_channels as i32;
    bus.silenceFlags = 0;
    bus.__field0.channelBuffers32 = channel_ptrs as *mut *mut f32;
    bus
}

/// Pre-allocated per-bus FFI scratch for one process direction. Holds, in
/// bus-index order:
///
/// - `bus_arrays`: a contiguous `AudioBusBuffers` array, handed to
///   `ProcessData::{inputs,outputs}`. Built once; the per-call `prepare`
///   only refreshes the channel pointers.
/// - `ptr_tables`: one `*mut c_void` pointer table per bus. `bus_arrays[i]`'s
///   `channelBuffers` slot points at `ptr_tables[i]`.
/// - `aux`: a single zeroed scratch block (silence for extra input buses /
///   a write-sink for extra output buses). Every aux channel that has no
///   real backing buffer points here.
///
/// Sample-type erased: the `AudioBusBuffers.channelBuffers32`/`64` union
/// members are the same machine pointer, and an all-zero bit pattern is `0.0`
/// for both `f32` and `f64`, so one scratch serves both. The `aux` block is
/// sized in `f64`s (8 bytes/sample) so it is large enough for the wider type.
///
/// Allocated once in
/// [`Vst3Instance::from_loaded`](super::instance::Vst3Instance) and reused so
/// the realtime `process` path is allocation-free. The flat caller buffer
/// (bus 0) is mapped onto bus 0; every other bus is backed by `aux`.
pub(super) struct BusBuffers {
    bus_channels: SmallVec<[usize; 4]>,
    bus_arrays: SmallVec<[vst3::Steinberg::Vst::AudioBusBuffers; 4]>,
    ptr_tables: SmallVec<[Vec<*mut std::ffi::c_void>; 4]>,
    aux: Vec<f64>,
}

// The pointer tables hold raw pointers into host-owned buffers that outlive
// each process call, mirroring `BufferPtrs`.
unsafe impl Send for BusBuffers {}
unsafe impl Sync for BusBuffers {}

impl BusBuffers {
    /// Pre-allocate per-bus pointer tables + the aux scratch block.
    ///
    /// `bus_channels` is the per-bus channel layout (empty == single bus of
    /// `fallback_channels`). `block_size` sizes the aux silence/sink block.
    pub(super) fn new(bus_channels: &[usize], fallback_channels: usize, block_size: usize) -> Self {
        let bus_channels: SmallVec<[usize; 4]> = if bus_channels.is_empty() {
            SmallVec::from_slice(&[fallback_channels])
        } else {
            SmallVec::from_slice(bus_channels)
        };
        let mut bus_arrays = SmallVec::new();
        let mut ptr_tables: SmallVec<[Vec<*mut std::ffi::c_void>; 4]> = SmallVec::new();
        for &ch in &bus_channels {
            let table = vec![std::ptr::null_mut::<std::ffi::c_void>(); ch.max(MIN_PTR_COUNT)];
            let mut bus = make_audio_bus(ch, std::ptr::null_mut());
            // channelBuffers pointer is refreshed every `prepare`; leave it
            // null now so a stale (about-to-move) Vec pointer is never read.
            bus.__field0.channelBuffers32 = std::ptr::null_mut();
            bus_arrays.push(bus);
            ptr_tables.push(table);
        }
        Self {
            bus_channels,
            bus_arrays,
            ptr_tables,
            aux: vec![0.0f64; block_size.max(1)],
        }
    }

    pub(super) fn num_buses(&self) -> usize {
        self.bus_channels.len()
    }

    /// Refresh every bus's channel pointers ahead of a `process` call and
    /// return the `*mut AudioBusBuffers` for `ProcessData`.
    ///
    /// `live`/`live_len` are the real channel pointers from
    /// [`BufferPtrs::prepare`](crate::types::BufferPtrs), laid out flat in bus
    /// order: bus 0's channels first, then bus 1's, and so on. Each bus consumes
    /// the next slice of `live`; a channel reads `live[flat]` while
    /// `flat < live_len`, and falls back to the shared `aux` scratch once `live`
    /// is exhausted (or for a padding slot beyond the bus's real channel count).
    /// This is what carries a sidechain bus: as long as the caller provides
    /// `main + sidechain` channels flat, bus 1 picks up the sidechain channels
    /// instead of silence.
    ///
    /// The aux block is re-zeroed for the input direction (`zero_aux = true`)
    /// so any bus channel without a live backing receives silence; for the
    /// output direction the sink contents are discarded.
    ///
    /// Allocation-free: pointer tables and `aux` were sized at construction.
    ///
    /// # Safety
    /// `live` must point at an array of at least `live_len` valid channel
    /// pointers that outlive the subsequent `process` call.
    pub(super) unsafe fn prepare(
        &mut self,
        live: *const *mut std::ffi::c_void,
        live_len: usize,
        zero_aux: bool,
    ) -> *mut vst3::Steinberg::Vst::AudioBusBuffers {
        if zero_aux {
            for s in self.aux.iter_mut() {
                *s = 0.0;
            }
        }
        let aux_ptr = self.aux.as_mut_ptr() as *mut std::ffi::c_void;
        // Running index into the flat, bus-ordered `live` array.
        let mut flat = 0usize;
        for (bus_idx, table) in self.ptr_tables.iter_mut().enumerate() {
            let bus_ch = self.bus_channels[bus_idx];
            for (c, slot) in table.iter_mut().enumerate() {
                if c < bus_ch && flat < live_len {
                    *slot = *live.add(flat);
                    flat += 1;
                } else {
                    // Aux/silence: bus channel with no live backing (extra bus
                    // beyond what the caller supplied) or a padding slot.
                    *slot = aux_ptr;
                }
            }
            let bus = &mut self.bus_arrays[bus_idx];
            bus.numChannels = bus_ch as i32;
            bus.silenceFlags = 0;
            bus.__field0.channelBuffers32 = table.as_mut_ptr() as *mut *mut f32;
        }
        self.bus_arrays.as_mut_ptr()
    }
}

#[cfg(test)]
mod tests {
    use super::{BusBuffers, MIN_PTR_COUNT};
    use std::ffi::c_void;

    const BLOCK: usize = 64;

    /// Single-bus legacy: empty layout collapses to one bus of the fallback
    /// channel count, and bus 0 maps straight onto the live channels.
    #[test]
    fn empty_layout_is_single_bus() {
        let mut bb = BusBuffers::new(&[], 2, BLOCK);
        assert_eq!(bb.num_buses(), 1);
        assert_eq!(bb.bus_channels[0], 2);

        let mut ch0 = [1.0f32; BLOCK];
        let mut ch1 = [2.0f32; BLOCK];
        let live = [
            ch0.as_mut_ptr() as *mut c_void,
            ch1.as_mut_ptr() as *mut c_void,
        ];
        unsafe {
            let arrays = bb.prepare(live.as_ptr(), live.len(), true);
            let bus0 = &*arrays;
            assert_eq!(bus0.numChannels, 2);
            let ptrs = bus0.__field0.channelBuffers32 as *const *mut f32;
            assert_eq!(*ptrs.add(0), ch0.as_mut_ptr());
            assert_eq!(*ptrs.add(1), ch1.as_mut_ptr());
        }
    }

    /// Multi-bus input: bus 0 takes the live channels; the second (sidechain)
    /// bus is backed by the shared, zeroed aux block, never the live buffers.
    #[test]
    fn extra_input_bus_gets_silence() {
        // main = stereo, sidechain = mono.
        let mut bb = BusBuffers::new(&[2, 1], 2, BLOCK);
        assert_eq!(bb.num_buses(), 2);

        let mut l = [5.0f32; BLOCK];
        let mut r = [6.0f32; BLOCK];
        let live = [l.as_mut_ptr() as *mut c_void, r.as_mut_ptr() as *mut c_void];
        unsafe {
            let arrays = bb.prepare(live.as_ptr(), live.len(), true);
            let bus0 = &*arrays.add(0);
            let bus1 = &*arrays.add(1);
            assert_eq!(bus0.numChannels, 2);
            assert_eq!(bus1.numChannels, 1);

            let p0 = bus0.__field0.channelBuffers32 as *const *mut f32;
            assert_eq!(*p0.add(0), l.as_mut_ptr());
            assert_eq!(*p0.add(1), r.as_mut_ptr());

            // Sidechain channel points into the aux silence block, and its
            // sample reads as zero.
            let p1 = bus1.__field0.channelBuffers32 as *const *mut f32;
            let sc = *p1.add(0);
            assert!(sc != l.as_mut_ptr() && sc != r.as_mut_ptr());
            assert_eq!(*sc, 0.0);
        }
    }

    /// Multi-bus input WITH the sidechain channel supplied flat: bus 0 takes
    /// the first 2 live channels, bus 1 (sidechain) takes the 3rd. This is the
    /// Stage-3 delivery path — the flat caller buffer carries main + sidechain
    /// in bus order, so the running flat index hands bus 1 the real channel
    /// rather than aux silence.
    #[test]
    fn sidechain_bus_reads_supplied_channel() {
        // main = stereo, sidechain = mono.
        let mut bb = BusBuffers::new(&[2, 1], 2, BLOCK);

        let mut l = [5.0f32; BLOCK];
        let mut r = [6.0f32; BLOCK];
        let mut sc = [7.0f32; BLOCK];
        let live = [
            l.as_mut_ptr() as *mut c_void,
            r.as_mut_ptr() as *mut c_void,
            sc.as_mut_ptr() as *mut c_void,
        ];
        unsafe {
            let arrays = bb.prepare(live.as_ptr(), live.len(), true);
            let bus0 = &*arrays.add(0);
            let bus1 = &*arrays.add(1);

            let p0 = bus0.__field0.channelBuffers32 as *const *mut f32;
            assert_eq!(*p0.add(0), l.as_mut_ptr());
            assert_eq!(*p0.add(1), r.as_mut_ptr());

            // Sidechain channel now points at the supplied buffer, reading 7.0,
            // NOT the aux silence block.
            let p1 = bus1.__field0.channelBuffers32 as *const *mut f32;
            let scp = *p1.add(0);
            assert_eq!(scp, sc.as_mut_ptr());
            assert_eq!(*scp, 7.0);
        }
    }

    /// The per-block `prepare` must not allocate — it only refills pre-sized
    /// pointer tables and re-zeros the aux block.
    #[test]
    fn prepare_is_alloc_free() {
        let mut bb = BusBuffers::new(&[2, 1], 2, BLOCK);
        let mut l = [0.5f32; BLOCK];
        let mut r = [0.5f32; BLOCK];
        let live = [l.as_mut_ptr() as *mut c_void, r.as_mut_ptr() as *mut c_void];
        // Warm up once outside the guard to mirror the RT discipline of the
        // real process loop.
        unsafe {
            let _ = bb.prepare(live.as_ptr(), live.len(), true);
        }
        assert_no_alloc::assert_no_alloc(|| unsafe {
            for _ in 0..256 {
                let _ = bb.prepare(live.as_ptr(), live.len(), true);
            }
        });
    }

    /// Output direction: extra buses get a sink (no zeroing required) and the
    /// padding slots (up to MIN_PTR_COUNT) stay non-null.
    #[test]
    fn output_padding_slots_are_non_null() {
        // Single mono output bus → table padded to MIN_PTR_COUNT.
        let mut bb = BusBuffers::new(&[1], 1, BLOCK);
        assert!(bb.ptr_tables[0].len() >= MIN_PTR_COUNT);
        let mut m = [9.0f32; BLOCK];
        let live = [m.as_mut_ptr() as *mut c_void];
        unsafe {
            let arrays = bb.prepare(live.as_ptr(), live.len(), false);
            let bus0 = &*arrays;
            let p = bus0.__field0.channelBuffers32 as *const *mut f32;
            assert_eq!(*p.add(0), m.as_mut_ptr());
            // Padding slot is non-null (points at aux); a defensive over-read
            // stays in-bounds even though the plugin sees numChannels=1.
            assert!(!(*p.add(1)).is_null());
        }
    }
}
