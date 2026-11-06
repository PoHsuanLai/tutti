//! How an [`OutputBlock`] is *run*.
//!
//! `tutti_io`'s recorder splits the same way: `PumpLoop` is one pass with no
//! thread and no clock, `PumpDriver` decides where that pass runs, and both of
//! its drivers call the same `pump_once`.
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
//! [`ManualStream::callback_once`] calls the same method, so a manual stream
//! runs exactly the callback CPAL runs.

use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, StreamTrait};
use tutti_core::{ChannelLayout, SampleRate, Samples};

use crate::MAX_FRAMES;
use crate::block::OutputBlock;
use crate::error::{Error, Result};
use crate::faults::StreamFaults;
use crate::owner::Owned;

/// The configuration an output stream is opened with: rate, width, sample
/// format and callback size.
///
/// [`AudioEngine`](crate::AudioEngine) reads it from the device's default
/// output config; build one with [`new`](Self::new) for a device-free engine
/// ([`AudioEngine::from_spec`](crate::AudioEngine::from_spec)).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct OutputSpec {
    /// Frames per second the stream runs at.
    pub sample_rate: SampleRate,
    /// The device's channel count; the graph root is folded to this width.
    pub channels: ChannelLayout,
    /// The device's native sample type. `I8`/`I16`/`I32`, `U8`/`U16`/`U32`,
    /// `F32` and `F64` are supported; any other fails to open with
    /// [`Error::InvalidConfig`].
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
    /// Creates a spec with no fixed callback size (the backend's default).
    #[must_use]
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

    /// Returns this spec with a fixed `quantum`-frame callback buffer.
    ///
    /// Keep `quantum` at or below [`MAX_FRAMES`]; a larger callback is
    /// clamped and its tail silenced.
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

/// Decides where an output stream's callback runs.
///
/// [`AudioEngine`](crate::AudioEngine) owns the spec, the fault record and the
/// stop, and hands a driver only the [`OutputBlock`] to run.
/// [`CpalDriver`] runs it on a real device; [`ManualStreamDriver`] lets the
/// caller run it by hand.
pub trait StreamDriver {
    /// The handle of the started stream; dropping it stops the stream.
    type Running: RunningStream;

    /// Opens and starts a stream at `spec`, running `block` once per callback
    /// and recording backend errors into `faults`.
    ///
    /// # Errors
    /// Whatever the driver cannot open; for [`CpalDriver`],
    /// [`Error::InvalidConfig`] for an unsupported sample format, or
    /// [`Error::BuildStream`] / [`Error::PlayStream`] from CPAL.
    fn open(
        self,
        spec: &OutputSpec,
        block: OutputBlock,
        faults: Arc<StreamFaults>,
    ) -> Result<Self::Running>;
}

/// The handle a [`StreamDriver`] hands back. Dropping it stops the stream.
// `Box<Self>` rather than `self`: `AudioEngine` stores this as a
// `dyn RunningStream`, and a by-value `self` cannot be called on a trait
// object.
pub trait RunningStream: Send {
    /// Stops the stream. Dropping the handle does the same; this makes the
    /// stop explicit at the call site.
    fn stop(self: Box<Self>);
}

// ------------------------------------------------------------- production --

/// The device driver: runs the callback on a real `cpal::Stream`.
///
/// Created internally by [`AudioEngine::start`](crate::AudioEngine::start) and
/// [`TuttiDriver::restart`](crate::TuttiDriver::restart) from the selected
/// device.
pub struct CpalDriver {
    device: cpal::Device,
}

impl std::fmt::Debug for CpalDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpalDriver").finish_non_exhaustive()
    }
}

impl CpalDriver {
    pub(crate) fn from_device(device: cpal::Device) -> Self {
        Self { device }
    }
}

/// The running handle of a [`CpalDriver`] stream. CPAL runs the callback for
/// as long as this value exists; dropping it stops the stream.
///
/// The `cpal::Stream` is created, played and dropped on a thread of its own
/// (some backends tie a stream to its creating thread); this handle is `Send`
/// and dropping it, from any thread, drops the stream there and waits for it.
#[derive(Debug)]
pub struct CpalStream(
    #[allow(dead_code, reason = "ownership is the API — held for Drop, never read")] Owned,
);

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
        let format = spec.sample_format;
        if !matches!(
            format,
            cpal::SampleFormat::I8
                | cpal::SampleFormat::I16
                | cpal::SampleFormat::I32
                | cpal::SampleFormat::U8
                | cpal::SampleFormat::U16
                | cpal::SampleFormat::U32
                | cpal::SampleFormat::F32
                | cpal::SampleFormat::F64
        ) {
            return Err(Error::InvalidConfig(format!(
                "Unsupported sample format: {format:?}"
            )));
        }
        Owned::spawn("tutti-output-stream", move || {
            let stream = match format {
                cpal::SampleFormat::I8 => self.build::<i8>(&config, block, faults)?,
                cpal::SampleFormat::I16 => self.build::<i16>(&config, block, faults)?,
                cpal::SampleFormat::I32 => self.build::<i32>(&config, block, faults)?,
                cpal::SampleFormat::U8 => self.build::<u8>(&config, block, faults)?,
                cpal::SampleFormat::U16 => self.build::<u16>(&config, block, faults)?,
                cpal::SampleFormat::U32 => self.build::<u32>(&config, block, faults)?,
                cpal::SampleFormat::F32 => self.build::<f32>(&config, block, faults)?,
                cpal::SampleFormat::F64 => self.build::<f64>(&config, block, faults)?,
                // Checked above, before the thread is spawned.
                _ => unreachable!("sample format checked before spawn"),
            };
            stream.play()?;
            Ok(stream)
        })
        .map(CpalStream)
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

/// A driver that runs no callbacks of its own: the caller runs them through
/// the paired [`ManualStream`].
///
/// For device-free tests of a host's lifecycle: pass it to
/// [`AudioEngine::start_with`](crate::AudioEngine::start_with) or
/// [`TuttiDriver::start_with`](crate::TuttiDriver::start_with) and render
/// blocks with [`ManualStream::render_block`].
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use tutti_core::{AudioTap, ChannelLayout, Engine, MasterMeter, SampleRate, Samples, Transport};
/// use tutti_cpal::{AudioCallbackState, AudioEngine, ManualStreamDriver, OutputSpec};
/// use tutti_graph::{Editor, Prepare};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let rate = SampleRate(48_000.0);
/// let transport = Transport::new(rate);
/// let (mut editor, executor) = Editor::new(Prepare::new(rate, Samples(512)));
/// let engine = Engine::new(&transport, &mut editor, executor)?;
/// let state = Arc::new(AudioCallbackState::new(engine, MasterMeter::new(), AudioTap::new()));
///
/// let spec = OutputSpec::new(rate, ChannelLayout::STEREO, tutti_cpal::cpal::SampleFormat::F32);
/// let mut audio = AudioEngine::from_spec(spec);
/// let (driver, stream) = ManualStreamDriver::new();
/// audio.start_with(state, driver)?;
///
/// let block = stream.render_block(256).expect("the stream is open");
/// assert_eq!(block.len(), 256 * 2);
///
/// audio.stop();
/// assert!(!stream.is_open());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct ManualStreamDriver {
    slot: Arc<Mutex<Option<OutputBlock>>>,
    faults: Arc<Mutex<Option<Arc<StreamFaults>>>>,
}

/// The caller's half of a [`ManualStreamDriver`], used to run callbacks by
/// hand.
///
/// Once the stream is stopped every method returns `None` (or `false`), which
/// is how a test observes the stop. Each call takes a mutex around the block;
/// this type is for tests and tools, not the audio thread.
#[derive(Debug)]
pub struct ManualStream {
    slot: Arc<Mutex<Option<OutputBlock>>>,
    faults: Arc<Mutex<Option<Arc<StreamFaults>>>>,
}

impl ManualStreamDriver {
    /// Creates a driver and the handle that runs its callbacks.
    #[allow(clippy::new_without_default, reason = "returns a pair, not Self")]
    #[must_use]
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

    /// Runs one callback, exactly as CPAL would, into a device buffer of the
    /// caller's sample type. Returns `None` if no stream is open.
    pub fn callback_once<T>(&self, data: &mut [T]) -> Option<()>
    where
        T: cpal::SizedSample + cpal::FromSample<f32>,
    {
        self.with_block(|b| b.render(data))
    }

    /// Renders `frames` frames at the stream's width and returns the
    /// interleaved `f32` buffer, or `None` if no stream is open. Allocates the
    /// returned buffer.
    pub fn render_block(&self, frames: usize) -> Option<Vec<f32>> {
        let channels = self.with_block(|b| b.channels())?;
        let mut out = vec![0.0f32; frames * channels];
        self.with_block(|b| b.render(&mut out))?;
        Some(out)
    }

    /// Returns the frames the last callback actually rendered, after the
    /// [`MAX_FRAMES`] clamp.
    pub fn last_rendered_frames(&self) -> Option<usize> {
        self.with_block(|b| b.last_rendered_frames())
    }

    /// Returns the channel count this stream was opened at.
    pub fn channels(&self) -> Option<usize> {
        self.with_block(|b| b.channels())
    }

    /// Delivers a backend error exactly as CPAL's error callback would, so a
    /// test can exercise the fault path without unplugging a device. Does
    /// nothing before the stream is first opened.
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

    /// Returns whether a stream is currently open on this handle.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.slot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }
}

/// The running handle of a [`ManualStreamDriver`] stream. Dropping it stops
/// the stream: every [`ManualStream`] call then returns `None`.
#[derive(Debug)]
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
mod quantum_tests;
