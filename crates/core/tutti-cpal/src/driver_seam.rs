//! How an [`OutputBlock`] is *run* — the `PumpDriver` analogue for output.
//!
//! `tutti_io`'s recorder splits the same way and for the same reason:
//! [`PumpLoop`] is one pass with no thread and no clock, [`PumpDriver`]
//! decides where that pass runs, and `ManualDriver`'s doc makes the claim that
//! justifies the whole split — *"It is not a second implementation of the
//! loop"*, because both drivers call the same `pump_once`.
//!
//! | `tutti-io` | here |
//! |---|---|
//! | `PumpLoop` | [`OutputBlock`] |
//! | `PumpDriver` | [`StreamDriver`] |
//! | `ThreadDriver` | [`CpalDriver`] |
//! | `ManualDriver` / `ManualPump` | [`ManualStreamDriver`] / [`ManualStream`] |
//! | `RunningPump` | [`RunningStream`] |
//!
//! [`CpalDriver`]'s closure body is `move |data, _| block.render(data)`, and
//! [`ManualStream::callback_once`] calls the same method. Before the split,
//! that body contained the `MAX_FRAMES` clamp, the zero-fill, the metering
//! fold and the eight-way format conversion, and the only thing able to run
//! any of it was CPAL with a sound card open.
//!
//! [`PumpLoop`]: https://docs.rs/tutti-io
//! [`PumpDriver`]: https://docs.rs/tutti-io

use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, StreamTrait};
use tutti_core::{ChannelLayout, SampleRate, Samples};

use crate::block::OutputBlock;
use crate::error::{Error, Result};
use crate::faults::StreamFaults;
use crate::MAX_FRAMES;

/// Everything a stream needs, resolved from a device once.
///
/// `cpal::SampleFormat` is named directly rather than mirrored. This crate's
/// [`Error`] already carries `cpal::BuildStreamError` and friends in public
/// variants, so cpal is in the public surface either way, and a parallel
/// format enum would be a second copy of the eight-way matrix these types
/// exist to make testable.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct OutputSpec {
    pub sample_rate: SampleRate,
    pub channels: ChannelLayout,
    pub sample_format: cpal::SampleFormat,
    /// The frames every callback carries, when the stream is opened with a
    /// fixed buffer size (`None`: the backend's default, whose size is not
    /// known until it calls back). A graph is prepared with it
    /// (`tutti_graph::Prepare::with_quantum`), so a node that keeps work in
    /// step with the device — a hosted out-of-process plugin, which ships one
    /// callback's worth to its server per callback — can.
    pub quantum: Option<Samples>,
}

/// The callback size asked of a device that offers a range: 512 frames
/// (10.7 ms at 48 kHz), a common DAW default, clamped into what the device
/// supports.
pub const PREFERRED_QUANTUM: Samples = Samples(512);

impl OutputSpec {
    /// A spec with no device behind it — what a test constructs.
    pub fn new(
        sample_rate: SampleRate,
        channels: ChannelLayout,
        sample_format: cpal::SampleFormat,
    ) -> Self {
        Self {
            sample_rate,
            channels,
            sample_format,
            quantum: None,
        }
    }

    /// This spec, opening the stream with a fixed `quantum`-frame buffer.
    #[must_use]
    pub fn with_quantum(mut self, quantum: Samples) -> Self {
        self.quantum = Some(quantum);
        self
    }

    pub(crate) fn from_supported(c: &cpal::SupportedStreamConfig) -> Self {
        Self {
            sample_rate: SampleRate::from(c.sample_rate().0),
            channels: ChannelLayout::from(usize::from(c.channels()).max(1)),
            sample_format: c.sample_format(),
            quantum: quantum_for(c.buffer_size()),
        }
    }

    pub(crate) fn stream_config(&self) -> cpal::StreamConfig {
        cpal::StreamConfig {
            channels: self.channels.count(),
            sample_rate: cpal::SampleRate(self.sample_rate.get().round() as u32),
            buffer_size: match self.quantum {
                // `u32` at the cpal boundary; a quantum is at most `MAX_FRAMES`.
                Some(q) => cpal::BufferSize::Fixed(u32::try_from(q.get()).unwrap_or(u32::MAX)),
                None => cpal::BufferSize::Default,
            },
        }
    }
}

/// The fixed callback size to open a device with: [`PREFERRED_QUANTUM`]
/// clamped into the range it reports, and never past [`MAX_FRAMES`] (the
/// callback's own buffers). `None` for a device that reports no range, which
/// is opened with its default buffer.
fn quantum_for(range: &cpal::SupportedBufferSize) -> Option<Samples> {
    match *range {
        cpal::SupportedBufferSize::Range { min, max } => {
            let (min, max) = (min as usize, (max as usize).min(MAX_FRAMES));
            (min <= max).then(|| Samples(PREFERRED_QUANTUM.get().clamp(min.max(1), max)))
        }
        cpal::SupportedBufferSize::Unknown => None,
    }
}

/// How an [`OutputBlock`] is run.
///
/// [`AudioEngine`](crate::AudioEngine) owns the take — the spec, the fault
/// sink, the stop — and delegates only the execution. A driver decides where
/// the callback runs and nothing about when the stream ends.
pub trait StreamDriver {
    /// A running stream, stoppable.
    type Running: RunningStream;

    /// Open and start. The eight-way sample-format fan-out happens inside,
    /// once, against `spec.sample_format`.
    fn open(
        self,
        spec: &OutputSpec,
        block: OutputBlock,
        faults: Arc<StreamFaults>,
    ) -> Result<Self::Running>;
}

/// The handle a [`StreamDriver`] hands back. Dropping it stops the stream.
///
/// `Box<Self>` rather than `self` for the same erasure reason
/// `tutti_io::RunningPump::join` gives: [`AudioEngine`](crate::AudioEngine)
/// stores this as a `dyn RunningStream` so its own type does not carry the
/// driver, and a by-value `self` on a trait object is not something the
/// compiler can size.
pub trait RunningStream: Send {
    /// Stop, explicitly and once. Dropping does the same thing; this makes it
    /// a statement rather than a side effect.
    fn stop(self: Box<Self>);
}

// ------------------------------------------------------------- production --

/// The production driver: a real `cpal::Stream`.
pub struct CpalDriver {
    device: cpal::Device,
}

impl CpalDriver {
    pub(crate) fn from_device(device: cpal::Device) -> Self {
        Self { device }
    }
}

/// Holds a [`cpal::Stream`] to keep it alive. CPAL runs the callback for as
/// long as this value exists; dropping it stops the stream. The inner field is
/// never read — ownership *is* the API.
pub struct CpalStream(
    #[allow(dead_code, reason = "ownership is the API — held for Drop, never read")] cpal::Stream,
);

// SAFETY: the same assertion `output.rs`'s `StreamHandle` made before this
// module existed. A `cpal::Stream` is not `Send` because some backends tie it
// to the thread that created it; this crate only ever creates one on the
// control thread and only ever drops it there, and the handle is moved into
// `AudioEngine`, which lives on that thread.
unsafe impl Send for CpalStream {}

impl RunningStream for CpalStream {
    fn stop(self: Box<Self>) {
        // Dropping is the stop.
    }
}

impl StreamDriver for CpalDriver {
    type Running = CpalStream;

    fn open(
        self,
        spec: &OutputSpec,
        block: OutputBlock,
        faults: Arc<StreamFaults>,
    ) -> Result<CpalStream> {
        let config = spec.stream_config();
        let stream = match spec.sample_format {
            cpal::SampleFormat::I8 => self.build::<i8>(&config, block, faults)?,
            cpal::SampleFormat::I16 => self.build::<i16>(&config, block, faults)?,
            cpal::SampleFormat::I32 => self.build::<i32>(&config, block, faults)?,
            cpal::SampleFormat::U8 => self.build::<u8>(&config, block, faults)?,
            cpal::SampleFormat::U16 => self.build::<u16>(&config, block, faults)?,
            cpal::SampleFormat::U32 => self.build::<u32>(&config, block, faults)?,
            cpal::SampleFormat::F32 => self.build::<f32>(&config, block, faults)?,
            cpal::SampleFormat::F64 => self.build::<f64>(&config, block, faults)?,
            format => {
                return Err(Error::InvalidConfig(format!(
                    "Unsupported sample format: {format:?}"
                )))
            }
        };
        stream.play()?;
        Ok(CpalStream(stream))
    }
}

impl CpalDriver {
    fn build<T>(
        &self,
        config: &cpal::StreamConfig,
        mut block: OutputBlock,
        faults: Arc<StreamFaults>,
    ) -> Result<cpal::Stream>
    where
        T: cpal::SizedSample + cpal::FromSample<f32>,
    {
        let channels = block.channels();
        Ok(self.device.build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                // The CPAL contract: a callback no larger than MAX_FRAMES.
                // The assertion lives HERE, at the boundary CPAL crosses, and
                // not inside `render` — it is a claim about CPAL, not about
                // the block, and keeping it out is what lets a debug-build
                // test observe the clamp instead of panicking before it.
                debug_assert!(
                    data.len() / channels <= MAX_FRAMES,
                    "CPAL callback frames {} exceeds MAX_FRAMES {MAX_FRAMES}",
                    data.len() / channels
                );
                block.render(data);
            },
            move |err| faults.record(&err),
            None,
        )?)
    }
}

// ------------------------------------------------------------------ tests --

/// A driver that runs no callbacks of its own: the caller runs them.
///
/// Shipped public, not `#[cfg(test)]`, for the same reason
/// `tutti_io::ManualDriver` is: each integration-test binary compiles
/// separately and cannot see a crate-private fixture, and a downstream host
/// writing its own device-free tests needs this too.
pub struct ManualStreamDriver {
    slot: Arc<Mutex<Option<OutputBlock>>>,
    faults: Arc<Mutex<Option<Arc<StreamFaults>>>>,
}

/// The caller's half of a [`ManualStreamDriver`].
///
/// Held across the engine's life. Once the stream is stopped the block is
/// gone, and every call here answers `None` — which is itself the assertion
/// that the stop ran, rather than something inferred from silence.
pub struct ManualStream {
    slot: Arc<Mutex<Option<OutputBlock>>>,
    faults: Arc<Mutex<Option<Arc<StreamFaults>>>>,
}

impl ManualStreamDriver {
    /// A driver and the handle that drives it.
    #[allow(clippy::new_without_default, reason = "returns a pair, not Self")]
    pub fn new() -> (Self, ManualStream) {
        let slot = Arc::new(Mutex::new(None));
        let faults = Arc::new(Mutex::new(None));
        (
            Self {
                slot: Arc::clone(&slot),
                faults: Arc::clone(&faults),
            },
            ManualStream { slot, faults },
        )
    }
}

impl ManualStream {
    fn with_block<R>(&self, f: impl FnOnce(&mut OutputBlock) -> R) -> Option<R> {
        let mut guard = self.slot.lock().unwrap_or_else(|p| p.into_inner());
        guard.as_mut().map(f)
    }

    /// Run exactly one callback, exactly as CPAL would, into a device buffer
    /// of the caller's sample type. `None` once the stream has been stopped.
    pub fn callback_once<T>(&self, data: &mut [T]) -> Option<()>
    where
        T: cpal::SizedSample + cpal::FromSample<f32>,
    {
        self.with_block(|b| b.render(data))
    }

    /// `f32` convenience: render `frames` frames at the block's own width and
    /// hand the interleaved buffer back.
    pub fn render_block(&self, frames: usize) -> Option<Vec<f32>> {
        let channels = self.with_block(|b| b.channels())?;
        let mut out = vec![0.0f32; frames * channels];
        self.with_block(|b| b.render(&mut out))?;
        Some(out)
    }

    /// Frames the last callback actually rendered, after the clamp.
    pub fn last_rendered_frames(&self) -> Option<usize> {
        self.with_block(|b| b.last_rendered_frames())
    }

    /// The device width this stream was opened at.
    pub fn channels(&self) -> Option<usize> {
        self.with_block(|b| b.channels())
    }

    /// Deliver a backend error exactly as CPAL's error callback would.
    ///
    /// This is the only way to test the fault path without unplugging a real
    /// sound card mid-run.
    pub fn fail(&self, err: cpal::StreamError) {
        if let Some(faults) = self
            .faults
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            faults.record(&err);
        }
    }

    /// Whether a stream is currently open on this handle.
    pub fn is_open(&self) -> bool {
        self.slot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }
}

/// Dropping this stops the stream, by taking the block out of the shared slot.
pub struct ManualRunning {
    slot: Arc<Mutex<Option<OutputBlock>>>,
}

impl RunningStream for ManualRunning {
    fn stop(self: Box<Self>) {
        // Drop does it.
    }
}

impl Drop for ManualRunning {
    fn drop(&mut self) {
        *self.slot.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

impl StreamDriver for ManualStreamDriver {
    type Running = ManualRunning;

    fn open(
        self,
        _spec: &OutputSpec,
        block: OutputBlock,
        faults: Arc<StreamFaults>,
    ) -> Result<ManualRunning> {
        // The engine's sink reaches the handle here rather than through a
        // separate wiring call: `open` is the one place both the driver and
        // the sink are in scope, and a generic `start_with` cannot know it is
        // holding a manual driver.
        *self.faults.lock().unwrap_or_else(|p| p.into_inner()) = Some(faults);
        *self.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(block);
        Ok(ManualRunning { slot: self.slot })
    }
}

#[cfg(test)]
mod quantum_tests {
    use super::*;

    /// A device reporting a buffer range is opened with a fixed callback size:
    /// the preferred 512 when it fits, clamped to the range when not, and
    /// never past the callback's own buffers; a device reporting none gets
    /// its default buffer, and the spec says the size is unknown.
    ///
    /// Mutation: return `None` for a range → the opened stream's size is
    /// unknown and no graph can keep in step with it → fails.
    #[test]
    fn a_device_range_gives_a_fixed_quantum() {
        use cpal::SupportedBufferSize::{Range, Unknown};
        assert_eq!(
            quantum_for(&Range { min: 64, max: 4096 }),
            Some(Samples(512))
        );
        assert_eq!(
            quantum_for(&Range {
                min: 1024,
                max: 4096
            }),
            Some(Samples(1024))
        );
        assert_eq!(
            quantum_for(&Range { min: 16, max: 256 }),
            Some(Samples(256))
        );
        assert_eq!(quantum_for(&Unknown), None);
        let spec = OutputSpec::new(
            SampleRate(48_000.0),
            ChannelLayout::STEREO,
            cpal::SampleFormat::F32,
        )
        .with_quantum(Samples(480));
        assert!(matches!(
            spec.stream_config().buffer_size,
            cpal::BufferSize::Fixed(480)
        ));
        let spec = OutputSpec::new(
            SampleRate(48_000.0),
            ChannelLayout::STEREO,
            cpal::SampleFormat::F32,
        );
        assert!(matches!(
            spec.stream_config().buffer_size,
            cpal::BufferSize::Default
        ));
    }
}
