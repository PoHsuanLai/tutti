//! The audio-thread half of an instance: `process` and its buffers.

use std::cell::Cell;
use std::marker::PhantomData;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::clap_event_transport;
use clap_sys::process::{
    CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR, clap_process, clap_process_status,
};

use crate::events::{EventSink, EventSource, with_input, with_output};
use crate::plugin::Core;
use crate::pool::{PoolHandle, PoolRequest};

mod sealed {
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

/// A sample width CLAP carries: `f32` through `data32`, `f64` through
/// `data64`. Sealed: a third width has no field to travel in.
pub trait Sample: Copy + Default + Send + sealed::Sealed + 'static {
    /// A port buffer over `channel_count` channel pointers at `ptrs`.
    #[doc(hidden)]
    fn port_buffer(ptrs: *mut *mut Self, channel_count: u32) -> clap_audio_buffer;
}

impl Sample for f32 {
    fn port_buffer(ptrs: *mut *mut f32, channel_count: u32) -> clap_audio_buffer {
        clap_audio_buffer {
            data32: ptrs,
            data64: ptr::null_mut(),
            channel_count,
            latency: 0,
            constant_mask: 0,
        }
    }
}

impl Sample for f64 {
    fn port_buffer(ptrs: *mut *mut f64, channel_count: u32) -> clap_audio_buffer {
        clap_audio_buffer {
            data32: ptr::null_mut(),
            data64: ptrs,
            channel_count,
            latency: 0,
            constant_mask: 0,
        }
    }
}

/// Why a block did not run, or ran and failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessError {
    /// The instance was destroyed (its `Plugin` dropped); nothing was called.
    Retired,
    /// `start_processing` answered `false` (or the plugin refused it).
    StartFailed,
    /// A channel slice is shorter than the block; nothing was called.
    BufferTooShort,
    /// The block is longer than the plugin was activated for, or than the
    /// scratch was sized for; nothing was called.
    BlockTooLarge,
    /// The port layout in the call needs more channel pointers or port
    /// buffers than the scratch was sized for (`resize_for`); nothing was
    /// called, and nothing allocated.
    LayoutMismatch,
    /// The plugin returned `CLAP_PROCESS_ERROR`; its outputs were zeroed.
    PluginReturnedError,
}

/// The channel-pointer tables of a port layout, and the silent channels that
/// stand in for any the caller does not supply.
///
/// Sized once off the audio thread ([`resize_for`](Self::resize_for)) so that
/// `process` allocates nothing. The pointers it holds are rewritten at the
/// start of every block from that block's slices and read only inside the same
/// `process` call, so none outlives the borrow it came from.
#[derive(Debug)]
pub struct PortScratch<T: Sample> {
    pad: Vec<Vec<T>>,
    input_ptrs: Vec<*mut T>,
    output_ptrs: Vec<*mut T>,
    input_bufs: Vec<clap_audio_buffer>,
    output_bufs: Vec<clap_audio_buffer>,
    /// What `resize_for` sized the tables for: channels and ports per side.
    sized: Capacity,
}

#[derive(Debug, Default, Clone, Copy)]
struct Capacity {
    in_channels: usize,
    out_channels: usize,
    in_ports: usize,
    out_ports: usize,
}

// SAFETY: the raw pointers are scratch rewritten and read inside one `process`
// call and never dereferenced across calls or threads; the rest is `Vec`s of
// `T: Send`.
unsafe impl<T: Sample> Send for PortScratch<T> {}

impl<T: Sample> Default for PortScratch<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Sample> PortScratch<T> {
    /// An empty scratch; [`resize_for`](Self::resize_for) before use.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pad: Vec::new(),
            input_ptrs: Vec::new(),
            output_ptrs: Vec::new(),
            input_bufs: Vec::new(),
            output_bufs: Vec::new(),
            sized: Capacity::default(),
        }
    }

    /// Sizes the tables for ports of `inputs` / `outputs` channels each and
    /// blocks up to `max_frames`. Allocates; off the audio thread.
    pub fn resize_for(&mut self, inputs: &[u32], outputs: &[u32], max_frames: usize) {
        let in_total: usize = inputs.iter().map(|&c| c as usize).sum();
        let out_total: usize = outputs.iter().map(|&c| c as usize).sum();
        self.pad.clear();
        self.pad.reserve_exact(in_total + out_total);
        for _ in 0..in_total + out_total {
            self.pad.push(vec![T::default(); max_frames]);
        }
        self.input_ptrs.clear();
        self.input_ptrs.reserve_exact(in_total);
        self.output_ptrs.clear();
        self.output_ptrs.reserve_exact(out_total);
        self.input_bufs.clear();
        self.input_bufs.reserve_exact(inputs.len());
        self.output_bufs.clear();
        self.output_bufs.reserve_exact(outputs.len());
        self.sized = Capacity {
            in_channels: in_total,
            out_channels: out_total,
            in_ports: inputs.len(),
            out_ports: outputs.len(),
        };
    }

    /// Points the tables at this block's channels, padding the missing ones
    /// with silence. `None` when a channel slice is shorter than the block or
    /// the pad cannot cover the layout.
    fn bind(
        &mut self,
        inputs: &[&[T]],
        outputs: &mut [&mut [T]],
        input_ports: &[u32],
        output_ports: &[u32],
        frames: usize,
    ) -> Result<(), ProcessError> {
        let wanted_in: usize = input_ports.iter().map(|&c| c as usize).sum();
        let wanted_out: usize = output_ports.iter().map(|&c| c as usize).sum();
        let used_in = inputs.len().min(wanted_in);
        let used_out = outputs.len().min(wanted_out);
        if inputs[..used_in].iter().any(|s| s.len() < frames)
            || outputs[..used_out].iter().any(|s| s.len() < frames)
        {
            return Err(ProcessError::BufferTooShort);
        }
        let sized = self.sized;
        if wanted_in > sized.in_channels
            || wanted_out > sized.out_channels
            || input_ports.len() > sized.in_ports
            || output_ports.len() > sized.out_ports
        {
            return Err(ProcessError::LayoutMismatch);
        }
        let pad_needed = (wanted_in - used_in) + (wanted_out - used_out);
        if self.pad.len() < wanted_in + wanted_out
            || pad_needed > self.pad.len()
            || self.pad.iter().any(|c| c.len() < frames)
        {
            return Err(ProcessError::BlockTooLarge);
        }

        self.input_ptrs.clear();
        self.output_ptrs.clear();
        self.input_bufs.clear();
        self.output_bufs.clear();

        // Input channels. The plugin reads them; `*mut` is the C struct's
        // type, not a licence to write (`process.h`: inputs are const).
        self.input_ptrs
            .extend(inputs[..used_in].iter().map(|s| s.as_ptr().cast_mut()));
        let mut pad = self.pad.iter_mut();
        while self.input_ptrs.len() < wanted_in {
            let channel = pad.next().ok_or(ProcessError::BlockTooLarge)?;
            channel[..frames].fill(T::default());
            self.input_ptrs.push(channel.as_mut_ptr());
        }
        self.output_ptrs
            .extend(outputs[..used_out].iter_mut().map(|s| s.as_mut_ptr()));
        while self.output_ptrs.len() < wanted_out {
            let channel = pad.next().ok_or(ProcessError::BlockTooLarge)?;
            channel[..frames].fill(T::default());
            self.output_ptrs.push(channel.as_mut_ptr());
        }

        // The tables are final: take the base pointers only now, so no later
        // push can move them.
        let mut offset = 0usize;
        for &channels in input_ports {
            let base = self.input_ptrs.as_mut_ptr().wrapping_add(offset);
            self.input_bufs.push(T::port_buffer(base, channels));
            offset += channels as usize;
        }
        let mut offset = 0usize;
        for &channels in output_ports {
            let base = self.output_ptrs.as_mut_ptr().wrapping_add(offset);
            self.output_bufs.push(T::port_buffer(base, channels));
            offset += channels as usize;
        }
        Ok(())
    }
}

/// Everything one block hands the plugin.
pub struct ProcessCall<'a, 'b, T: Sample> {
    /// The channel tables.
    pub scratch: &'a mut PortScratch<T>,
    /// The caller's input channels.
    pub inputs: &'a [&'b [T]],
    /// The caller's output channels.
    pub outputs: &'a mut [&'b mut [T]],
    /// Channels of each input port.
    pub input_ports: &'a [u32],
    /// Channels of each output port.
    pub output_ports: &'a [u32],
    /// Frames in the block.
    pub frames: u32,
    /// The plugin's running frame counter.
    pub steady_time: i64,
    /// The transport, if any.
    pub transport: Option<&'a clap_event_transport>,
    /// Events for the plugin.
    pub in_events: &'a dyn EventSource,
    /// Where the plugin's events go.
    pub out_events: &'a mut dyn EventSink,
    /// A pool to serve `clap.thread-pool` requests on, if any.
    pub pool: Option<&'a PoolHandle>,
}

impl<T: Sample> std::fmt::Debug for ProcessCall<'_, '_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessCall")
            .field("frames", &self.frames)
            .field("input_ports", &self.input_ports)
            .field("output_ports", &self.output_ports)
            .finish_non_exhaustive()
    }
}

/// The audio-thread half of one activation: the entries `process` runs
/// through, each under the instance's audio claim.
///
/// Made by [`Plugin::audio_side`](crate::Plugin::audio_side). It names no
/// main-thread entry, so a processor holding it cannot make one.
pub struct AudioSide {
    core: Arc<Core>,
    _not_sync: PhantomData<Cell<()>>,
}

impl std::fmt::Debug for AudioSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSide")
            .field("processing", &self.is_processing())
            .finish_non_exhaustive()
    }
}

impl AudioSide {
    pub(crate) fn new(core: Arc<Core>) -> Self {
        Self {
            core,
            _not_sync: PhantomData,
        }
    }

    pub(crate) fn core(&self) -> &Arc<Core> {
        &self.core
    }

    /// Whether `start_processing` has run and `stop_processing` has not.
    #[must_use]
    pub fn is_processing(&self) -> bool {
        self.core.processing.load(Ordering::Relaxed)
    }

    /// Whether the instance can no longer be called: destroyed (its `Plugin`
    /// dropped) or deactivated. Read under the claim, it is exact.
    #[must_use]
    pub fn is_retired(&self) -> bool {
        self.core.retired.load(Ordering::Acquire) || !self.core.active.load(Ordering::Acquire)
    }

    /// `plugin.reset`, a no-op once retired.
    pub fn reset(&mut self) {
        let _claim = self.core.roles.claim();
        if self.is_retired() {
            return;
        }
        // SAFETY: the instance is alive (not retired, claim held: `Plugin`'s
        // drop cannot destroy it meanwhile) and audio-side entries are
        // serialised by the claim.
        unsafe {
            if let Some(reset) = (*self.core.raw()).reset {
                reset(self.core.raw());
            }
        }
    }

    /// `plugin.stop_processing` if processing; a no-op once retired.
    pub fn stop(&mut self) {
        let _claim = self.core.roles.claim();
        if self.is_retired() || !self.core.processing.swap(false, Ordering::Relaxed) {
            return;
        }
        // SAFETY: as `reset`; `stop_processing` follows the
        // `start_processing` the flag recorded.
        unsafe {
            if let Some(stop) = (*self.core.raw()).stop_processing {
                stop(self.core.raw());
            }
        }
    }

    /// Runs one block: starts processing if it is not, binds the buffers and
    /// calls `process`.
    ///
    /// Takes the audio claim for the whole call. A pool in `call` serves the
    /// plugin's `request_exec` calls for exactly the span of the plugin's
    /// `process`.
    ///
    /// # Errors
    /// See [`ProcessError`]; every error but `PluginReturnedError` leaves the
    /// plugin uncalled.
    pub fn process<T: Sample>(
        &mut self,
        call: ProcessCall<'_, '_, T>,
    ) -> Result<clap_process_status, ProcessError> {
        let ProcessCall {
            scratch,
            inputs,
            outputs,
            input_ports,
            output_ports,
            frames,
            steady_time,
            transport,
            in_events,
            out_events,
            pool,
        } = call;
        let _claim = self.core.roles.claim();
        if self.is_retired() {
            return Err(ProcessError::Retired);
        }
        if frames > self.core.max_frames.load(Ordering::Acquire) {
            return Err(ProcessError::BlockTooLarge);
        }
        scratch.bind(inputs, outputs, input_ports, output_ports, frames as usize)?;
        self.ensure_processing()?;

        let plugin = self.core.raw();
        // SAFETY: the instance is alive (above).
        let process_fn = unsafe { (*plugin).process };
        let status = match process_fn {
            None => CLAP_PROCESS_CONTINUE,
            Some(process_fn) => with_input(in_events, |in_raw| {
                with_output(out_events, |out_raw| {
                    let data = clap_process {
                        steady_time,
                        frames_count: frames,
                        transport: transport.map_or(ptr::null(), ptr::from_ref),
                        audio_inputs: scratch.input_bufs.as_ptr(),
                        audio_outputs: scratch.output_bufs.as_mut_ptr(),
                        audio_inputs_count: scratch.input_bufs.len() as u32,
                        audio_outputs_count: scratch.output_bufs.len() as u32,
                        in_events: in_raw,
                        out_events: out_raw,
                    };
                    // SAFETY: the plugin's own `process` with the instance and
                    // a `clap_process` whose every pointer is valid for the
                    // call: the buffer tables were bound just above to
                    // `frames`-long channels (checked) that `inputs`/`outputs`
                    // /the pad keep borrowed; the lists are the stack lists
                    // built here; the transport is borrowed from the caller.
                    // The claim is held, so this is the only audio-side call.
                    let run = || unsafe { process_fn(plugin, &data) };
                    // A handle made for another instance is ignored.
                    match pool.filter(|handle| handle.is_for(&self.core)) {
                        Some(handle) => {
                            PoolRequest::new(self.core.host_id(), handle, plugin).serve(run)
                        }
                        None => run(),
                    }
                })
            }),
        };

        if status == CLAP_PROCESS_ERROR {
            for channel in outputs.iter_mut() {
                let n = channel.len().min(frames as usize);
                channel[..n].fill(T::default());
            }
            return Err(ProcessError::PluginReturnedError);
        }
        Ok(status)
    }

    /// Starts processing if it has not been, under the claim `process` holds.
    fn ensure_processing(&self) -> Result<(), ProcessError> {
        if self.core.processing.load(Ordering::Relaxed) {
            return Ok(());
        }
        // SAFETY: the instance is alive and the claim is held (the caller).
        let start = unsafe { (*self.core.raw()).start_processing };
        if let Some(start) = start {
            // SAFETY: as above; `start_processing` precedes `process`.
            if !unsafe { start(self.core.raw()) } {
                return Err(ProcessError::StartFailed);
            }
        }
        self.core.processing.store(true, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RT regression: once the scratch has been sized at activation, `bind`
    /// only reuses capacity, with no heap grow.
    ///
    /// Mutation: have `bind` collect the pointers into a fresh `Vec`: the
    /// allocation gate panics.
    #[test]
    fn bind_is_allocation_free() {
        // main + sidechain stereo in, stereo out
        let input_ports = [2, 2];
        let output_ports = [2];
        let max_frames = 512usize;
        let mut scratch = PortScratch::<f32>::new();
        scratch.resize_for(&input_ports, &output_ports, max_frames);

        // The caller provides 2 input channels and 2 outputs.
        let in_a = [0.0f32; 512];
        let in_b = [0.0f32; 512];
        let mut out_a = [0.0f32; 512];
        let mut out_b = [0.0f32; 512];
        let inputs: [&[f32]; 2] = [&in_a, &in_b];

        // Warm up: the first call primes the pointer and buffer tables.
        let mut outputs: [&mut [f32]; 2] = [&mut out_a, &mut out_b];
        scratch
            .bind(
                &inputs,
                &mut outputs,
                &input_ports,
                &output_ports,
                max_frames,
            )
            .unwrap();

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..10_000 {
                scratch
                    .bind(
                        &inputs,
                        &mut outputs,
                        &input_ports,
                        &output_ports,
                        max_frames,
                    )
                    .unwrap();
            }
        });
    }

    /// A layout the scratch was not sized for is refused without allocating,
    /// even when the pad total alone would cover it: five input channels in
    /// a scratch sized for four would grow the pointer table on the audio
    /// thread.
    ///
    /// Mutation: drop the capacity check in `bind`: the input table grows and
    /// the allocation gate aborts the test.
    #[test]
    fn a_layout_the_scratch_was_not_sized_for_is_refused_without_allocating() {
        let mut scratch = PortScratch::<f32>::new();
        scratch.resize_for(&[2, 2], &[2], 512);
        let in_a = [0.0f32; 512];
        let in_b = [0.0f32; 512];
        let mut out_a = [0.0f32; 512];
        let mut out_b = [0.0f32; 512];
        let inputs: [&[f32]; 2] = [&in_a, &in_b];
        let mut outputs: [&mut [f32]; 2] = [&mut out_a, &mut out_b];
        assert_no_alloc::assert_no_alloc(|| {
            // 5 + 1 = 6 channels: within the pad total (6), past the input side (4).
            assert_eq!(
                scratch.bind(&inputs, &mut outputs, &[5], &[1], 512),
                Err(ProcessError::LayoutMismatch)
            );
            // Three ports where two were sized for.
            assert_eq!(
                scratch.bind(&inputs, &mut outputs, &[1, 1, 1], &[2], 512),
                Err(ProcessError::LayoutMismatch)
            );
        });
    }

    /// A caller that supplies fewer channels than the plugin expects (a mono
    /// caller on a stereo input) is padded from the preallocated pool: no
    /// allocation either, and the pad is silent.
    ///
    /// Mutation: skip the `fill` of a padded channel: the silence assertion
    /// fails once the pad holds a stale sample.
    #[test]
    fn bind_with_pad_is_allocation_free_and_silent() {
        let input_ports = [4];
        let output_ports = [2];
        let max_frames = 256usize;
        let mut scratch = PortScratch::<f32>::new();
        scratch.resize_for(&input_ports, &output_ports, max_frames);
        for channel in &mut scratch.pad {
            channel.fill(1.0);
        }

        // The caller only provides 1 input channel and 1 output channel.
        let in_ch = [0.5f32; 256];
        let mut out_ch = [0.0f32; 256];
        let inputs: [&[f32]; 1] = [&in_ch];
        let mut outputs: [&mut [f32]; 1] = [&mut out_ch];

        scratch
            .bind(
                &inputs,
                &mut outputs,
                &input_ports,
                &output_ports,
                max_frames,
            )
            .unwrap();
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..1_000 {
                scratch
                    .bind(
                        &inputs,
                        &mut outputs,
                        &input_ports,
                        &output_ports,
                        max_frames,
                    )
                    .unwrap();
            }
        });
        // Three padded input channels and one padded output channel, all
        // silent after the bind that follows a dirty pool.
        assert_eq!(scratch.input_ptrs.len(), 4);
        assert_eq!(scratch.output_ptrs.len(), 2);
        assert!(
            scratch
                .pad
                .iter()
                .take(4)
                .all(|c| c.iter().all(|&s| s == 0.0))
        );
    }

    /// A channel shorter than the block is refused before any pointer is
    /// taken: the plugin would write past it.
    ///
    /// Mutation: drop the `s.len() < frames` check in `bind`: this returns
    /// `Ok` and the assertion fails.
    #[test]
    fn a_channel_shorter_than_the_block_is_refused() {
        let ports = [1];
        let mut scratch = PortScratch::<f32>::new();
        scratch.resize_for(&ports, &ports, 64);
        let short = [0.0f32; 32];
        let mut out = [0.0f32; 64];
        let inputs: [&[f32]; 1] = [&short];
        let mut outputs: [&mut [f32]; 1] = [&mut out];
        assert_eq!(
            scratch.bind(&inputs, &mut outputs, &ports, &ports, 64),
            Err(ProcessError::BufferTooShort)
        );
    }

    /// A block longer than the pad the scratch was sized for is refused when
    /// it needs a pad channel.
    ///
    /// Mutation: drop the `c.len() < frames` check: the pad is sliced past its
    /// length and this panics instead of returning the error.
    #[test]
    fn a_block_longer_than_the_scratch_is_refused() {
        let ports = [2];
        let mut scratch = PortScratch::<f32>::new();
        scratch.resize_for(&ports, &ports, 16);
        let mut outputs: [&mut [f32]; 0] = [];
        assert_eq!(
            scratch.bind(&[], &mut outputs, &ports, &ports, 64),
            Err(ProcessError::BlockTooLarge)
        );
    }
}
