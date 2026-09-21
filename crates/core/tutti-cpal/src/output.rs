//! CPAL audio I/O — callback state, RT entry point, and device stream management.
//!
//! The RT callback runs two steps per block: an optional MIDI *pre-block*
//! producer ([`MidiPreBlock`]) that delivers events into node inboxes, then the
//! graph render ([`Engine`]). Metering runs over the result.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use tutti_core::Engine;
use tutti_core::{meter_output, AudioTap, MasterMeter, MeteringContext};
use tutti_core::{ChannelLayout, InterleavedMut, SampleRate, ScopedNoDenormals};

#[cfg(feature = "midi")]
use tutti_midi_runtime::{MidiPostBlock, MidiPreBlock};

use crate::error::{Error, Result};

/// Maximum frames per CPAL callback buffer.
///
/// The stream's mix buffer is sized to this once and never resized, so it is
/// also the largest block [`process_audio`] can be handed: a longer callback is
/// clamped and its tail silenced rather than reallocating on the audio thread.
pub const MAX_FRAMES: usize = 8192;

/// State shared between the engine and the RT audio callback.
///
/// Holds the [`Engine`] and, under `midi`, the two MIDI phases that bracket the
/// render. All are RT-safe; the callback owns the ordering
/// (`pre_block.run` → `engine.process` → `post_block.run`).
pub struct AudioCallbackState {
    pub(crate) engine: Engine,
    /// The once-per-block MIDI producer, run before the graph render to deliver
    /// events into node inboxes. `None` when no MIDI subsystem is wired.
    #[cfg(feature = "midi")]
    pub(crate) pre_block: Option<MidiPreBlock>,
    /// The once-per-block MIDI consumer, run after the graph render to fan out
    /// whatever the graph emitted. `None` when no MIDI subsystem is wired.
    ///
    /// Separate from `pre_block` rather than folded into it because the two run
    /// on opposite sides of `engine.process` — that ordering *is* the design
    /// (see [`MidiPostBlock`]), and a single object would hide it.
    #[cfg(feature = "midi")]
    pub(crate) post_block: Option<MidiPostBlock>,
    pub(crate) meter: MasterMeter,
    pub(crate) tap: AudioTap,
}

impl AudioCallbackState {
    /// Assemble the state a stream's callback reads, with no MIDI phases
    /// installed. Called once at engine build, on the control thread.
    pub fn new(engine: Engine, meter: MasterMeter, tap: AudioTap) -> Self {
        Self {
            engine,
            #[cfg(feature = "midi")]
            pre_block: None,
            #[cfg(feature = "midi")]
            post_block: None,
            meter,
            tap,
        }
    }

    /// Install the pre-block MIDI producer (called once at engine build).
    #[cfg(feature = "midi")]
    pub fn with_pre_block(mut self, pre_block: MidiPreBlock) -> Self {
        self.pre_block = Some(pre_block);
        self
    }

    /// Install the post-block MIDI consumer (called once at engine build).
    #[cfg(feature = "midi")]
    pub fn with_post_block(mut self, post_block: MidiPostBlock) -> Self {
        self.post_block = Some(post_block);
        self
    }

    /// Clear the RT processors' recorded owner thread-IDs.
    ///
    /// Control-thread only, and only while no stream is running: a restart
    /// moves the callback to a new CPAL thread, and the owner checks would
    /// otherwise flag the new thread as an intruder.
    pub fn reset_owners(&self) {
        self.engine.reset_owners();
        #[cfg(feature = "midi")]
        if let Some(pre_block) = &self.pre_block {
            pre_block.reset_owners();
        }
        #[cfg(feature = "midi")]
        if let Some(post_block) = &self.post_block {
            post_block.reset_owners();
        }
    }
}

/// Render one block into `output`, an interleaved device buffer that carries
/// its own width. The graph root is folded to that width (see
/// [`Engine::process`]).
///
/// # Real-time
///
/// **Runs on the audio thread. Must not allocate, lock, or block.** Every
/// buffer it touches is sized once at stream build to [`MAX_FRAMES`]; a longer
/// callback is clamped and its tail silenced rather than reallocating here.
/// `tests/rt_no_alloc.rs` gates this against a disabled allocator.
#[inline]
pub fn process_audio(state: &AudioCallbackState, output: &mut InterleavedMut<'_>) {
    let _no_denormals = ScopedNoDenormals::new();
    // The frame count is the buffer's, not a separate argument that could
    // disagree with it. `len()` is frames; the division by the stride happens
    // inside the type, once.
    #[cfg(feature = "midi")]
    let frames = output.len();
    // Pre-block MIDI: deliver this block's events into node inboxes before the
    // graph renders.
    #[cfg(feature = "midi")]
    if let Some(pre_block) = &state.pre_block {
        pre_block.run(frames);
    }
    state.engine.process(output);
    // Post-block MIDI: fan out whatever the graph emitted. Must run *after*
    // `process`, because that is what makes delivery independent of the order
    // the emitting nodes happened to be scheduled in — every consumer's poll
    // for this block has already happened, so an event always lands after it.
    #[cfg(feature = "midi")]
    if let Some(post_block) = &state.post_block {
        post_block.run();
    }
}

/// Holds a [`cpal::Stream`] to keep it alive. CPAL runs the audio callback
/// on a background thread for as long as this value exists; dropping it stops
/// the stream. The inner field is never read — ownership *is* the API.
struct StreamHandle(
    #[allow(dead_code, reason = "ownership is the API — held for Drop, never read")] cpal::Stream,
);

unsafe impl Send for StreamHandle {}

/// Owns the CPAL stream and the device configuration it was built from.
///
/// The lifecycle half of the device layer: [`TuttiDriver`](crate::TuttiDriver)
/// wraps one and is what a host normally holds. Every method here runs on the
/// control thread — none is callable from the RT callback.
pub struct AudioEngine {
    sample_rate: SampleRate,
    channels: ChannelLayout,
    is_running: bool,
    device_index: Option<usize>,
    _stream: Option<StreamHandle>,
}

// Hand-rolled: `StreamHandle` wraps a CPAL stream, which is not `Debug`.
// Reports the configuration a host would want in a log line; whether a stream
// object exists is covered by `is_running`.
impl std::fmt::Debug for AudioEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEngine")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("is_running", &self.is_running)
            .field("device_index", &self.device_index)
            .finish_non_exhaustive()
    }
}

impl AudioEngine {
    /// Open a device and read its default output config, without starting a
    /// stream. `None` selects the host's default device.
    ///
    /// # Errors
    /// Returns [`Error::InvalidDevice`] if `device_index` is out of range, or
    /// [`Error::DeviceNotAvailable`] if the device has no default output config.
    pub fn new(device_index: Option<usize>) -> Result<Self> {
        let device = get_device(device_index)?;
        let config = device.default_output_config()?;

        Ok(Self {
            sample_rate: SampleRate::from(config.sample_rate().0),
            channels: ChannelLayout::from(usize::from(config.channels())),
            is_running: false,
            device_index,
            _stream: None,
        })
    }

    /// Build a stream on the selected device and start it. A no-op if one is
    /// already running.
    ///
    /// Re-reads the device's config, so [`sample_rate`](Self::sample_rate) and
    /// [`channels`](Self::channels) describe the stream that is actually
    /// playing rather than whatever [`new`](Self::new) saw.
    ///
    /// # Errors
    /// Returns [`Error::InvalidDevice`] for a bad index,
    /// [`Error::DeviceNotAvailable`] if the config cannot be read,
    /// [`Error::InvalidConfig`] for a sample format the engine does not build,
    /// or [`Error::BuildStream`] / [`Error::PlayStream`] from CPAL.
    pub fn start(&mut self, state: Arc<AudioCallbackState>) -> Result<()> {
        if self.is_running {
            return Ok(());
        }

        let device = get_device(self.device_index)?;
        let config = device.default_output_config()?;

        // The reported layout is the layout of the stream about to be built,
        // not the one the constructor happened to see. `set_device` + `start`
        // (what `TuttiDriver::restart` does) reaches here with a different
        // device than `new` read, and `build_stream` derives its real layout
        // from this same `config`. Leaving these fields at their construction
        // values makes `channels()` / `sample_rate()` describe a device that is
        // no longer playing while the audio itself is correct — so a reader
        // sizing a buffer from `channels()` gets the old width with nothing to
        // warn it.
        self.sample_rate = SampleRate::from(config.sample_rate().0);
        self.channels = ChannelLayout::from(usize::from(config.channels()));

        let stream = match config.sample_format() {
            cpal::SampleFormat::I8 => build_stream::<i8>(&device, &config.into(), state)?,
            cpal::SampleFormat::I16 => build_stream::<i16>(&device, &config.into(), state)?,
            cpal::SampleFormat::I32 => build_stream::<i32>(&device, &config.into(), state)?,
            cpal::SampleFormat::U8 => build_stream::<u8>(&device, &config.into(), state)?,
            cpal::SampleFormat::U16 => build_stream::<u16>(&device, &config.into(), state)?,
            cpal::SampleFormat::U32 => build_stream::<u32>(&device, &config.into(), state)?,
            cpal::SampleFormat::F32 => build_stream::<f32>(&device, &config.into(), state)?,
            cpal::SampleFormat::F64 => build_stream::<f64>(&device, &config.into(), state)?,
            format => {
                return Err(Error::InvalidConfig(format!(
                    "Unsupported sample format: {format:?}"
                )));
            }
        };

        stream.play()?;
        self._stream = Some(StreamHandle(stream));
        self.is_running = true;

        Ok(())
    }

    /// Drop the stream, which stops the callback. Idempotent.
    ///
    /// Dropping is the stop: CPAL runs the callback for exactly as long as the
    /// stream value lives.
    pub fn stop(&mut self) {
        self._stream = None;
        self.is_running = false;
    }

    /// Rate of the running stream, or of the config read at construction if
    /// none has started.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Channel layout of the running stream, or of the config read at
    /// construction if none has started. This is the width
    /// [`process_audio`] is handed.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// Whether a stream is currently open and playing.
    pub fn is_running(&self) -> bool {
        self.is_running
    }

    /// Select the device the next [`start`](Self::start) opens. `None` means
    /// the host default. Does not disturb a running stream.
    pub fn set_device(&mut self, index: Option<usize>) {
        self.device_index = index;
    }

    /// The selected device's name, queried fresh from the host.
    ///
    /// # Errors
    /// Returns [`Error::InvalidDevice`] if the index no longer resolves, or
    /// [`Error::DeviceNameError`] if the host cannot name it.
    pub fn device_name(&self) -> Result<String> {
        Ok(get_device(self.device_index)?.name()?)
    }

    /// Enumerate output devices as `(index, name)` pairs. The index is
    /// positional in this enumeration — see [`DeviceInfo::index`](crate::DeviceInfo::index).
    ///
    /// # Errors
    /// Returns [`Error::DevicesError`] if the host cannot enumerate.
    pub fn output_devices() -> Result<impl Iterator<Item = (usize, String)>> {
        Ok(cpal::default_host()
            .output_devices()?
            .enumerate()
            .map(|(i, d)| (i, d.name().unwrap_or_default())))
    }
}

fn get_device(index: Option<usize>) -> Result<cpal::Device> {
    let host = cpal::default_host();

    match index {
        Some(i) => {
            let devices: Vec<_> = host.output_devices()?.collect();
            let count = devices.len();
            devices.into_iter().nth(i).ok_or_else(|| {
                Error::InvalidDevice(format!("Device index {i} out of range ({count} available)"))
            })
        }
        None => host
            .default_output_device()
            .ok_or_else(|| Error::InvalidDevice("No output device available".into())),
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    state: Arc<AudioCallbackState>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    // The device's true channel layout. The engine folds the graph root to this
    // width; a device wider than MAX_ROOT_CHANNELS (rare) simply gets silent
    // extra channels (the root renders ≤ 8 and `fold_frame` zero-fills the rest).
    let layout = ChannelLayout::from(usize::from(config.channels).max(1));
    // The interleave stride for both the mix buffer and the device buffer,
    // derived ONCE here — never inside the callback's per-frame loops.
    let channels = layout.count() as usize;

    // Pre-allocate the internal mix buffer to MAX_FRAMES at the device width,
    // plus a stereo scratch for metering. Sized once from the real device config;
    // never resized at runtime — an over-sized CPAL callback is clamped below and
    // the tail of `data` gets silence. This keeps the callback alloc-free.
    let mut buffer = vec![0.0f32; MAX_FRAMES * channels];
    let mut meter_buf = vec![0.0f32; MAX_FRAMES * 2];
    let mut metering_ctx = MeteringContext::new();

    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let raw_frames = data.len() / channels;
            // Clamp to MAX_FRAMES so we never allocate. If CPAL ever hands us a
            // larger buffer we process the head and write silence to the tail.
            let frames = raw_frames.min(MAX_FRAMES);
            debug_assert!(
                raw_frames <= MAX_FRAMES,
                "CPAL callback frames {raw_frames} exceeds MAX_FRAMES {MAX_FRAMES}"
            );

            let mix = &mut buffer[..frames * channels];
            // Zero before rendering — the previous callback's contents are not
            // meaningful input for the graph.
            mix.fill(0.0);
            let mut mix = InterleavedMut::new(mix, layout);
            process_audio(&state, &mut mix);
            // Back to a flat slice for the metering fold and the device write.
            // `samples()` is the escape hatch the type documents: both loops
            // below are per-frame and must index raw.
            let mix = mix.as_ref().samples();

            // Meter a STEREO fold of the device buffer — `meter_output` / the UI
            // waveform assume stereo, and a stereo monitor is meaningful at any
            // device width.
            //
            // The `Stereo` arm is a deliberate optimization, not divergent
            // logic: `fold_frame`'s 2-wide arm already passes a stereo frame
            // through unchanged, so this only replaces a per-frame call with one
            // bulk memcpy. Keep them in step — if the fold's stereo arm ever
            // stops being a passthrough, this branch has to go, not be patched.
            let meter = &mut meter_buf[..frames * 2];
            if layout == ChannelLayout::STEREO {
                meter.copy_from_slice(mix);
            } else {
                for (i, out) in meter.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let f = &mix[i * channels..i * channels + channels];
                    tutti_core::fold_frame(f, out);
                }
            }
            meter_output(meter, frames, &state.meter, &state.tap, &mut metering_ctx);

            write_output(data, channels, mix, frames);
        },
        |_err| {},
        None,
    )?;

    Ok(stream)
}

/// Convert the rendered f32 mix into the device's sample format.
///
/// `data` stays a bare `&mut [T]` and `channels` a bare `usize`: `T` is
/// `cpal::SizedSample` (i16, u32, f64, …), so `InterleavedMut` — which is
/// f32-only by construction — cannot describe the destination. Widening the
/// newtype over `T` would buy nothing here, because the only arithmetic in this
/// function is `i / channels`, and the width it needs is the *source's*, which
/// the caller already reads off the `InterleavedMut` it built. The vocabulary
/// stops at the format boundary, as it does at the C ABI and WIT boundaries.
#[inline]
fn write_output<T: cpal::SizedSample + cpal::FromSample<f32>>(
    data: &mut [T],
    channels: usize,
    output: &[f32],
    rendered_frames: usize,
) {
    // `output` is already `channels`-wide interleaved (the engine folded the
    // graph root to the device width). Copy every channel through; frames past
    // what we rendered (an over-sized CPAL callback) get silence.
    let silence = T::from_sample(0.0);
    for (i, sample) in data.iter_mut().enumerate() {
        let frame = i / channels;
        *sample = if frame < rendered_frames {
            T::from_sample(output[i])
        } else {
            silence
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use tutti_core::dsp::{lowpass_hz, sine_hz, Net};
    use tutti_core::Engine;
    use tutti_core::{Beat, BeatDuration, MotionEvent, Transport, TransportClock};

    /// Build an engine + transport pair whose graph actually renders.
    ///
    /// The clock alone leaves every output edge on `Port::Zero`, so
    /// `process_audio` folds silence and never walks a vertex — which would
    /// leave the transport assertions below reading a playhead nothing drove.
    /// A sine through a filter into the output gives the render path real
    /// vertices to evaluate, per-vertex buffers to gather, and a fold to the
    /// device width.
    ///
    /// `tests/rt_no_alloc.rs` mirrors this fixture, because the allocation
    /// gates need a `#[global_allocator]` that only a test binary root can
    /// declare.
    fn build_callback_state(sample_rate: f64) -> (Transport, AudioCallbackState) {
        let transport = Transport::new(sample_rate);

        let mut net = Net::new(0, 2);
        let clock = TransportClock::new(transport.clock_links(), sample_rate);
        net.push(Box::new(clock));

        let source = net.push(Box::new(sine_hz::<f32>(220.0)));
        let filter = net.push(Box::new(lowpass_hz::<f32>(2_000.0, 0.7)));
        net.connect(source, 0, filter, 0);
        // `pipe_output` fans the filter's single output across both device
        // channels, so every output edge is a real `Port::Local` rather than the
        // `Port::Zero` an unwired graph leaves behind.
        net.pipe_output(filter);

        let backend = net.backend();

        // Hold the net alive for the duration of the test via a leaked arc —
        // the backend borrows the engine via its inner NetBackend.
        let _keep_net_alive: &'static Mutex<Net> = Box::leak(Box::new(Mutex::new(net)));

        let engine = Engine::new(transport.motion.clone(), backend);
        let state = AudioCallbackState::new(engine, MasterMeter::new(), AudioTap::new());
        (transport, state)
    }

    #[test]
    fn test_transport_advances_with_graph() {
        let sample_rate = 44100.0;
        let (transport, state) = build_callback_state(sample_rate);

        transport.settings.set_beat(0.0);
        transport.settings.set_tempo(120.0);
        let _ = transport.motion.try_send(MotionEvent::Play);
        transport.motion.drain();

        let frames = 256;
        let mut output = vec![0.0f32; frames * 2];
        process_audio(
            &state,
            &mut InterleavedMut::new(&mut output, ChannelLayout::STEREO),
        );

        let expected_beat = Beat(256.0 * (120.0 / 60.0) / 44100.0);
        let actual_beat = transport.settings.beat();
        assert!(
            (actual_beat - expected_beat).abs() < BeatDuration(1e-6),
            "expected {expected_beat:?}, got {actual_beat:?}"
        );
    }

    #[test]
    fn test_transport_loop_wrapping() {
        let sample_rate = 44100.0;
        let (transport, state) = build_callback_state(sample_rate);

        transport.settings.set_tempo(120.0);
        transport.settings.loop_span.set_range(0.0, 4.0);
        transport.settings.loop_span.set_enabled(true);
        let _ = transport.motion.try_send(MotionEvent::Play);
        transport.motion.drain();
        transport.motion.seek.request(3.99);

        let frames = 1024;
        let mut output = vec![0.0f32; frames * 2];
        process_audio(
            &state,
            &mut InterleavedMut::new(&mut output, ChannelLayout::STEREO),
        );

        let beat = transport.settings.beat();
        assert!(
            beat < Beat(4.0),
            "expected beat wrapped below 4.0, got {beat:?}"
        );
    }
}
