//! One activation, both halves held together.

use tutti_plugin_types::{
    AudioBuffer, AudioBufferMut, Loaded, PluginError, PluginIo, Prepare, ProcessContext,
    ProcessOutput, Processor, RenderMode, Sample, SampleFormat, SampleRate, Samples,
};

/// The default rate.
pub const RATE: f64 = 48_000.0;
/// The default block size.
pub const BLOCK: usize = 64;
/// What an output holds before a plugin writes it, so a plugin or host that
/// leaves a channel unwritten shows.
const UNWRITTEN: f64 = 9.0;

/// A `Prepare` for realtime rendering.
#[must_use]
pub fn prepare(rate: f64, block: usize, format: SampleFormat) -> Prepare {
    Prepare {
        rate: SampleRate(rate),
        max_block: Samples(block),
        format,
        mode: RenderMode::Realtime,
    }
}

/// A sample width a case can render at.
pub trait Width: Sample + Into<f64> + From<f32> {
    /// The width's buffer variant.
    fn wrap<'t, 'd: 't>(buffer: AudioBuffer<'t, 'd, Self>) -> AudioBufferMut<'t, 'd>;
}

impl Width for f32 {
    fn wrap<'t, 'd: 't>(buffer: AudioBuffer<'t, 'd, Self>) -> AudioBufferMut<'t, 'd> {
        AudioBufferMut::F32(buffer)
    }
}

impl Width for f64 {
    fn wrap<'t, 'd: 't>(buffer: AudioBuffer<'t, 'd, Self>) -> AudioBufferMut<'t, 'd> {
        AudioBufferMut::F64(buffer)
    }
}

/// The two halves of one activation.
pub struct Active<L: Loaded> {
    pair: Option<(L::Processor, L::Controller)>,
    prepare: Prepare,
}

impl<L: Loaded> Active<L> {
    /// Activates `loaded`; a refusal hands the plugin back when the format can.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    pub fn activate(loaded: L, prepare: Prepare) -> Result<Self, (Option<L>, PluginError)> {
        match loaded.activate(prepare) {
            Ok(pair) => Ok(Self {
                pair: Some(pair),
                prepare,
            }),
            Err(refused) => Err((refused.plugin, refused.reason)),
        }
    }

    fn halves(&mut self) -> &mut (L::Processor, L::Controller) {
        self.pair.as_mut().expect("both halves")
    }

    /// Joins the halves back into the loaded plugin.
    ///
    /// # Panics
    ///
    /// When the plugin will not rejoin them.
    #[must_use]
    pub fn deactivate(mut self) -> L {
        let (processor, controller) = self.pair.take().expect("both halves");
        L::deactivate(processor, controller)
            .unwrap_or_else(|stuck| panic!("halves would not rejoin: {}", stuck.reason))
    }

    /// Deactivates and activates again with `wanted`.
    ///
    /// # Panics
    ///
    /// When the halves will not rejoin or the plugin refuses.
    pub fn reconfigure(&mut self, wanted: Prepare) {
        let pair = self.pair.take().expect("both halves");
        let loaded = L::deactivate(pair.0, pair.1)
            .unwrap_or_else(|stuck| panic!("halves would not rejoin: {}", stuck.reason));
        self.pair = Some(
            loaded
                .activate(wanted)
                .unwrap_or_else(|refused| panic!("reactivation refused: {}", refused.reason)),
        );
        self.prepare = wanted;
    }

    /// The main-thread half.
    pub fn controller(&mut self) -> &mut L::Controller {
        &mut self.halves().1
    }

    /// The realtime half.
    pub fn processor(&mut self) -> &mut L::Processor {
        &mut self.halves().0
    }

    /// One block; returns the two output channels.
    ///
    /// # Panics
    ///
    /// When the block does not render.
    pub fn render<S: Width>(
        &mut self,
        inputs: &[Vec<S>],
        frames: usize,
        ctx: &ProcessContext<'_>,
    ) -> Vec<Vec<S>> {
        let ins: Vec<&[S]> = inputs.iter().map(Vec::as_slice).collect();
        let mut outs = vec![vec![S::from(UNWRITTEN as f32); frames]; 2];
        let mut refs: Vec<&mut [S]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
        let mut out = ProcessOutput::default();
        let mut io = PluginIo {
            buffer: S::wrap(AudioBuffer::new(&ins, &mut refs, self.prepare.rate.0)),
            ctx,
            out: &mut out,
        };
        self.processor()
            .process(&mut io)
            .expect("the block renders");
        outs
    }

    /// A block of silence in, the outputs back.
    pub fn silent(&mut self, frames: usize) -> Vec<Vec<f32>> {
        self.render(
            &[vec![0.0; frames], vec![0.0; frames]],
            frames,
            &ProcessContext::new(),
        )
    }
}
