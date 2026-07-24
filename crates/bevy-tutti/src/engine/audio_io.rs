//! CPAL audio I/O — callback state, RT entry point, and device stream management.
//!
//! The RT callback runs two steps per block: an optional MIDI *pre-block*
//! producer ([`MidiPreBlock`]) that delivers events into node inboxes, then the
//! graph render ([`Engine`]). Metering runs over the result.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::Arc;
use tutti_core::engine::Engine;
use tutti_core::metering::{meter_output, AudioTap, MasterMeter, MeteringContext};
use tutti_core::{ChannelLayout, ScopedNoDenormals};

#[cfg(feature = "midi")]
use tutti_midi_runtime::MidiPreBlock;

use crate::engine::error::{Error, Result};

/// Maximum frames per CPAL callback buffer. Pre-allocates the internal f32
/// buffer to this size to avoid allocation in the audio thread.
const MAX_FRAMES: usize = 8192;

/// State shared between the engine and the RT audio callback.
///
/// Holds the [`Engine`] and, under `midi`, the pre-block MIDI producer that runs
/// before each render. Both are RT-safe; the callback owns the ordering
/// (`pre_block.run` then `engine.process`).
pub(crate) struct AudioCallbackState {
    pub(crate) engine: Engine,
    /// The once-per-block MIDI producer, run before the graph render to deliver
    /// events into node inboxes. `None` when no MIDI subsystem is wired.
    #[cfg(feature = "midi")]
    pub(crate) pre_block: Option<MidiPreBlock>,
    pub(crate) meter: MasterMeter,
    pub(crate) tap: AudioTap,
}

impl AudioCallbackState {
    pub(crate) fn new(engine: Engine, meter: MasterMeter, tap: AudioTap) -> Self {
        Self {
            engine,
            #[cfg(feature = "midi")]
            pre_block: None,
            meter,
            tap,
        }
    }

    /// Install the pre-block MIDI producer (called once at engine build).
    #[cfg(feature = "midi")]
    pub(crate) fn with_pre_block(mut self, pre_block: MidiPreBlock) -> Self {
        self.pre_block = Some(pre_block);
        self
    }

    pub(crate) fn reset_owners(&self) {
        self.engine.reset_owners();
        #[cfg(feature = "midi")]
        if let Some(pre_block) = &self.pre_block {
            pre_block.reset_owners();
        }
    }
}

#[inline]
pub(crate) fn process_audio(state: &AudioCallbackState, output: &mut [f32]) {
    let _no_denormals = ScopedNoDenormals::new();
    let frames = output.len() / 2;
    // Pre-block MIDI: deliver this block's events into node inboxes before the
    // graph renders.
    #[cfg(feature = "midi")]
    if let Some(pre_block) = &state.pre_block {
        pre_block.run(frames);
    }
    state.engine.process(output, frames);
}

/// Holds a [`cpal::Stream`] to keep it alive. CPAL runs the audio callback
/// on a background thread for as long as this value exists; dropping it stops
/// the stream. The inner field is never read — ownership *is* the API.
struct StreamHandle(#[allow(dead_code)] cpal::Stream);

unsafe impl Send for StreamHandle {}

/// Owns the CPAL stream and device configuration. Private to the engine.
pub(crate) struct AudioEngine {
    sample_rate: f64,
    channels: ChannelLayout,
    is_running: bool,
    device_index: Option<usize>,
    _stream: Option<StreamHandle>,
}

impl AudioEngine {
    pub(crate) fn new(device_index: Option<usize>) -> Result<Self> {
        let device = get_device(device_index)?;
        let config = device.default_output_config()?;

        Ok(Self {
            sample_rate: f64::from(config.sample_rate().0),
            channels: ChannelLayout::from(usize::from(config.channels())),
            is_running: false,
            device_index,
            _stream: None,
        })
    }

    pub(crate) fn start(&mut self, state: Arc<AudioCallbackState>) -> Result<()> {
        if self.is_running {
            return Ok(());
        }

        let device = get_device(self.device_index)?;
        let config = device.default_output_config()?;

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

    pub(crate) fn stop(&mut self) {
        self._stream = None;
        self.is_running = false;
    }

    pub(crate) fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    pub(crate) fn channels(&self) -> ChannelLayout {
        self.channels
    }

    pub(crate) fn is_running(&self) -> bool {
        self.is_running
    }

    pub(crate) fn set_device(&mut self, index: Option<usize>) {
        self.device_index = index;
    }

    pub(crate) fn device_name(&self) -> Result<String> {
        Ok(get_device(self.device_index)?.name()?)
    }

    pub(crate) fn output_devices() -> Result<impl Iterator<Item = (usize, String)>> {
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
    let channels = usize::from(config.channels);

    // Pre-allocate the internal buffer to MAX_FRAMES stereo frames. We never
    // resize it at runtime: any over-sized CPAL callback is clamped below,
    // and the tail of `data` gets silence. This keeps the callback alloc-free.
    let mut buffer = vec![0.0f32; MAX_FRAMES * 2];
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

            let needed = frames * 2;
            let mix = &mut buffer[..needed];
            // Zero before rendering — the previous callback's contents are not
            // meaningful input for the graph.
            mix.fill(0.0);
            process_audio(&state, mix);

            meter_output(mix, frames, &state.meter, &state.tap, &mut metering_ctx);

            write_output(data, channels, mix, frames);
        },
        |_err| {},
        None,
    )?;

    Ok(stream)
}

#[inline]
fn write_output<T: cpal::SizedSample + cpal::FromSample<f32>>(
    data: &mut [T],
    channels: usize,
    output: &[f32],
    rendered_frames: usize,
) {
    let silence = T::from_sample(0.0);
    for (i, sample) in data.iter_mut().enumerate() {
        let frame = i / channels;
        let ch = i % channels;
        *sample = if frame < rendered_frames && ch < 2 {
            T::from_sample(output[frame * 2 + ch])
        } else {
            silence
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use tutti_core::engine::Engine;
    use tutti_core::{dsp::Net, MotionEvent, Transport, TransportClock};

    /// Build a minimal engine + transport pair for callback-level tests.
    /// Bypasses the engine builder — these tests exercise the RT callback
    /// in isolation, not the full engine.
    fn build_callback_state(sample_rate: f64) -> (Transport, AudioCallbackState) {
        let transport = Transport::new(sample_rate);

        let mut net = Net::new(0, 2);
        let clock = TransportClock::from_inputs(transport.clock_inputs(), sample_rate)
            .with_position_writeback(Arc::clone(&transport.settings.beat));
        net.push(Box::new(clock));
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
        process_audio(&state, &mut output);

        let expected_beat = 256.0 * (120.0 / 60.0) / 44100.0;
        let actual_beat = transport.settings.beat();
        assert!(
            (actual_beat - expected_beat).abs() < 1e-6,
            "expected {expected_beat}, got {actual_beat}"
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
        process_audio(&state, &mut output);

        let beat = transport.settings.beat();
        assert!(beat < 4.0, "expected beat wrapped below 4.0, got {beat}");
    }

    /// RT-safety regression: `process_audio` must not allocate on the
    /// audio thread. Backs the umbrella step of the RT-safety audit —
    /// the pre-allocated `MAX_FRAMES * 2` buffer and the clamped/silenced
    /// tail on over-sized callbacks should keep this allocation-free.
    #[test]
    fn process_audio_is_allocation_free() {
        let sample_rate = 48_000.0;
        let (transport, state) = build_callback_state(sample_rate);
        transport.settings.set_tempo(120.0);
        let _ = transport.motion.try_send(MotionEvent::Play);
        transport.motion.drain();

        // Warm up outside the no-alloc scope — first call primes any
        // internal state on the transport / clock.
        let mut output = vec![0.0f32; 1024 * 2];
        process_audio(&state, &mut output);

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..1_000 {
                process_audio(&state, &mut output);
            }
        });
    }
}
