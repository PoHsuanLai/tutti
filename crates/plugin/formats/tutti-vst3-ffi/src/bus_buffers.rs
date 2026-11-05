//! Pre-allocated per-bus FFI scratch that marshals the host's deinterleaved
//! channel slices into VST3's `AudioBusBuffers` array shape.
//!
//! The processor's scratch holds one [`BusBuffers`] per process direction and
//! binds it to each block's channel slices with [`BusBuffers::bind_inputs_f32`] (or its `f64`
//! twin) or [`BusBuffers::bind_outputs_f32`]; the realtime `process` path stays
//! allocation-free because every table here is sized once at construction.

use std::ffi::c_void;
use std::marker::PhantomData;

use vst3::Steinberg::Vst::AudioBusBuffers;

use tutti_types::ChannelLayout;

use crate::error::Vst3Error;
use crate::sdk::zeroed;
use crate::types::SampleWidth;

/// Minimum pointer-table width per bus. A defensive over-read of a stereo
/// table on a mono bus then stays in-bounds (the extra slot points at `aux`).
pub(crate) const MIN_PTR_COUNT: usize = 2;

fn make_audio_bus(num_channels: ChannelLayout) -> AudioBusBuffers {
    let mut bus: AudioBusBuffers = zeroed();
    bus.numChannels = num_channels.count() as i32;
    bus.silenceFlags = 0;
    bus
}

/// Per-bus `AudioBusBuffers` for one process direction.
///
/// Built for one [`SampleWidth`], so it writes the matching
/// `channelBuffers32`/`64` union member.
///
/// The `aux` block stays `Vec<f64>` regardless of `T`: it's the wider of the two
/// sample widths, and an all-zero bit pattern is `0.0` for both `f32` and `f64`,
/// so one zeroed block reads as silence whichever format the plugin uses.
///
/// Allocated once at activation and reused so the realtime `process` path is
/// allocation-free. The flat caller channel list carries every bus's channels
/// in bus order; any bus (or channel) the caller does not supply is backed by
/// `aux`.
pub struct BusBuffers {
    // Sized once here, never resized: boxed slices, since nothing pushes.
    bus_channels: Box<[usize]>,
    bus_arrays: Box<[AudioBusBuffers]>,
    ptr_tables: Box<[Vec<*mut c_void>]>,
    /// The flat list of the caller's channel pointers for the block being
    /// bound: `flat_width` slots, the per-direction channel total (main +
    /// sidechain/aux) clamped to [`MIN_PTR_COUNT`], so sizing to just the main
    /// bus would not truncate sidechain channels.
    live: Box<[*mut c_void]>,
    aux: Vec<f64>,
    /// Which union member the channel pointers are written through.
    width: SampleWidth,
}

impl std::fmt::Debug for BusBuffers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BusBuffers")
            .field("bus_channels", &self.bus_channels)
            .finish_non_exhaustive()
    }
}

// SAFETY: the tables hold raw pointers that are written by `bind_*` from slices
// the caller borrows for the block, and read only by the `process` call that
// takes the `BoundBuses` those borrows produced. Between blocks they are plain
// stale addresses nothing dereferences, and `aux` is owned. Moving the scratch
// to another thread therefore moves no live reference.
unsafe impl Send for BusBuffers {}

impl BusBuffers {
    /// Pre-allocates the per-bus pointer tables and the aux scratch block.
    ///
    /// `bus_channels` is the per-bus channel layout (empty == single bus of
    /// `fallback_channels`). `block_size` sizes the aux silence/sink block, and
    /// so the longest block [`AudioSide::process`](crate::AudioSide::process)
    /// accepts.
    ///
    /// `bus_channels` stays a raw `&[usize]` slice (per-bus host counts); only
    /// the single-scalar `fallback_channels` is a `ChannelLayout`.
    #[must_use]
    pub fn new(
        width: SampleWidth,
        bus_channels: &[usize],
        fallback_channels: ChannelLayout,
        block_size: usize,
    ) -> Self {
        let flat_width: usize = if bus_channels.is_empty() {
            fallback_channels.count() as usize
        } else {
            bus_channels.iter().sum()
        };
        let bus_channels: Box<[usize]> = if bus_channels.is_empty() {
            Box::new([fallback_channels.count() as usize])
        } else {
            bus_channels.into()
        };
        let mut bus_arrays = Vec::with_capacity(bus_channels.len());
        let mut ptr_tables = Vec::with_capacity(bus_channels.len());
        for &ch in &bus_channels {
            let table = vec![std::ptr::null_mut::<c_void>(); ch.max(MIN_PTR_COUNT)];
            bus_arrays.push(make_audio_bus(ChannelLayout::from(ch)));
            ptr_tables.push(table);
        }
        Self {
            bus_channels,
            bus_arrays: bus_arrays.into_boxed_slice(),
            ptr_tables: ptr_tables.into_boxed_slice(),
            live: vec![std::ptr::null_mut::<c_void>(); flat_width.max(MIN_PTR_COUNT)]
                .into_boxed_slice(),
            aux: vec![0.0f64; block_size.max(1)],
            width,
        }
    }

    /// How many buses this direction has.
    #[must_use]
    pub fn num_buses(&self) -> usize {
        self.bus_channels.len()
    }

    /// The flat width of the caller's channel list: how many channels of a
    /// block reach the buses.
    #[must_use]
    pub fn flat_width(&self) -> usize {
        self.live.len()
    }

    /// The longest block these buffers can back: the aux block's length.
    #[must_use]
    pub fn max_block(&self) -> usize {
        self.aux.len()
    }

    /// Binds a 32-bit input direction to this block's channel slices, laid
    /// out flat in bus order. Channels past [`flat_width`](Self::flat_width)
    /// are not bound; buses the caller does not supply are backed by zeroed
    /// silence and flagged silent.
    ///
    /// The ABI types the input channels `*mut`, so the plugin is handed
    /// pointers it could write through. Its contract is that it does not: an
    /// input is read-only, and processing in place means the caller supplying
    /// the same buffer as input and output, which a shared `&[T]` cannot be.
    /// A plugin that writes an input anyway writes into the caller's shared
    /// slice, which this crate cannot prevent.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::InvalidArgument`] if these buffers were built for the other
    /// [`SampleWidth`].
    pub fn bind_inputs_f32<'a>(
        &'a mut self,
        channels: &'a [&'a [f32]],
    ) -> Result<BoundBuses<'a>, Vst3Error> {
        self.expect_width(SampleWidth::F32)?;
        Ok(self.bind_in(channels))
    }

    /// As [`bind_inputs_f32`](Self::bind_inputs_f32) for 64-bit samples.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::InvalidArgument`] if these buffers were built for the other
    /// [`SampleWidth`].
    pub fn bind_inputs_f64<'a>(
        &'a mut self,
        channels: &'a [&'a [f64]],
    ) -> Result<BoundBuses<'a>, Vst3Error> {
        self.expect_width(SampleWidth::F64)?;
        Ok(self.bind_in(channels))
    }

    /// Binds a 32-bit output direction; buses the caller does not supply are
    /// backed by a sink the plugin may write to.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::InvalidArgument`] if these buffers were built for the other
    /// [`SampleWidth`].
    pub fn bind_outputs_f32<'a, 'b: 'a>(
        &'a mut self,
        channels: &'a mut [&'b mut [f32]],
    ) -> Result<BoundBuses<'a>, Vst3Error> {
        self.expect_width(SampleWidth::F32)?;
        Ok(self.bind_out(channels))
    }

    /// As [`bind_outputs_f32`](Self::bind_outputs_f32) for 64-bit samples.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::InvalidArgument`] if these buffers were built for the other
    /// [`SampleWidth`].
    pub fn bind_outputs_f64<'a, 'b: 'a>(
        &'a mut self,
        channels: &'a mut [&'b mut [f64]],
    ) -> Result<BoundBuses<'a>, Vst3Error> {
        self.expect_width(SampleWidth::F64)?;
        Ok(self.bind_out(channels))
    }

    fn expect_width(&self, width: SampleWidth) -> Result<(), Vst3Error> {
        if self.width == width {
            Ok(())
        } else {
            Err(Vst3Error::InvalidArgument)
        }
    }

    /// The width these buffers were built for.
    #[must_use]
    pub fn width(&self) -> SampleWidth {
        self.width
    }

    /// Binds the input direction to this block's channel slices, laid out flat
    /// in bus order. Channels past [`flat_width`](Self::flat_width) are not
    /// bound; buses the caller does not supply are backed by zeroed silence and
    /// flagged silent.
    fn bind_in<'a, T>(&'a mut self, channels: &'a [&'a [T]]) -> BoundBuses<'a> {
        let mut shortest = usize::MAX;
        let mut bound = 0;
        for (slot, channel) in self.live.iter_mut().zip(channels) {
            *slot = channel.as_ptr() as *mut c_void;
            shortest = shortest.min(channel.len());
            bound += 1;
        }
        BoundBuses {
            buffers: self,
            live_len: bound,
            shortest,
            zero_aux: true,
            _channels: PhantomData,
        }
    }

    /// Binds the output direction to this block's channel slices, laid out flat
    /// in bus order. Buses the caller does not supply are backed by a sink the
    /// plugin may write to.
    fn bind_out<'a, 'b: 'a, T>(&'a mut self, channels: &'a mut [&'b mut [T]]) -> BoundBuses<'a> {
        let mut shortest = usize::MAX;
        let mut bound = 0;
        for (slot, channel) in self.live.iter_mut().zip(channels.iter_mut()) {
            shortest = shortest.min(channel.len());
            *slot = channel.as_mut_ptr() as *mut c_void;
            bound += 1;
        }
        BoundBuses {
            buffers: self,
            live_len: bound,
            shortest,
            zero_aux: false,
            _channels: PhantomData,
        }
    }

    /// Refills the per-bus tables from `live_len` channel pointers and returns
    /// the `AudioBusBuffers` array.
    ///
    /// # Safety
    ///
    /// `live` points at `live_len` readable `*mut c_void` entries. The returned
    /// array points into `live`'s targets and into `self`: it is valid until
    /// either changes.
    pub(crate) unsafe fn prepare_raw(
        &mut self,
        live: *const *mut c_void,
        live_len: usize,
        zero_aux: bool,
    ) -> *mut AudioBusBuffers {
        if zero_aux {
            self.aux.fill(0.0);
        }
        let aux_ptr = self.aux.as_mut_ptr() as *mut c_void;

        let mut next_live = 0usize;
        for bus_idx in 0..self.ptr_tables.len() {
            // SAFETY: `live` has `live_len` entries, as the caller promises.
            next_live =
                unsafe { self.fill_bus(bus_idx, live, live_len, next_live, aux_ptr, zero_aux) };
        }
        self.bus_arrays.as_mut_ptr()
    }

    /// Fills bus `bus_idx`'s pointer table from the flat list starting at
    /// `next_live`, padding with `aux`, and returns where the next bus resumes.
    ///
    /// # Safety
    ///
    /// As [`prepare_raw`](Self::prepare_raw): `live` has `live_len` entries.
    unsafe fn fill_bus(
        &mut self,
        bus_idx: usize,
        live: *const *mut c_void,
        live_len: usize,
        mut next_live: usize,
        aux_ptr: *mut c_void,
        silence_aux_channels: bool,
    ) -> usize {
        let bus_ch = self.bus_channels[bus_idx];
        let table = &mut self.ptr_tables[bus_idx];

        let mut silence_flags: u64 = 0;
        for (slot_idx, slot) in table.iter_mut().enumerate() {
            let is_real_channel = slot_idx < bus_ch; // vs a MIN_PTR_COUNT padding slot
            let has_live_audio = next_live < live_len; // flat list not yet exhausted
            *slot = if is_real_channel && has_live_audio {
                // SAFETY: `next_live < live_len`, and `live` has `live_len`
                // entries by the caller's promise.
                let channel = unsafe { *live.add(next_live) };
                next_live += 1;
                channel
            } else {
                if is_real_channel && silence_aux_channels && slot_idx < 64 {
                    silence_flags |= 1 << slot_idx;
                }
                aux_ptr
            };
        }

        let bus = &mut self.bus_arrays[bus_idx];
        bus.numChannels = bus_ch as i32;
        bus.silenceFlags = silence_flags;
        self.width.set_channel_buffers(bus, table.as_mut_ptr());

        next_live
    }
}

/// One direction's buses bound to a block's channel slices, for one `process`
/// call.
///
/// The lifetime `'a` is the borrow of both the scratch and the slices the
/// pointers in it came from, so the pointers cannot outlive the audio they
/// point at, and a `process` call that was not given a fresh binding cannot
/// exist.
pub struct BoundBuses<'a> {
    buffers: &'a mut BusBuffers,
    live_len: usize,
    /// The shortest bound channel, `usize::MAX` with none.
    shortest: usize,
    zero_aux: bool,
    _channels: PhantomData<&'a mut ()>,
}

impl std::fmt::Debug for BoundBuses<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundBuses")
            .field("live_len", &self.live_len)
            .finish_non_exhaustive()
    }
}

impl BoundBuses<'_> {
    /// The width of the buses these are bound to.
    pub(crate) fn width(&self) -> SampleWidth {
        self.buffers.width
    }

    /// The longest block these buses can serve: the shortest bound channel and
    /// the aux block.
    pub(crate) fn capacity(&self) -> usize {
        self.shortest.min(self.buffers.max_block())
    }

    /// The channel count of each bound bus.
    pub(crate) fn bus_channels(&self) -> &[usize] {
        &self.buffers.bus_channels
    }

    /// The number of buses.
    pub(crate) fn num_buses(&self) -> usize {
        self.buffers.num_buses()
    }

    /// Fills the `AudioBusBuffers` array and returns it.
    pub(crate) fn finish(&mut self) -> *mut AudioBusBuffers {
        let live = self.buffers.live.as_ptr();
        // SAFETY: `live` is this scratch's own table, whose first `live_len`
        // slots `bind_*` filled from slices that `'a` keeps borrowed.
        unsafe { self.buffers.prepare_raw(live, self.live_len, self.zero_aux) }
    }
}

#[cfg(test)]
mod tests {
    use super::{BusBuffers, MIN_PTR_COUNT};
    use crate::types::SampleWidth;
    use std::ffi::c_void;
    use tutti_types::ChannelLayout;

    const BLOCK: usize = 64;

    /// Single-bus legacy: empty layout collapses to one bus of the fallback
    /// channel count, and bus 0 maps straight onto the live channels.
    #[test]
    fn empty_layout_is_single_bus() {
        let mut bb = BusBuffers::new(SampleWidth::F32, &[], ChannelLayout::from(2u16), BLOCK);
        assert_eq!(bb.num_buses(), 1);
        assert_eq!(bb.bus_channels[0], 2);

        let mut ch0 = [1.0f32; BLOCK];
        let mut ch1 = [2.0f32; BLOCK];
        let ch0_p = ch0.as_mut_ptr();
        let ch1_p = ch1.as_mut_ptr();
        let live = [ch0_p as *mut c_void, ch1_p as *mut c_void];
        // SAFETY: the test's tables and channel arrays are live locals, and every slot read was
        // filled by `prepare_raw` just above.
        unsafe {
            let arrays = bb.prepare_raw(live.as_ptr(), live.len(), true);
            let bus0 = &*arrays;
            assert_eq!(bus0.numChannels, 2);
            let ptrs = bus0.__field0.channelBuffers32 as *const *mut f32;
            assert_eq!(*ptrs.add(0), ch0_p);
            assert_eq!(*ptrs.add(1), ch1_p);
        }
    }

    /// Multi-bus input: bus 0 takes the live channels; the second (sidechain)
    /// bus is backed by the shared, zeroed aux block, never the live buffers.
    #[test]
    fn extra_input_bus_gets_silence() {
        // main = stereo, sidechain = mono.
        let mut bb = BusBuffers::new(SampleWidth::F32, &[2, 1], ChannelLayout::from(2u16), BLOCK);
        assert_eq!(bb.num_buses(), 2);

        let mut l = [5.0f32; BLOCK];
        let mut r = [6.0f32; BLOCK];
        let l_p = l.as_mut_ptr();
        let r_p = r.as_mut_ptr();
        let live = [l_p as *mut c_void, r_p as *mut c_void];
        // SAFETY: the test's tables and channel arrays are live locals, and every slot read was
        // filled by `prepare_raw` just above.
        unsafe {
            let arrays = bb.prepare_raw(live.as_ptr(), live.len(), true);
            let bus0 = &*arrays.add(0);
            let bus1 = &*arrays.add(1);
            assert_eq!(bus0.numChannels, 2);
            assert_eq!(bus1.numChannels, 1);

            let p0 = bus0.__field0.channelBuffers32 as *const *mut f32;
            assert_eq!(*p0.add(0), l_p);
            assert_eq!(*p0.add(1), r_p);

            // Sidechain channel points into the aux silence block, and its
            // sample reads as zero.
            let p1 = bus1.__field0.channelBuffers32 as *const *mut f32;
            let sc = *p1.add(0);
            assert!(sc != l_p && sc != r_p);
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
        let mut bb = BusBuffers::new(SampleWidth::F32, &[2, 2], ChannelLayout::from(2u16), BLOCK);
        let mut l = [0.0f32; BLOCK];
        let mut r = [0.0f32; BLOCK];
        let l_p = l.as_mut_ptr();
        let r_p = r.as_mut_ptr();
        let live = [l_p as *mut c_void, r_p as *mut c_void];
        // SAFETY: the test's tables and channel arrays are live locals, and every slot read was
        // filled by `prepare_raw` just above.
        unsafe {
            // zero_aux = false → output direction.
            let arrays = bb.prepare_raw(live.as_ptr(), live.len(), false);
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
        let mut bb = BusBuffers::new(SampleWidth::F32, &[2], ChannelLayout::from(2u16), BLOCK);
        let live: [*mut c_void; 0] = [];
        // SAFETY: the test's tables and channel arrays are live locals, and every slot read was
        // filled by `prepare_raw` just above.
        unsafe {
            let arrays = bb.prepare_raw(live.as_ptr(), 0, true);
            // Both real channels aux-backed → bits 0 and 1 set, nothing above.
            assert_eq!((*arrays).silenceFlags, 0b11);
        }
    }

    /// Multi-bus input WITH the sidechain channel supplied flat: bus 0 takes
    /// the first 2 live channels, bus 1 (sidechain) takes the 3rd. The flat
    /// caller buffer carries main + sidechain in bus order, so the running flat
    /// index hands bus 1 the real channel rather than aux silence.
    #[test]
    fn sidechain_bus_reads_supplied_channel() {
        // main = stereo, sidechain = mono.
        let mut bb = BusBuffers::new(SampleWidth::F32, &[2, 1], ChannelLayout::from(2u16), BLOCK);

        let mut l = [5.0f32; BLOCK];
        let mut r = [6.0f32; BLOCK];
        let mut sc = [7.0f32; BLOCK];
        let l_p = l.as_mut_ptr();
        let r_p = r.as_mut_ptr();
        let sc_p = sc.as_mut_ptr();
        let live = [l_p as *mut c_void, r_p as *mut c_void, sc_p as *mut c_void];
        // SAFETY: the test's tables and channel arrays are live locals, and every slot read was
        // filled by `prepare_raw` just above.
        unsafe {
            let arrays = bb.prepare_raw(live.as_ptr(), live.len(), true);
            let bus0 = &*arrays.add(0);
            let bus1 = &*arrays.add(1);

            let p0 = bus0.__field0.channelBuffers32 as *const *mut f32;
            assert_eq!(*p0.add(0), l_p);
            assert_eq!(*p0.add(1), r_p);

            // Sidechain channel now points at the supplied buffer, reading 7.0,
            // NOT the aux silence block.
            let p1 = bus1.__field0.channelBuffers32 as *const *mut f32;
            let scp = *p1.add(0);
            assert_eq!(scp, sc_p);
            assert_eq!(*scp, 7.0);
        }
    }

    /// More caller channels than table slots: only the filled slots reach
    /// the bus, and the surplus channel is never read.
    ///
    /// Mutation: in `bind_in`, drop the `zip` cap (index `live` by the channel
    /// number instead) -> the fourth channel indexes past the three-slot table
    /// and panics (miri reports the overrun), so the assertions below never
    /// hold.
    #[test]
    fn surplus_caller_channels_are_not_read_past_the_table() {
        let mut bb = BusBuffers::new(SampleWidth::F32, &[3], ChannelLayout::from(3u16), BLOCK);
        assert_eq!(bb.flat_width(), 3);
        let (a, b, c, d) = (
            [1.0f32; BLOCK],
            [2.0f32; BLOCK],
            [3.0f32; BLOCK],
            [4.0f32; BLOCK],
        );
        let channels: [&[f32]; 4] = [&a, &b, &c, &d];
        let mut bound = bb.bind_inputs_f32(&channels).unwrap();
        assert_eq!(bound.live_len, 3);
        let bus = bound.finish();
        // SAFETY: `bus` is bus 0 of the one-bus scratch; its table has 3 slots.
        unsafe {
            let chans = (*bus).__field0.channelBuffers32 as *const *mut f32;
            assert_eq!(*chans.add(0) as *const f32, a.as_ptr());
            assert_eq!(*chans.add(1) as *const f32, b.as_ptr());
            assert_eq!(*chans.add(2) as *const f32, c.as_ptr());
            assert_eq!((*bus).silenceFlags, 0);
        }
    }

    /// A bound direction's capacity is the shortest bound channel, so a block
    /// longer than any of its channels is refused before the plugin sees it.
    ///
    /// Mutation: have `capacity` return `self.buffers.max_block()` alone and
    /// the second assert fails.
    #[test]
    fn a_bound_direction_is_no_longer_than_its_shortest_channel() {
        let mut bb = BusBuffers::new(SampleWidth::F32, &[2], ChannelLayout::from(2u16), BLOCK);
        let (long, short) = ([0.0f32; BLOCK], [0.0f32; BLOCK / 2]);
        let channels: [&[f32]; 2] = [&long, &short];
        assert_eq!(bb.bind_inputs_f32(&channels).unwrap().capacity(), BLOCK / 2);
        let none: [&[f32]; 0] = [];
        assert_eq!(bb.bind_inputs_f32(&none).unwrap().capacity(), BLOCK);
    }

    /// The per-block `prepare_raw` must not allocate — it only refills pre-sized
    /// pointer tables and re-zeros the aux block.
    #[test]
    fn prepare_is_alloc_free() {
        let mut bb = BusBuffers::new(SampleWidth::F32, &[2, 1], ChannelLayout::from(2u16), BLOCK);
        let mut l = [0.5f32; BLOCK];
        let mut r = [0.5f32; BLOCK];
        let l_p = l.as_mut_ptr();
        let r_p = r.as_mut_ptr();
        let live = [l_p as *mut c_void, r_p as *mut c_void];
        // Warm up once outside the guard to mirror the RT discipline of the
        // real process loop.
        // SAFETY: the test's tables and channel arrays are live locals, and every slot read was
        // filled by `prepare_raw` just above.
        unsafe {
            let _ = bb.prepare_raw(live.as_ptr(), live.len(), true);
        }
        // SAFETY: the test's tables and channel arrays are live locals, and every slot read was
        // filled by `prepare_raw` just above.
        assert_no_alloc::assert_no_alloc(|| unsafe {
            for _ in 0..256 {
                let _ = bb.prepare_raw(live.as_ptr(), live.len(), true);
            }
        });
    }

    /// Output direction: extra buses get a sink (no zeroing required) and the
    /// padding slots (up to MIN_PTR_COUNT) stay non-null.
    #[test]
    fn output_padding_slots_are_non_null() {
        // Single mono output bus → table padded to MIN_PTR_COUNT.
        let mut bb = BusBuffers::new(SampleWidth::F32, &[1], ChannelLayout::from(1u16), BLOCK);
        assert!(bb.ptr_tables[0].len() >= MIN_PTR_COUNT);
        let mut m = [9.0f32; BLOCK];
        let m_p = m.as_mut_ptr();
        let live = [m_p as *mut c_void];
        // SAFETY: the test's tables and channel arrays are live locals, and every slot read was
        // filled by `prepare_raw` just above.
        unsafe {
            let arrays = bb.prepare_raw(live.as_ptr(), live.len(), false);
            let bus0 = &*arrays;
            let p = bus0.__field0.channelBuffers32 as *const *mut f32;
            assert_eq!(*p.add(0), m_p);
            // Padding slot is non-null (points at aux); a defensive over-read
            // stays in-bounds even though the plugin sees numChannels=1.
            assert!(!(*p.add(1)).is_null());
        }
    }

    /// An `f64` direction must write the `channelBuffers64` union member, not
    /// `channelBuffers32`. Both alias the same bytes, so reading either back
    /// yields the same pointer — this asserts the value landed and that the
    /// 64-bit member is the one written through.
    #[test]
    fn f64_direction_writes_channel_buffers_64() {
        let mut bb = BusBuffers::new(SampleWidth::F64, &[2], ChannelLayout::from(2u16), BLOCK);
        let mut l = [1.0f64; BLOCK];
        let mut r = [2.0f64; BLOCK];
        let l_p = l.as_mut_ptr();
        let r_p = r.as_mut_ptr();
        let live = [l_p as *mut c_void, r_p as *mut c_void];
        // SAFETY: the test's tables and channel arrays are live locals, and every slot read was
        // filled by `prepare_raw` just above.
        unsafe {
            let arrays = bb.prepare_raw(live.as_ptr(), live.len(), true);
            let bus0 = &*arrays;
            // Read back through the 64-bit member.
            let p64 = bus0.__field0.channelBuffers64 as *const *mut f64;
            assert_eq!(*p64.add(0), l_p);
            assert_eq!(*p64.add(1), r_p);
            // The union aliases, so the 32-bit view sees the same machine
            // pointers (just relabeled) — confirms it's one slot, not two.
            let p32 = bus0.__field0.channelBuffers32 as *const *mut f64;
            assert_eq!(*p32.add(0), l_p);
        }
    }
}
