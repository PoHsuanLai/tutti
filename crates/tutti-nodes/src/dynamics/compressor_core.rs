//! The multiband's band compressor: the block-form compressor core.
//!
//! The compressor node is [`CompressorFamily`](tutti_dsp::CompressorFamily). Each band of
//! the [`MultibandCompressorNode`](super::MultibandCompressorNode) still runs this core
//! over [`CompressorKernel`]; it goes with the multiband's move onto the family model
//! (design 024 section 3, row 9).

use tutti_dsp::{CompressorBlock, CompressorKernel};
use tutti_types::{CompressionRatio, Db, Param, SampleRate, Seconds};

use super::params::{AttackRelease, ThresholdParams};
use crate::ramp::{LastGood, clamp_finite};

/// Shared compressor state used by the per-sample gain computation.
#[derive(Clone, Debug)]
pub(super) struct CompressorCore {
    pub threshold: ThresholdParams,
    pub ratio: Param<CompressionRatio>,
    pub timing: AttackRelease,
    pub makeup_db: Param<Db>,

    kernel: CompressorKernel,
    /// The makeup gain the previous block ended on, the start of this block's
    /// ramp. `None` until the first block, which then starts on its own value
    /// — a node has no "previous" makeup to ramp in from.
    last_makeup: Option<Db>,
    /// Last finite threshold, knee, ratio, makeup, attack and release. Every
    /// one of them reaches the envelope follower — a recursive state that a
    /// single NaN or ±∞ leaves NaN for good — so a non-finite write reads as
    /// unchanged (see [`LastGood`]).
    good: [LastGood; 6],
}

impl CompressorCore {
    /// Builds the shared core. The follower is seeded at the placeholder
    /// [`SampleRate::DEFAULT`]; `CompressorNode`'s `prepare` retunes it
    /// from `timing`, which is why the times are kept as [`Seconds`] rather
    /// than only as coefficients — the seconds are the recoverable form, the
    /// coefficients are not.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_types::SampleRate::DEFAULT
    pub fn new(
        threshold_db: impl Into<Db>,
        ratio: impl Into<CompressionRatio>,
        attack: impl Into<Seconds>,
        release: impl Into<Seconds>,
    ) -> Self {
        let threshold_db = threshold_db.into();
        let ratio = CompressionRatio::new_clamped(ratio.into().get());
        let attack = attack.into();
        let release = release.into();
        Self {
            threshold: ThresholdParams::new(threshold_db, Db(0.0)),
            ratio: Param::new(ratio),
            timing: AttackRelease::new(attack, release),
            makeup_db: Param::new(Db(0.0)),
            kernel: CompressorKernel::new(attack, release, SampleRate::DEFAULT),
            last_makeup: None,
            good: [
                LastGood::new(threshold_db.get(), 0.0),
                LastGood::fixed(0.0),
                LastGood::new(ratio.get(), CompressionRatio::UNITY.get()),
                LastGood::fixed(0.0),
                LastGood::new(attack.to_f32(), 0.0),
                LastGood::new(release.to_f32(), 0.0),
            ],
        }
    }

    pub fn gain_reduction_db(&self) -> Db {
        self.kernel.gain_reduction_db()
    }

    /// Detaches every control cell (see [`Param::detach`]): the `fork_fresh`
    /// half of the core, keeping the current values. A cell added to the core
    /// is detached here, once, for every node that forks it.
    pub fn detach(&mut self) {
        self.threshold.detach();
        self.ratio.detach();
        self.timing.detach();
        self.makeup_db.detach();
    }

    pub fn reset(&mut self) {
        self.kernel.reset();
        // Ramp history is state like the envelope: a reset node starts its
        // next block on the current makeup rather than ramping in from a
        // value it held before the discontinuity.
        self.last_makeup = None;
    }

    pub fn set_sample_rate(&mut self, sample_rate: impl Into<tutti_types::SampleRate>) {
        let (attack, release) = self.times([None, None]);
        self.kernel.set_sample_rate(sample_rate, attack, release);
    }

    /// `driven` is the block's attack and release from the node's param
    /// ports where a lane drives them; `None` reads the cell.
    #[inline]
    pub fn update_coefficients(&mut self, driven: [Option<f32>; 2]) {
        let (attack, release) = self.times(driven);
        self.kernel.update_coefficients(attack, release);
    }

    /// Attack and release, with non-finite writes held off, floored as
    /// the setters floor them ([`AttackRelease::clamp_time`]), driven or
    /// not.
    #[inline]
    fn times(&mut self, [attack, release]: [Option<f32>; 2]) -> (Seconds, Seconds) {
        (
            Seconds::from_f32(AttackRelease::clamp_time(
                self.good[4].read(attack.unwrap_or_else(|| self.timing.attack.load().to_f32())),
            )),
            Seconds::from_f32(AttackRelease::clamp_time(
                self.good[5].read(release.unwrap_or_else(|| self.timing.release.load().to_f32())),
            )),
        )
    }

    /// Reads every block-rate control once, and moves the makeup ramp's start
    /// to this block's end. `driven` is the block's knee, ratio and makeup
    /// from the node's param ports where a lane drives them; `None` reads the
    /// cell. Knee and ratio are clamped as the setters clamp (a knee floored
    /// at 0, a ratio at 1), a cell written raw as well as a driven value.
    #[inline]
    pub fn begin_block(&mut self, [knee, ratio, makeup]: [Option<f32>; 3]) -> CompressorBlock {
        let (threshold, knee_cell) = self.threshold.load();
        let threshold = Db(self.good[0].read(threshold.get()));
        let knee = Db(
            self.good[1].read(clamp_finite(knee.unwrap_or(knee_cell.get()), |k| {
                k.max(0.0)
            })),
        );
        let makeup_to =
            Db(self.good[3].read(makeup.unwrap_or_else(|| self.makeup_db.load().get())));
        let makeup_from = self.last_makeup.unwrap_or(makeup_to);
        self.last_makeup = Some(makeup_to);
        CompressorBlock {
            threshold,
            knee,
            ratio: CompressionRatio(self.good[2].read(clamp_finite(
                ratio.unwrap_or_else(|| self.ratio.load().get()),
                |r| CompressionRatio::new_clamped(r).get(),
            ))),
            makeup_from,
            makeup_to,
        }
    }

    /// Compressor gain (linear) for frame `i` of an `n`-frame block; see
    /// [`CompressorKernel::gain`].
    #[inline]
    pub fn gain(
        &mut self,
        block: &CompressorBlock,
        i: usize,
        n: usize,
        sc_level: f32,
        threshold_override: Option<Db>,
    ) -> f32 {
        self.kernel.gain(block, i, n, sc_level, threshold_override)
    }
}
