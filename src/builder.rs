//! Builder for configuring and constructing a [`TuttiEngine`].

use crate::audio_io::{AudioCallbackState, AudioEngine};
use crate::core::{
    ClickNode, ClickSettings, MeteringHandle, MeteringManager, PdcManager, TransportClock,
    TransportHandle, TransportManager, TuttiNet,
};
use crate::engine::DefaultProcessor;
use crate::{Result, TuttiDriver, TuttiEngine, TuttiGraph};

use tutti_core::dsp::An;
use tutti_core::processor::GraphProcessor;
use tutti_core::Arc;

#[cfg(feature = "midi")]
use crate::midi::MidiIo;
#[cfg(feature = "midi")]
use tutti_core::midi::MidiProcessor;
#[cfg(feature = "midi")]
use tutti_midi_runtime::MidiBus;
#[cfg(feature = "midi")]
use tutti_midi_runtime::MidiRoutingTable;

#[cfg(feature = "sampler")]
use crate::sampler::Sampler;

/// Configures and constructs a [`TuttiEngine`].
///
/// Subsystems that require expensive OS initialization are opt-in at build
/// time: MIDI ports via [`Self::midi`]. Everything else is automatically
/// enabled when its Cargo feature is compiled.
///
/// The sample rate is dictated by the selected audio output device and
/// cannot be overridden through the builder. Read it back from
/// `engine.sample_rate` after [`Self::build`].
///
/// # Example
///
/// ```ignore
/// use tutti::prelude::*;
///
/// let engine = TuttiEngine::builder()
///     .outputs(2)
///     .build()?;
///
/// let TuttiEngine { mut graph, transport, .. } = engine;
/// let id = graph.add(sine_hz(440.0));
/// graph.pipe_output(id);
/// graph.commit();
/// transport.play();
/// ```
pub struct TuttiEngineBuilder {
    output_device: Option<usize>,
    inputs: usize,
    outputs: usize,

    #[cfg(feature = "midi")]
    enable_midi: bool,

    #[cfg(feature = "mpe")]
    mpe_mode: Option<tutti_midi_io::MpeMode>,
}

impl Default for TuttiEngineBuilder {
    fn default() -> Self {
        Self {
            output_device: None,
            inputs: 0,
            outputs: 2,

            #[cfg(feature = "midi")]
            enable_midi: false,

            #[cfg(feature = "mpe")]
            mpe_mode: None,
        }
    }
}

impl TuttiEngineBuilder {
    /// Selects the CPAL output device by index.
    ///
    /// Accepts a bare `usize`, `Some(usize)`, or `None`. `None` falls back to
    /// the system default output.
    ///
    /// Default: system default output.
    pub fn output_device(mut self, index: impl Into<Option<usize>>) -> Self {
        self.output_device = index.into();
        self
    }

    /// Sets the number of input channels exposed to the graph.
    ///
    /// Default: `0`.
    pub fn inputs(mut self, count: usize) -> Self {
        self.inputs = count;
        self
    }

    /// Sets the number of output channels exposed to the graph.
    ///
    /// Default: `2`.
    pub fn outputs(mut self, count: usize) -> Self {
        self.outputs = count;
        self
    }

    /// Enables hardware MIDI so the built [`TuttiEngine`] exposes `midi_io`.
    ///
    /// Opening OS MIDI ports is a runtime operation that can fail; errors
    /// surface from [`Self::build`] rather than from this setter.
    ///
    /// Default: disabled.
    #[cfg(feature = "midi")]
    pub fn midi(mut self) -> Self {
        self.enable_midi = true;
        self
    }

    /// Enables MPE with the given mode and turns on the MIDI subsystem.
    ///
    /// Default: MPE disabled.
    #[cfg(feature = "mpe")]
    pub fn mpe(mut self, mode: tutti_midi_io::MpeMode) -> Self {
        self.mpe_mode = Some(mode);
        self.enable_midi = true;
        self
    }

    /// Builds the [`TuttiEngine`], starting the audio stream.
    ///
    /// The sample rate is dictated by the audio device (not the builder), so
    /// read it back from the returned engine's `sample_rate` field. The
    /// returned struct is a flat bundle of public fields and is typically
    /// consumed by destructuring:
    ///
    /// ```ignore
    /// let TuttiEngine { mut graph, transport, metering, .. } = builder.build()?;
    /// ```
    ///
    /// # Example
    ///
    /// ```ignore
    /// use tutti::prelude::*;
    ///
    /// let engine = TuttiEngine::builder()
    ///     .outputs(2)
    ///     .build()?;
    ///
    /// let TuttiEngine { mut graph, transport, .. } = engine;
    /// let id = graph.add(sine_hz(440.0));
    /// graph.pipe_output(id);
    /// graph.commit();
    /// transport.play();
    /// ```
    pub fn build(self) -> Result<TuttiEngine> {
        // Build MIDI I/O first so we can hand its port manager to the processor.
        #[cfg(feature = "midi")]
        let midi_io = if self.enable_midi {
            let port_manager = Arc::new(tutti_midi_io::MidiPortManager::new(256));
            Some(MidiIo::new(port_manager))
        } else {
            None
        };

        let mut audio_engine = AudioEngine::new(self.output_device)?;
        let sample_rate = audio_engine.sample_rate();
        let channels = audio_engine.channels();

        let inputs = self.inputs;
        let outputs = if self.outputs == 0 { 2 } else { self.outputs };

        let transport_mgr = Arc::new(TransportManager::new(sample_rate));
        let metering_mgr = Arc::new(MeteringManager::new(sample_rate));
        let click_settings = Arc::new(ClickSettings::new());
        let pdc = PdcManager::new(outputs, 0);
        let pdc_snapshot = pdc.snapshot_arc();

        let mut net = TuttiNet::new(inputs, outputs);

        // Transport clock — infrastructure node writing back beat position via atomics.
        let clock = TransportClock::new(
            transport_mgr.tempo().clone(),
            transport_mgr.paused().clone(),
            sample_rate,
        )
        .with_seek(
            transport_mgr.seek_target().clone(),
            transport_mgr.seek_pending().clone(),
        )
        .with_loop(
            transport_mgr.loop_enabled_flag().clone(),
            transport_mgr.loop_start_beat_atomic().clone(),
            transport_mgr.loop_end_beat_atomic().clone(),
        )
        .with_position_writeback(transport_mgr.current_beat().clone());
        net.inner_mut().push(Box::new(clock));

        // Metronome — mixed into master output.
        let click_transport = TransportHandle::new(transport_mgr.clone(), click_settings.clone());
        let click = ClickNode::new(click_transport, click_settings.clone(), sample_rate);
        let click_id = net.inner_mut().push(Box::new(An(click)));
        net.inner_mut().pipe_output(click_id);

        let backend = net.backend();

        #[cfg(feature = "midi")]
        let midi_route = MidiRoutingTable::new();
        #[cfg(feature = "midi")]
        let midi_bus = MidiBus::new();

        let graph_processor = GraphProcessor::new(transport_mgr.clone(), backend);

        #[cfg(feature = "midi")]
        let processor: DefaultProcessor = {
            let mut midi_proc = MidiProcessor::new(graph_processor, midi_route.snapshot_arc());
            midi_proc.set_queue(Arc::new(midi_bus.clone()));

            if let Some(ref io) = midi_io {
                midi_proc.set_input(io.port_manager().clone());
            }

            midi_proc
        };

        #[cfg(not(feature = "midi"))]
        let processor: DefaultProcessor = graph_processor;

        let callback_state = Arc::new(AudioCallbackState::new(processor, metering_mgr.clone()));
        audio_engine.start(callback_state.clone())?;

        #[cfg(feature = "sampler")]
        let sampler = Arc::new(
            Sampler::builder(sample_rate)
                .pdc(pdc_snapshot.clone())
                .build()?,
        );
        // Silence the unused warning in the non-sampler config.
        #[cfg(not(feature = "sampler"))]
        let _ = &pdc_snapshot;

        #[cfg(feature = "soundfont")]
        let soundfont = Arc::new(crate::synth::SoundFontSystem::new(sample_rate as u32));

        let graph = TuttiGraph::from_parts(
            net,
            pdc,
            #[cfg(feature = "midi")]
            midi_route,
            sample_rate,
            channels,
        );

        let driver = TuttiDriver::from_parts(audio_engine, callback_state);

        let transport = TransportHandle::new(transport_mgr, click_settings);

        #[cfg(feature = "analysis")]
        let analysis = crate::analysis::AnalysisHandle::with_metering(
            sample_rate,
            metering_mgr.clone(),
        );

        let metering = MeteringHandle::new(metering_mgr);

        Ok(TuttiEngine {
            graph,
            driver,
            transport,
            metering,
            sample_rate,
            channels,
            #[cfg(feature = "midi")]
            midi: midi_bus,
            #[cfg(feature = "midi")]
            midi_io,
            #[cfg(feature = "sampler")]
            sampler,
            #[cfg(feature = "soundfont")]
            soundfont,
            #[cfg(feature = "analysis")]
            analysis,
        })
    }
}
