//! Pre-allocated per-bus FFI scratch that marshals the host's flat,
//! deinterleaved channel pointers into VST3's `AudioBusBuffers` array shape.
//!
//! [`Vst3Instance`](super::instance::Vst3Instance) holds one [`BusBuffers`] per
//! process direction and refreshes it each block via [`BusBuffers::prepare`];
//! the realtime `process` path stays allocation-free because every table here
//! is sized once at construction.

use std::marker::PhantomData;

use smallvec::SmallVec;

use tutti_types::ChannelLayout;

use crate::types::Vst3Sample;

/// Minimum pointer-table width per bus. A defensive over-read of a stereo
/// table on a mono bus then stays in-bounds (the extra slot points at `aux`).
pub(super) const MIN_PTR_COUNT: usize = 2;

/// The resolved scratch sizing for one process direction, derived from a
/// plugin's per-bus channel layout.
///
/// Generic over the committed sample format `T` so the [`BusBuffers`] it holds
/// writes the matching `channelBuffers32`/`64` union member.
///
/// Both the initial sizing (from the `PluginInfo` snapshot) and the
/// post-activation re-sync (from the live component, since some plugins only
/// finalise their arrangement once active) reduce to the same computation:
/// given the per-bus channel vec plus the main-bus channel count, produce the
/// flat `BufferPtrs` width and the per-bus [`BusBuffers`] scratch.
pub(super) struct DirectionScratch<T: Vst3Sample> {
    /// Per-bus `AudioBusBuffers` scratch for this direction.
    pub buses: BusBuffers<T>,
    /// Flat `BufferPtrs` width: the per-direction channel total (main +
    /// sidechain/aux), clamped to [`MIN_PTR_COUNT`]. The flat caller buffer
    /// carries every bus's channels in bus order, so sizing to just the main
    /// bus would truncate sidechain channels before they reach `prepare`.
    pub ptr_count: usize,
}

impl<T: Vst3Sample> DirectionScratch<T> {
    /// Resolve one direction from its per-bus channel layout.
    ///
    /// `bus_channels` is the live per-bus channel count vec (empty == a single
    /// bus of `main_channels`); `main_channels` is bus 0's channel layout, used
    /// as the fallback and to size the per-bus scratch. `block_size` sizes the
    /// aux silence/sink block.
    ///
    /// `bus_channels` stays a raw `&[usize]` slice: it originates as per-bus
    /// host counts (`PluginInfo::input_bus_channels` / the read-back
    /// `SpeakerArrangement` popcounts), never as `ChannelLayout` values.
    pub fn resolve(
        bus_channels: &[usize],
        main_channels: ChannelLayout,
        block_size: usize,
    ) -> Self {
        let total: usize = if bus_channels.is_empty() {
            main_channels.count() as usize
        } else {
            bus_channels.iter().sum()
        };
        Self {
            buses: BusBuffers::new(bus_channels, main_channels, block_size),
            ptr_count: total.max(MIN_PTR_COUNT),
        }
    }
}

/// Build an `AudioBusBuffers` from a channel count, leaving the channel-pointer
/// union member null (zeroed) — refreshed every `prepare` via
/// [`Vst3Sample::set_channel_buffers`].
fn make_audio_bus(num_channels: ChannelLayout) -> vst3::Steinberg::Vst::AudioBusBuffers {
    let mut bus: vst3::Steinberg::Vst::AudioBusBuffers = unsafe { std::mem::zeroed() };
    bus.numChannels = num_channels.count() as i32;
    bus.silenceFlags = 0;
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
/// Parameterized by the committed sample format `T`, which decides the
/// `channelBuffers32`/`64` union member written for every bus (see
/// [`set_channel_buffers`]). The two members alias the same machine pointer, so
/// `T` only selects *which* member the code names — not the bytes stored.
///
/// The `aux` block stays `Vec<f64>` regardless of `T`: it's the wider of the two
/// sample widths, and an all-zero bit pattern is `0.0` for both `f32` and `f64`,
/// so one zeroed block reads as silence whichever format the plugin uses.
///
/// Allocated once in
/// [`Vst3Instance::from_loaded`](super::instance::Vst3Instance) and reused so
/// the realtime `process` path is allocation-free. The flat caller buffer
/// (bus 0) is mapped onto bus 0; every other bus is backed by `aux`.
pub(super) struct BusBuffers<T: Vst3Sample> {
    bus_channels: SmallVec<[usize; 4]>,
    bus_arrays: SmallVec<[vst3::Steinberg::Vst::AudioBusBuffers; 4]>,
    ptr_tables: SmallVec<[Vec<*mut std::ffi::c_void>; 4]>,
    aux: Vec<f64>,
    /// `T` governs the union member written but is not stored in any field.
    _format: PhantomData<T>,
}

// The pointer tables hold raw pointers into host-owned buffers that outlive
// each process call, mirroring `BufferPtrs`.
unsafe impl<T: Vst3Sample> Send for BusBuffers<T> {}
unsafe impl<T: Vst3Sample> Sync for BusBuffers<T> {}

impl<T: Vst3Sample> BusBuffers<T> {
    /// Pre-allocate per-bus pointer tables + the aux scratch block.
    ///
    /// `bus_channels` is the per-bus channel layout (empty == single bus of
    /// `fallback_channels`). `block_size` sizes the aux silence/sink block.
    ///
    /// `bus_channels` stays a raw `&[usize]` slice (per-bus host counts); only
    /// the single-scalar `fallback_channels` is a `ChannelLayout`.
    pub(super) fn new(
        bus_channels: &[usize],
        fallback_channels: ChannelLayout,
        block_size: usize,
    ) -> Self {
        let bus_channels: SmallVec<[usize; 4]> = if bus_channels.is_empty() {
            SmallVec::from_slice(&[fallback_channels.count() as usize])
        } else {
            SmallVec::from_slice(bus_channels)
        };
        let mut bus_arrays = SmallVec::new();
        let mut ptr_tables: SmallVec<[Vec<*mut std::ffi::c_void>; 4]> = SmallVec::new();
        for &ch in &bus_channels {
            let table = vec![std::ptr::null_mut::<std::ffi::c_void>(); ch.max(MIN_PTR_COUNT)];
            // channelBuffers pointer is left null (zeroed) and refreshed every
            // `prepare`, so a stale (about-to-move) Vec pointer is never read.
            bus_arrays.push(make_audio_bus(ChannelLayout::from(ch)));
            ptr_tables.push(table);
        }
        Self {
            bus_channels,
            bus_arrays,
            ptr_tables,
            aux: vec![0.0f64; block_size.max(1)],
            _format: PhantomData,
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
        // Input direction: aux is the silence source, so it must read as zero.
        // Output direction: aux is a discard sink, so its contents don't matter.
        if zero_aux {
            for s in self.aux.iter_mut() {
                *s = 0.0;
            }
        }
        let aux_ptr = self.aux.as_mut_ptr() as *mut std::ffi::c_void;

        // Deal the flat, bus-ordered `live` channels out one bus at a time. The
        // index advances *across* buses — bus 0 consumes its channels, then bus 1
        // continues from where bus 0 stopped — so it's threaded through each call.
        let mut next_live = 0usize;
        for bus_idx in 0..self.ptr_tables.len() {
            next_live = self.fill_bus(bus_idx, live, live_len, next_live, aux_ptr, zero_aux);
        }
        self.bus_arrays.as_mut_ptr()
    }

    /// Point one bus's channel table at the next slice of the flat `live` list,
    /// then stamp the bus header. Returns the advanced `live` index for the next
    /// bus to continue from.
    ///
    /// Each table slot is backed by either a supplied `live` channel or the
    /// shared `aux` block — never null. A slot falls back to `aux` when it is
    /// *not a real channel* (a [`MIN_PTR_COUNT`] padding slot past the bus's
    /// declared channel count) or there is *no live audio left* (the caller
    /// supplied fewer channels than the plugin advertises — e.g. an unconnected
    /// sidechain bus). For an input direction `aux` is zeroed silence; for an
    /// output direction it's a write-sink.
    ///
    /// When `silence_aux_channels` is set (input direction — `aux` is provably
    /// zero), each real channel that fell back to `aux` gets its `silenceFlags`
    /// bit set, so the plugin may skip processing it. Channels backed by a
    /// supplied `live` pointer are never flagged: we can't claim they're zero.
    /// The flag is left clear for the output direction, where `aux` is an
    /// unzeroed sink.
    ///
    /// # Safety
    /// Same contract as [`Self::prepare`]: `live` is a valid array of
    /// `live_len` channel pointers, and `next_live <= live_len`.
    unsafe fn fill_bus(
        &mut self,
        bus_idx: usize,
        live: *const *mut std::ffi::c_void,
        live_len: usize,
        mut next_live: usize,
        aux_ptr: *mut std::ffi::c_void,
        silence_aux_channels: bool,
    ) -> usize {
        let bus_ch = self.bus_channels[bus_idx];
        let table = &mut self.ptr_tables[bus_idx];

        // One bit per real channel; set when that channel is provably silent
        // (backed by the zeroed `aux` block on an input bus).
        let mut silence_flags: u64 = 0;
        for (slot_idx, slot) in table.iter_mut().enumerate() {
            let is_real_channel = slot_idx < bus_ch; // vs a MIN_PTR_COUNT padding slot
            let has_live_audio = next_live < live_len; // flat list not yet exhausted
            *slot = if is_real_channel && has_live_audio {
                let channel = *live.add(next_live);
                next_live += 1;
                channel
            } else {
                // A real channel with no live backing reads as zeroed `aux`, so
                // on an input bus it's safe to declare silent (bit < 64 always:
                // VST3 buses never exceed 64 channels).
                if is_real_channel && silence_aux_channels && slot_idx < 64 {
                    silence_flags |= 1 << slot_idx;
                }
                aux_ptr
            };
        }

        let bus = &mut self.bus_arrays[bus_idx];
        bus.numChannels = bus_ch as i32;
        bus.silenceFlags = silence_flags;
        T::set_channel_buffers(bus, table.as_mut_ptr());

        next_live
    }
}

#[cfg(test)]
mod tests {
    use super::{BusBuffers, MIN_PTR_COUNT};
    use std::ffi::c_void;
    use tutti_types::ChannelLayout;

    const BLOCK: usize = 64;

    /// Single-bus legacy: empty layout collapses to one bus of the fallback
    /// channel count, and bus 0 maps straight onto the live channels.
    #[test]
    fn empty_layout_is_single_bus() {
        let mut bb = BusBuffers::<f32>::new(&[], ChannelLayout::from(2u16), BLOCK);
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
        let mut bb = BusBuffers::<f32>::new(&[2, 1], ChannelLayout::from(2u16), BLOCK);
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

            // Bus 0 has real audio on both channels → no silence declared.
            assert_eq!(bus0.silenceFlags, 0);
            // Bus 1's single channel is aux-backed → declared silent (bit 0).
            assert_eq!(bus1.silenceFlags, 0b1);
        }
    }

    /// Silence flags are an input-only hint: the output direction's `aux` is an
    /// unzeroed discard sink, so an aux-backed output channel must NOT be
    /// declared silent (the plugin still writes real audio into it).
    #[test]
    fn output_direction_never_declares_silence() {
        // main = stereo out, plus an aux output bus the host doesn't drive.
        let mut bb = BusBuffers::<f32>::new(&[2, 2], ChannelLayout::from(2u16), BLOCK);
        let mut l = [0.0f32; BLOCK];
        let mut r = [0.0f32; BLOCK];
        let live = [l.as_mut_ptr() as *mut c_void, r.as_mut_ptr() as *mut c_void];
        unsafe {
            // zero_aux = false → output direction.
            let arrays = bb.prepare(live.as_ptr(), live.len(), false);
            assert_eq!((*arrays.add(0)).silenceFlags, 0);
            // The aux-backed second bus is a sink, not silence — still 0.
            assert_eq!((*arrays.add(1)).silenceFlags, 0);
        }
    }

    /// A fully-supplied input bus declares no silence; an entirely aux-backed
    /// input bus declares all its real channels silent (and only those, not the
    /// MIN_PTR_COUNT padding slots).
    #[test]
    fn fully_unconnected_input_bus_flags_only_real_channels() {
        // One stereo bus, but the caller supplies zero channels.
        let mut bb = BusBuffers::<f32>::new(&[2], ChannelLayout::from(2u16), BLOCK);
        let live: [*mut c_void; 0] = [];
        unsafe {
            let arrays = bb.prepare(live.as_ptr(), 0, true);
            // Both real channels aux-backed → bits 0 and 1 set, nothing above.
            assert_eq!((*arrays).silenceFlags, 0b11);
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
        let mut bb = BusBuffers::<f32>::new(&[2, 1], ChannelLayout::from(2u16), BLOCK);

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
        let mut bb = BusBuffers::<f32>::new(&[2, 1], ChannelLayout::from(2u16), BLOCK);
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
        let mut bb = BusBuffers::<f32>::new(&[1], ChannelLayout::from(1u16), BLOCK);
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

    /// An `f64` direction must write the `channelBuffers64` union member, not
    /// `channelBuffers32`. Both alias the same bytes, so reading either back
    /// yields the same pointer — this asserts the value landed and that the
    /// 64-bit member is the one we wrote through.
    #[test]
    fn f64_direction_writes_channel_buffers_64() {
        let mut bb = BusBuffers::<f64>::new(&[2], ChannelLayout::from(2u16), BLOCK);
        let mut l = [1.0f64; BLOCK];
        let mut r = [2.0f64; BLOCK];
        let live = [l.as_mut_ptr() as *mut c_void, r.as_mut_ptr() as *mut c_void];
        unsafe {
            let arrays = bb.prepare(live.as_ptr(), live.len(), true);
            let bus0 = &*arrays;
            // Read back through the 64-bit member.
            let p64 = bus0.__field0.channelBuffers64 as *const *mut f64;
            assert_eq!(*p64.add(0), l.as_mut_ptr());
            assert_eq!(*p64.add(1), r.as_mut_ptr());
            // The union aliases, so the 32-bit view sees the same machine
            // pointers (just relabeled) — confirms it's one slot, not two.
            let p32 = bus0.__field0.channelBuffers32 as *const *mut f64;
            assert_eq!(*p32.add(0), l.as_mut_ptr());
        }
    }
}
