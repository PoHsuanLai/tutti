//! Tests for the voice pool, kept in one module because they exercise the
//! pieces in combination — a command sent through the handle, drained by the
//! pool, rendered through a slot, summed — and because many of them reach
//! private state (`slot.stretch`, `voices`) that a sibling module could not see.
//!
//! The implementation now lives one file per duty: [`types`](super::types),
//! [`slot`](super::slot), [`command`](super::command), [`pool`](super::pool),
//! [`node`](super::node).

#[allow(unused_imports)]
use super::command::*;
#[allow(unused_imports)]
use super::disk_voice::{DiskSource, DiskVoice, DiskVoiceConfig};
#[allow(unused_imports)]
use super::memory_source::{LoopSetting, MemorySource, VoiceWindow};
#[allow(unused_imports)]
use super::node::*;
#[allow(unused_imports)]
use super::pool::*;
#[allow(unused_imports)]
use super::slot::*;
#[allow(unused_imports)]
use super::types::*;
#[allow(unused_imports)]
use crate::stretch;
#[allow(unused_imports)]
use crossbeam_channel::bounded;
#[allow(unused_imports)]
use std::sync::Arc;
#[allow(unused_imports)]
use tutti_core::{
    Amplitude, AudioUnit, Beat, BeatDuration, BufferMut, BufferRef, Cents, ChannelLayout,
    PlaybackRate, ReadRate, SamplePosition, Samples, SignalFrame, StretchFactor, Timeline, Wave,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::memory_source::MemorySourceConfig;
    use tutti_core::dsp::{BufferArray, U2};
    use tutti_core::Bpm;

    use crate::test_transport::MockTransport;

    fn make_wave(samples: usize) -> Arc<Wave> {
        let data: Vec<f32> = (0..samples)
            .map(|i| (i as f32 + 1.0) / samples as f32)
            .collect();
        Arc::new(Wave::from_samples(44100.0, &data))
    }

    /// Send an in-memory voice through the one `AddVoice` path — the test-setup
    /// mirror of the timeline's `promote_pending_clip_waves` emit.
    fn add_ram_clip(handle: &VoicePoolHandle, id: SlotId, sampler: MemorySource) {
        handle.send(VoiceCommand::AddVoice {
            id,
            voice: Box::new(Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback::default(),
                channel_index: None,
            }),
            stretch: None,
        });
    }

    /// A transport seek must flush the stretch filter's buffered audio.
    ///
    /// **This is the gate for the seek bug and it FAILS before the fix.**
    ///
    /// `Timeline` is poll-only — `beat()` / `tempo()` / `is_rolling()`, no seek
    /// event — and nothing on any transport-driven path calls
    /// `AudioUnit::reset()`. So when the playhead jumps, the placement gate
    /// re-derives the new position correctly and immediately, while
    /// `stretch::Unit` keeps draining a FIFO primed from *before* the jump: up
    /// to `window * 4` samples per channel, plus per-bin phase accumulators
    /// still tracking the old material.
    ///
    /// The wave is loud in its first half and **exactly silent** in its second,
    /// which is what makes the assertion about the bug rather than about
    /// liveness. Every other stretch test on this path asserts only `!= 0.0` or
    /// `> 1e-6` (`clips_sum_together`, `six_channel_clip_with_stretch_...`), and
    /// a leak at signal level passes all of them — the same blind spot that let
    /// a 60 dB gain error live in the vocoder. Here, parking the playhead in the
    /// silent half means any output above the floor is provably material the
    /// filter should no longer be holding.
    /// Resetting a still-shared clone must not reach the live voice.
    ///
    /// The export path is `clone_isolated` -> `isolate` -> `reset`. It used to
    /// be `clone_isolated` -> `reset` -> `isolate`, and in that order `reset`
    /// cleared the FIFOs and phase accumulators of the *live* unit through the
    /// shared bank — measured as live output dropping to exactly 0.0 for one
    /// window. This pins the order-independent property: whatever the render
    /// does to its own clone after isolating, the live voice keeps its state.
    #[test]
    fn resetting_an_isolated_clone_leaves_the_live_voice_playing() {
        // Matches `make_wave`, so the source needs no rate conversion.
        const SR: f64 = 44_100.0;
        // Long enough to outlast the vocoder's fill-up: at 2x stretch the unit
        // consumes source at 1/2 rate, so 8192 warm-up ticks eat 4096 samples
        // and the 2048-sample window needs 4096 ticks before anything is emitted.
        let wave = make_wave(48_000);
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let sampler = MemorySource::with_transport(wave, transport.clone(), Beat::new(0.0), None);
        let mut live = VoiceNode::with_channels(
            Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Playback::default()
                },
                channel_index: None,
            },
            1usize,
        );
        live.allocate();
        assert!(live.slot.stretch.is_some(), "vacuous without a filter");

        // Build real vocoder history. The playhead must ADVANCE: a placed
        // source derives its position from the transport every tick, so a frozen
        // playhead feeds the vocoder a constant and it synthesises nothing.
        let mut out = [0.0f32; 1];
        let mut peak_before = 0.0f32;
        for _ in 0..8192 {
            live.tick(&[], &mut out);
            transport.advance(1, SR);
            peak_before = peak_before.max(out[0].abs());
        }
        assert!(
            peak_before > 1e-4,
            "the live voice must be audible before the render starts"
        );

        // What the export path does: clone, isolate, then reset the clone.
        let mut render = live.clone();
        render.isolate();
        render.reset();

        // The live voice must be unaffected — it keeps emitting immediately,
        // with no re-fill gap.
        let mut peak_after = 0.0f32;
        for _ in 0..512 {
            live.tick(&[], &mut out);
            transport.advance(1, SR);
            peak_after = peak_after.max(out[0].abs());
        }
        assert!(
            peak_after > 1e-4,
            "a region render silenced the live voice \
             (peak {peak_before:.6} -> {peak_after:.6})"
        );
    }

    /// Every node type the offline render can carry must sever its shared state.
    ///
    /// The render clones the live net and ticks it on a worker pool while the
    /// audio thread plays the original — the one genuinely concurrent path in
    /// the engine. `VoicePool` severs by clearing its voices; `VoiceNode` keeps
    /// its slot, so it has to sever its stretch filter explicitly.
    ///
    /// Asserted on `Arc` identity rather than on audio, because the failure is a
    /// data race: in release two threads would mutate one `UnsafeCell` with no
    /// synchronisation, which no output assertion can reliably observe.
    #[test]
    fn isolate_severs_a_standalone_voices_stretch_bank() {
        let wave = make_wave(4096);
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let sampler = MemorySource::with_transport(wave, transport.clone(), Beat::new(0.0), None);
        let live = VoiceNode::with_channels(
            Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Playback::default()
                },
                channel_index: None,
            },
            2usize,
        );
        assert!(
            live.slot.stretch.is_some(),
            "test is vacuous unless a filter is resident"
        );

        // What `clone_isolated` produces, then what the render's isolation pass
        // does to it.
        let mut render = live.clone();
        assert!(
            render
                .slot
                .stretch
                .as_ref()
                .unwrap()
                .shares_bank_with(live.slot.stretch.as_ref().unwrap()),
            "the clone should start out sharing — otherwise this proves nothing"
        );

        render.isolate();
        assert!(
            !render
                .slot
                .stretch
                .as_ref()
                .unwrap()
                .shares_bank_with(live.slot.stretch.as_ref().unwrap()),
            "isolate() left the render sharing the live voice's vocoder bank; \
             a worker thread would race the audio thread on it"
        );
    }

    #[test]
    fn a_transport_seek_flushes_stretch_state() {
        const SR: f64 = 44_100.0;
        // Ten seconds, so the playhead has room to run for thousands of blocks
        // inside one half without leaving it.
        const LEN: usize = 441_000;
        // 120 BPM = 2 beats/s, so the wave spans 20 beats and the halves split at
        // beat 10.
        const LOUD_BEAT: f64 = 2.0;
        const SILENT_BEAT: f64 = 12.0;

        // Loud first half at 3 kHz (a real signal — the vocoder needs a changing
        // input to synthesise from), exactly silent second half.
        let data: Vec<f32> = (0..LEN)
            .map(|i| {
                if i < LEN / 2 {
                    0.5 * (std::f32::consts::TAU * 3000.0 * i as f32 / SR as f32).sin()
                } else {
                    0.0
                }
            })
            .collect();
        let wave = Arc::new(Wave::from_samples(SR, &data));

        let transport = MockTransport::rolling(Beat::new(SILENT_BEAT), Bpm::new(120.0));
        let (mut unit, handle) = VoicePool::with_channels(Some(transport.clone()), None, 1usize);

        let sampler = MemorySource::with_transport(wave, transport.clone(), Beat::new(0.0), None);
        handle.send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Playback::default()
                },
                channel_index: None,
            }),
            // The sender materialises the filter on the control thread, as
            // `VoicePoolHandle::send` does for a voice that arrives
            // already needing one.
            stretch: None,
        });

        let mut out = [0.0f32; 1];
        unit.tick(&[], &mut out);
        assert!(
            unit.voices[0].needs_stretch(),
            "test is vacuous unless the stretch path is live"
        );

        // Advance the playhead one sample per tick, the way a real transport
        // moves — a frozen playhead would make every frame re-derive the same
        // position, so the source would emit a constant and the vocoder would
        // have nothing to synthesise from.
        let drive = |unit: &mut VoicePool, n: usize| {
            let mut peak = 0.0f32;
            let mut out = [0.0f32; 1];
            for _ in 0..n {
                unit.tick(&[], &mut out);
                transport.advance(1, SR);
                peak = peak.max(out[0].abs());
            }
            peak
        };

        // Settle in the silent half: whatever the filter emits here is the floor.
        drive(&mut unit, 8192);

        // Seek backwards into the loud half and prime the filter with it.
        transport.set_beat(Beat::new(LOUD_BEAT));
        let loud = drive(&mut unit, 8192);
        assert!(
            loud > 0.05,
            "the loud half should be audible after seeking into it; peak {loud}"
        );

        // Seek back into the silent half. The source is silent from this playhead
        // on, so anything above the floor is stale.
        transport.set_beat(Beat::new(SILENT_BEAT));
        let after_seek = drive(&mut unit, 2048);

        assert!(
            after_seek < 0.01,
            "stretch filter leaked pre-seek audio across a transport jump: \
             peak {after_seek} (the source is silent at this playhead)"
        );
    }

    /// **The same seek flush, for a STANDALONE voice.** `VoiceNode` is the other
    /// node that plays a stretched voice, and it had no cursor at all — so the
    /// pool-level fix covered one of the two paths and left this one smearing.
    ///
    /// Not hypothetical: `dawai-spectral`'s resynth adds bare `VoiceNode`s as
    /// correction nodes, and `tutti-export`'s offline rebind has a dedicated arm
    /// for them. A scrub across a stretched correction would drag pre-seek audio
    /// over the new region with nothing in the suite to notice, because every
    /// other assertion on this path checks only that output is non-zero.
    #[test]
    fn a_transport_seek_flushes_a_standalone_voice_node() {
        const SR: f64 = 44_100.0;
        const LEN: usize = 441_000;
        const LOUD_BEAT: f64 = 2.0;
        const SILENT_BEAT: f64 = 12.0;

        let data: Vec<f32> = (0..LEN)
            .map(|i| {
                if i < LEN / 2 {
                    0.5 * (std::f32::consts::TAU * 3000.0 * i as f32 / SR as f32).sin()
                } else {
                    0.0
                }
            })
            .collect();
        let wave = Arc::new(Wave::from_samples(SR, &data));

        let transport = MockTransport::rolling(Beat::new(SILENT_BEAT), Bpm::new(120.0));
        let sampler = MemorySource::with_transport(wave, transport.clone(), Beat::new(0.0), None);
        let mut node = VoiceNode::with_channels(
            Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Playback::default()
                },
                channel_index: None,
            },
            1usize,
        );
        assert!(
            node.slot.needs_stretch() && node.slot.stretch.is_some(),
            "test is vacuous unless the stretch path is live"
        );

        let drive = |node: &mut VoiceNode, n: usize| {
            let mut peak = 0.0f32;
            let mut out = [0.0f32; 1];
            for _ in 0..n {
                node.tick(&[], &mut out);
                transport.advance(1, SR);
                peak = peak.max(out[0].abs());
            }
            peak
        };

        drive(&mut node, 8192);

        transport.set_beat(Beat::new(LOUD_BEAT));
        let loud = drive(&mut node, 8192);
        assert!(
            loud > 0.05,
            "the loud half should be audible after seeking into it; peak {loud}"
        );

        transport.set_beat(Beat::new(SILENT_BEAT));
        let after_seek = drive(&mut node, 2048);
        assert!(
            after_seek < 0.01,
            "a standalone VoiceNode leaked pre-seek audio across a transport \
             jump: peak {after_seek} (the source is silent at this playhead)"
        );
    }

    /// The standalone twin of the pool's false-positive guard: a `VoiceNode`
    /// playing continuously must never flush.
    ///
    /// A cursor whose jump threshold is too tight resets the vocoder every block,
    /// turning its output into a stutter of ~50 ms fragments. Nothing else here
    /// would see it — every other assertion on this path is `!= 0.0`, and a
    /// stuttering stretcher is still non-zero.
    ///
    #[test]
    fn continuous_playback_does_not_flush_a_standalone_voice_node() {
        const SR: f64 = 44_100.0;
        const LEN: usize = 441_000;

        let data: Vec<f32> = (0..LEN)
            .map(|i| 0.5 * (std::f32::consts::TAU * 3000.0 * i as f32 / SR as f32).sin())
            .collect();
        let wave = Arc::new(Wave::from_samples(SR, &data));

        let transport = MockTransport::rolling(Beat::new(1.0), Bpm::new(120.0));
        let sampler = MemorySource::with_transport(wave, transport.clone(), Beat::new(0.0), None);
        let mut node = VoiceNode::with_channels(
            Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Playback::default()
                },
                channel_index: None,
            },
            1usize,
        );

        let mut out = [0.0f32; 1];
        for _ in 0..16_384 {
            node.tick(&[], &mut out);
            transport.advance(1, SR);
        }

        let mut quiet_blocks = 0usize;
        for _ in 0..256 {
            let mut peak = 0.0f32;
            for _ in 0..64 {
                node.tick(&[], &mut out);
                transport.advance(1, SR);
                peak = peak.max(out[0].abs());
            }
            if peak < 0.01 {
                quiet_blocks += 1;
            }
        }
        assert_eq!(
            quiet_blocks, 0,
            "continuous playback flushed the filter: {quiet_blocks}/256 blocks fell silent"
        );
    }

    /// **A stretched placed voice must not transpose**, across block boundaries.
    ///
    /// The bug this pins was in the *assembly*, not the DSP.
    /// [`stretch::Unit`] was correct and unit-tested, but `VoicePool` never
    /// applied [`stretch::Unit::input_rate`] — the method had zero call sites in
    /// the whole crate — so the vocoder was fed one source sample per output
    /// sample and the factor acted as plain varispeed: 2.0x turned 440 Hz into
    /// 880 Hz with the duration unchanged.
    ///
    /// **Why this renders many blocks.** The failure lives at block boundaries.
    /// A placed voice re-derives its origin from the playhead each block, and the
    /// playhead runs at wall clock, so a stretched read that covers
    /// `block / stretch` source samples gets re-seated a full `block` further on
    /// at the next boundary — discarding the stretch, forever. A single-block
    /// test cannot see it, and a fix applied to the within-block step alone made
    /// it *worse* (pitch +35% off, spectral purity 0.95 -> 0.54) rather than
    /// failing outright.
    ///
    /// Asserted by measuring the dominant frequency, because every other stretch
    /// assertion on this path is `!= 0.0` — and an octave-transposed voice is
    /// emphatically non-zero. Found by rendering WAVs and analysing them in
    /// numpy (`examples/render_cases.rs`); this is that check brought in-tree so
    /// CI can see it.
    #[test]
    fn a_stretched_placed_voice_holds_its_pitch_across_blocks() {
        const SR: f64 = 48_000.0;
        const BLOCK: usize = 64;
        const BLOCKS: usize = 750;

        for factor in [0.5f32, 1.5, 2.0] {
            let (mut pool, handle) = VoicePool::new();
            let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));

            // 440 Hz, long enough that even the fastest consumption stays inside
            // the material for the whole render.
            let data: Vec<f32> = (0..(SR as usize * 4))
                .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / SR as f32).sin())
                .collect();
            let wave = Arc::new(Wave::from_samples(SR, &data));

            let source =
                MemorySource::with_transport(wave, transport.clone(), Beat::new(0.0), None);
            let mut play = Playback::default();
            play.stretch = StretchFactor::new(factor);
            handle.send(VoiceCommand::AddVoice {
                id: SlotId(1),
                voice: Box::new(Voice {
                    source: VoiceSource::Memory(source),
                    play,
                    channel_index: None,
                }),
                stretch: None,
            });

            let ib = BufferArray::<U2>::new();
            let mut ob = BufferArray::<U2>::new();
            let mut out = Vec::with_capacity(BLOCK * BLOCKS);
            for _ in 0..BLOCKS {
                pool.process(BLOCK, &ib.buffer_ref(), &mut ob.buffer_mut());
                for i in 0..BLOCK {
                    out.push(ob.buffer_ref().at_f32(0, i));
                }
                transport.advance(BLOCK as i64, SR);
            }

            // Measure well past the vocoder's fill, over a whole number of
            // periods' worth of samples.
            let settled = &out[24_000..24_000 + 8192];
            let got = dominant_hz(settled, SR as f32);
            assert!(
                (got - 440.0).abs() < 440.0 * 0.02,
                "stretch {factor}x transposed a placed voice to {got:.1} Hz \
                 (the source is 440 Hz; stretch must change duration, not pitch)"
            );
        }
    }

    /// The **disk** tier must consume its source at the stretched rate too.
    ///
    /// The memory tier's fix scales a cursor; the disk tier has no cursor to
    /// scale. It pops from a butler-fed ring, advancing an internal
    /// `fractional_pos` by `RtState::read_rate` (speed × src_ratio), so the rate
    /// has to reach *that* accumulator instead.
    ///
    /// Asserted as **source consumption**, not as pitch. A ring-fed reader has no
    /// absolute position to measure a frequency against — it renders whatever the
    /// butler last handed it — so the observable property is how fast the ring
    /// drains: at 2x stretch a block of output must consume half a block of
    /// source, because the vocoder supplies the other half.
    ///
    /// This is the same defect as
    /// `a_stretched_placed_voice_holds_its_pitch_across_blocks`, in the tier
    /// where it shows up differently.
    #[test]
    fn a_stretched_disk_voice_consumes_its_source_at_the_stretched_rate() {
        use crate::butler::RtState;

        const BLOCK: usize = 64;
        const BLOCKS: usize = 8;

        use crate::butler::{share_reader, RegionBuffer, RegionId};
        use crate::voice::{DiskSource, DiskVoice, DiskVoiceConfig, VoiceWindow};
        use std::path::PathBuf;
        use std::sync::atomic::Ordering;

        // How many source frames the ring gives up over a fixed render, at a
        // given stretch factor. `read_position` is the ring's own consumption
        // counter, so this measures what the reader actually took rather than
        // what anything claims it should have.
        let consumed = |factor: f32| -> u64 {
            let (mut pool, handle) = VoicePool::new();
            let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));

            // Enough ring that the unstretched (fastest) case cannot underrun,
            // so a short read means the rate applied rather than that we ran dry
            // — the control assertion below checks the other direction.
            let flat: Vec<f32> = (0..8192)
                .flat_map(|i| {
                    let s = (std::f32::consts::TAU * 440.0 * i as f32 / 44_100.0).sin();
                    [s, s]
                })
                .collect();
            let (mut writer, reader) =
                RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 8192 + 64, 2usize);
            writer.push_interleaved(&flat);
            let read_pos = reader.read_position_shared();
            // ONE `RtState`, shared by the source and the gate — `DiskVoice::new`
            // requires it, and the published stretch rate is read back through
            // the same cell. Two separate states here made the fix look inert.
            let state = Arc::new(RtState::new());
            let inner = DiskSource::new(share_reader(reader), Arc::clone(&state));
            let voice = DiskVoice::new(
                inner,
                Arc::clone(&state),
                DiskVoiceConfig {
                    timeline: transport.clone(),
                    window: VoiceWindow {
                        start: Beat::new(0.0),
                        duration: None,
                    },
                    file_sample_rate: 44_100.0,
                },
            );

            let mut play = Playback::default();
            play.stretch = StretchFactor::new(factor);
            handle.send(VoiceCommand::AddVoice {
                id: SlotId(1),
                voice: Box::new(Voice {
                    source: VoiceSource::Disk(voice),
                    play,
                    channel_index: None,
                }),
                stretch: None,
            });

            let ib = BufferArray::<U2>::new();
            let mut ob = BufferArray::<U2>::new();
            for _ in 0..BLOCKS {
                pool.process(BLOCK, &ib.buffer_ref(), &mut ob.buffer_mut());
                transport.advance(BLOCK as i64, 44_100.0);
            }
            read_pos.load(Ordering::Relaxed)
        };

        let unstretched = consumed(1.0);
        let doubled = consumed(2.0);
        assert!(
            unstretched > 0,
            "the unstretched control consumed nothing — the ring never fed, so \
             this comparison would prove nothing"
        );

        // At 2x the vocoder emits two output samples per source sample, so the
        // ring must drain at half the rate.
        let ratio = unstretched as f64 / doubled.max(1) as f64;
        assert!(
            (ratio - 2.0).abs() < 0.15,
            "2x stretch consumed {doubled} source frames where 1x consumed \
             {unstretched} (ratio {ratio:.2}, want 2.0): the stretch rate never \
             reached the disk tier's ring, so the factor acts as varispeed"
        );
    }

    /// Dominant frequency by Hann-windowed DFT scan — a test helper, not
    /// production code. Coarse (2 Hz) because the assertion tolerance is 2%.
    #[cfg(test)]
    fn dominant_hz(x: &[f32], sample_rate: f32) -> f32 {
        let n = x.len();
        let mut best = (0.0f32, 0.0f32);
        let mut f = 60.0f32;
        while f < 2500.0 {
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for (i, &s) in x.iter().enumerate() {
                let w = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos();
                let p = std::f32::consts::TAU * f * i as f32 / sample_rate;
                re += s * w * p.cos();
                im -= s * w * p.sin();
            }
            let m = (re * re + im * im).sqrt();
            if m > best.1 {
                best = (f, m);
            }
            f += 2.0;
        }
        best.0
    }

    /// Continuous playback must NOT flush — the false positive that matters.
    ///
    /// A jump threshold set too tight fires on ordinary blocks, so the vocoder
    /// resets constantly and its output becomes a stutter of ~50 ms fragments.
    /// Nothing else in the suite would see it: every other stretch assertion is
    /// `!= 0.0` or `> 1e-6`, and a stuttering stretcher is still non-zero. So
    /// this asserts *continuity of level* across hundreds of ordinary blocks —
    /// after the pipeline fills, no block may collapse to silence.
    ///
    /// It spent time `#[ignore]`d on a SECOND, unrelated cause of the same
    /// symptom: it measured 32/256 silent blocks even with the flush logic removed,
    /// because the vocoder published `stretch x` as many output samples as it
    /// consumed against a host that takes exactly one per call. Two independent
    /// bugs producing one indistinguishable symptom is the argument for asserting
    /// level continuity here rather than merely "output is non-zero".
    #[test]
    fn continuous_playback_does_not_flush_the_stretch_filter() {
        const SR: f64 = 44_100.0;
        const LEN: usize = 441_000;

        // Steady 3 kHz throughout: any dropout is the filter being reset, not the
        // material.
        let data: Vec<f32> = (0..LEN)
            .map(|i| 0.5 * (std::f32::consts::TAU * 3000.0 * i as f32 / SR as f32).sin())
            .collect();
        let wave = Arc::new(Wave::from_samples(SR, &data));

        let transport = MockTransport::rolling(Beat::new(1.0), Bpm::new(120.0));
        let (mut unit, handle) = VoicePool::with_channels(Some(transport.clone()), None, 1usize);

        let sampler = MemorySource::with_transport(wave, transport.clone(), Beat::new(0.0), None);
        handle.send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Playback::default()
                },
                channel_index: None,
            }),
            stretch: None,
        });

        let mut out = [0.0f32; 1];
        // Fill the vocoder pipeline first: the opening blocks are legitimately
        // quiet while the FIFOs and overlap-add tail prime.
        for _ in 0..16_384 {
            unit.tick(&[], &mut out);
            transport.advance(1, SR);
        }
        assert!(unit.voices[0].needs_stretch(), "stretch path must be live");

        // Steady state: measure per-block peaks and require every one to carry
        // signal. A reset mid-run empties the FIFO, so the following block is
        // silent — which is exactly what this catches.
        let mut quiet_blocks = 0usize;
        for _ in 0..256 {
            let mut peak = 0.0f32;
            for _ in 0..64 {
                unit.tick(&[], &mut out);
                transport.advance(1, SR);
                peak = peak.max(out[0].abs());
            }
            if peak < 0.01 {
                quiet_blocks += 1;
            }
        }
        assert_eq!(
            quiet_blocks, 0,
            "continuous playback flushed the filter: {quiet_blocks}/256 blocks fell silent"
        );
    }

    #[test]
    fn add_and_remove_clips() {
        let (mut unit, handle) = VoicePool::new();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        let sampler =
            MemorySource::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(out[0] != 0.0 || out[1] != 0.0, "voice should produce audio");

        handle.send(VoiceCommand::Remove(SlotId(1)));
        unit.tick(&[], &mut out);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn silence_when_transport_stopped() {
        let (mut unit, handle) = VoicePool::new();
        let transport = MockTransport::stopped(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        let sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn clips_sum_together() {
        let (mut unit, handle) = VoicePool::new();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        for i in 0..3 {
            let sampler =
                MemorySource::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
            add_ram_clip(&handle, SlotId(i), sampler);
        }

        let mut out_3 = [0.0f32; 2];
        unit.tick(&[], &mut out_3);

        let (mut unit2, handle2) = VoicePool::new();
        let sampler =
            MemorySource::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
        add_ram_clip(&handle2, SlotId(0), sampler);
        let mut out_1 = [0.0f32; 2];
        unit2.tick(&[], &mut out_1);

        let tolerance = 0.001;
        assert!((out_3[0] - out_1[0] * 3.0).abs() < tolerance);
        assert!((out_3[1] - out_1[1] * 3.0).abs() < tolerance);
    }

    #[test]
    fn clone_snapshots_clips() {
        let (mut unit, handle) = VoicePool::new();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        let sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);

        let mut cloned = unit.clone();
        let mut out_clone = [0.0f32; 2];
        cloned.tick(&[], &mut out_clone);

        assert!(out_clone[0] != 0.0, "cloned unit should have the voice");
    }

    #[test]
    fn insert_voice_is_audible_without_channel() {
        let (mut unit, _handle) = VoicePool::new();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        let sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        unit.insert_voice(
            SlotId(1),
            Voice {
                play: Playback {
                    gain: sampler.gain(),
                    speed: sampler.speed(),
                    loop_: sampler.loop_setting(),
                    direction: Direction::Forward,
                    stretch: StretchFactor::new(1.0),
                    pitch: Cents::new(0.0),
                },
                source: VoiceSource::Memory(sampler),
                channel_index: None,
            },
        );

        // No tick/drain needed — the voice is already in the slot list.
        assert_eq!(unit.voice_count(), 1);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(
            out[0] != 0.0 || out[1] != 0.0,
            "inserted voice should produce audio"
        );
    }

    #[test]
    fn detached_reader_has_no_channel_and_is_empty() {
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        // A render-only reader is born empty and channel-less: there is no
        // sender for its `rx`, so no command can ever reach it.
        let mut unit = VoicePool::detached(transport.clone());
        assert_eq!(unit.voice_count(), 0, "detached reader starts empty");

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out); // drains its (permanently empty) queue
        assert_eq!(
            unit.voice_count(),
            0,
            "detached reader receives no commands"
        );
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);

        // But voices inserted directly (the Populate path) are audible.
        let sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        unit.insert_voice(
            SlotId(1),
            Voice {
                play: Playback {
                    gain: sampler.gain(),
                    speed: sampler.speed(),
                    loop_: sampler.loop_setting(),
                    direction: Direction::Forward,
                    stretch: StretchFactor::new(1.0),
                    pitch: Cents::new(0.0),
                },
                source: VoiceSource::Memory(sampler),
                channel_index: None,
            },
        );
        unit.tick(&[], &mut out);
        assert!(out[0] != 0.0 || out[1] != 0.0, "inserted voice is audible");
    }

    #[test]
    fn update_gain() {
        let (mut unit, handle) = VoicePool::new();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        let sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out_before = [0.0f32; 2];
        unit.tick(&[], &mut out_before);

        handle.send(VoiceCommand::UpdateGain {
            id: SlotId(1),
            gain: Amplitude::new(0.5),
        });

        let mut out_after = [0.0f32; 2];
        unit.tick(&[], &mut out_after);

        assert!((out_after[0] - out_before[0] * 0.5).abs() < 0.01);
    }

    #[test]
    fn update_stretch_enables_processor() {
        let (mut unit, handle) = VoicePool::new();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(4096);

        let sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(!unit.voices[0].needs_stretch(), "no stretch by default");

        handle.send(VoiceCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: StretchFactor::new(2.0),
            pitch_cents: Cents::new(0.0),
        });
        unit.tick(&[], &mut out);
        assert!(unit.voices[0].needs_stretch(), "stretch should be active");

        handle.send(VoiceCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: StretchFactor::new(1.0),
            pitch_cents: Cents::new(0.0),
        });
        unit.tick(&[], &mut out);
        assert!(
            !unit.voices[0].needs_stretch(),
            "identity stretch disables processor"
        );
    }

    /// `VoiceNode` MUST be an `AudioUnit` in its own right: `dawai-spectral`'s
    /// resynth adds a standalone voice node to its net, and `tutti-export`'s
    /// region render downcasts these nodes to rebind them offline. If a `Voice`
    /// stopped being an `AudioUnit`, both paths would break — resynth couldn't
    /// add it, and the offline downcast would silently stop matching (wrong
    /// transport offline, no compile error). This guards that contract: a
    /// standalone `VoiceNode` produces the same audio one mixer slot does.
    #[test]
    fn voice_node_is_a_standalone_audio_unit() {
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        // Same voice the mixer would hold for one voice.
        let sampler =
            MemorySource::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
        let voice = Voice {
            source: VoiceSource::Memory(sampler),
            play: Playback::default(),
            channel_index: None,
        };

        // As a standalone AudioUnit: 0 in, 2 out, produces audio.
        let mut node = VoiceNode::new(voice);
        assert_eq!(node.inputs(), 0);
        assert_eq!(node.outputs(), 2);

        let mut out = [0.0f32; 2];
        node.tick(&[], &mut out);
        assert!(
            out[0] != 0.0 || out[1] != 0.0,
            "standalone VoiceNode should produce audio"
        );

        // It reads the SAME single-voice frame the mixer does for one slot.
        let sampler2 = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        let (mut mixer, _handle) = VoicePool::new();
        mixer.insert_voice(
            SlotId(1),
            Voice {
                play: Playback {
                    gain: sampler2.gain(),
                    speed: sampler2.speed(),
                    loop_: sampler2.loop_setting(),
                    direction: Direction::Forward,
                    stretch: StretchFactor::new(1.0),
                    pitch: Cents::new(0.0),
                },
                source: VoiceSource::Memory(sampler2),
                channel_index: None,
            },
        );
        let mut node2 = VoiceNode::new(Voice {
            source: VoiceSource::Memory(MemorySource::with_transport(
                make_wave(100),
                MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0)),
                Beat::new(0.0),
                None,
            )),
            play: Playback::default(),
            channel_index: None,
        });
        let mut mixer_out = [0.0f32; 2];
        let mut node_out = [0.0f32; 2];
        mixer.tick(&[], &mut mixer_out);
        node2.tick(&[], &mut node_out);
        assert!(
            (mixer_out[0] - node_out[0]).abs() < 1e-6,
            "VoiceNode frame must match the mixer's single-slot read"
        );
    }

    /// `Voice::replace_transport` rebinds the clock the source actually READS,
    /// preserving start/duration — the offline render's rebind path.
    ///
    /// This test used to assert on `play.placement`, the record that was rebound
    /// but never read. It therefore passed whether or not the real read clock
    /// moved, which is the precise failure it was written to catch. With that
    /// field deleted there is only one clock, and the assertion is behavioural:
    /// the swapped-in transport is STOPPED, so the source must report no position
    /// and render silence.
    #[test]
    fn voice_replace_transport_rebinds_the_clock_the_source_reads() {
        let wave = make_wave(100);
        let live = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let sampler = MemorySource::with_transport(wave, live, Beat::new(2.0), None);
        let mut voice = Voice {
            source: VoiceSource::Memory(sampler),
            play: Playback::default(),
            channel_index: None,
        };

        // Rolling at beat 0 but the window starts at beat 2 — outside, so move the
        // playhead in first and confirm the source is reading.
        match &voice.source {
            VoiceSource::Memory(s) => {
                assert!(
                    s.window_position().is_none(),
                    "setup: beat 0 is before the beat-2 window"
                );
            }
            other => panic!("expected a Memory source, got {other:?}"),
        }

        let stopped = MockTransport::stopped(Beat::new(4.0), Bpm::new(140.0));
        voice.replace_transport(stopped);

        match &voice.source {
            VoiceSource::Memory(s) => {
                // The window survived the rebind...
                assert_eq!(s.start_beat(), Beat::new(2.0));
                // ...and the source now reads the STOPPED clock. Beat 4 is inside
                // the window, so a source still on the live clock would have
                // reported a position here.
                assert!(
                    s.window_position().is_none(),
                    "the source must read the stopped offline clock, not the live one"
                );
            }
            other => panic!("expected a Memory source, got {other:?}"),
        }
    }

    // --- 0e: dropped commands must not be recorded as applied ---

    /// A streaming voice with no butler channel cannot have its loop applied —
    /// the butler owns streaming loop state. The drain used to send nothing and
    /// still write `play.loop_`, so the intent record claimed a loop that was
    /// never set; `insert_voice` would then replay that lie. Reachable on every
    /// offline path: `new()`, `detached()`, and `isolate()` all have no butler.
    #[test]
    fn loop_on_a_butlerless_streaming_voice_is_not_recorded() {
        use crate::butler::{share_reader, RegionBuffer, RegionId, RtState};
        use crate::voice::disk_voice::DiskSource;
        use crate::voice::disk_voice::{DiskVoice, DiskVoiceConfig};

        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let (mut unit, handle) = VoicePool::new();

        // Build a Disk voice with no butler channel.
        let (writer, reader) =
            RegionBuffer::with_capacity(RegionId(1), std::path::PathBuf::new(), 128, 2usize);
        drop(writer);
        let state = std::sync::Arc::new(RtState::new());
        let inner = DiskSource::new(share_reader(reader), state.clone());
        let clip_reader = DiskVoice::new(
            inner,
            state,
            DiskVoiceConfig {
                timeline: transport.clone(),
                window: VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                },
                file_sample_rate: 44100.0,
            },
        );

        let id = SlotId(7);
        handle.send(VoiceCommand::AddVoice {
            id,
            voice: Box::new(Voice {
                source: VoiceSource::Disk(clip_reader),
                play: Playback::default(),
                channel_index: None,
            }),
            stretch: None,
        });

        handle.send(VoiceCommand::UpdateLoop {
            id,
            looping: true,
            loop_start: SamplePosition::new(0.0),
            loop_end: SamplePosition::new(64.0),
            crossfade_samples: 0,
        });

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);

        let play = unit.playback_of(id).expect("slot exists");
        assert_eq!(
            play.loop_,
            LoopSetting::Off,
            "a loop the butler was never told about must not be recorded as applied"
        );
    }

    /// Channel `c` carries the constant `c + 1`.
    fn indexed_wave(channels: usize, len: usize) -> Arc<Wave> {
        let mut w = Wave::zero(channels, 44_100.0, len as f64 / 44_100.0);
        for i in 0..w.len() {
            for c in 0..channels {
                w.set(c, i, (c + 1) as f32);
            }
        }
        Arc::new(w)
    }

    #[test]
    fn reader_and_voice_node_default_to_stereo() {
        let (unit, _h) = VoicePool::new();
        assert_eq!(unit.channels(), ChannelLayout::Stereo);
        assert_eq!(unit.outputs(), 2);
    }

    /// `route`'s width must track `outputs()` on both nodes, or fundsp mis-plans
    /// their latency — silent except as PDC drift.
    #[test]
    fn route_width_tracks_outputs_on_both_nodes() {
        for w in [1usize, 2, 6, 8] {
            let (mut unit, _h) = VoicePool::with_channels(None, None, w);
            let out = unit.route(&SignalFrame::new(0), 44_100.0);
            assert_eq!(
                out.len(),
                unit.outputs(),
                "reader route/outputs at width {w}"
            );

            let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
            let sampler = MemorySource::with_config(
                indexed_wave(6, 64),
                MemorySourceConfig {
                    channels: ChannelLayout::from(w),
                    timeline: Some(transport),
                    window: VoiceWindow {
                        start: Beat::new(0.0),
                        duration: None,
                    },
                    ..Default::default()
                },
            );
            let voice = Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback::default(),
                channel_index: None,
            };
            let mut vn = VoiceNode::with_channels(voice, w);
            let out = vn.route(&SignalFrame::new(0), 44_100.0);
            assert_eq!(
                out.len(),
                vn.outputs(),
                "voice node route/outputs at width {w}"
            );
        }
    }

    /// A 6-channel voice in a 6-wide reader must reach all six outputs, through
    /// both entry points (`tick` sums into the caller's slice; `process`
    /// accumulates into a planar buffer — different code).
    #[test]
    fn six_channel_clip_reaches_all_six_reader_outputs() {
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let (mut unit, _h) = VoicePool::with_channels(Some(transport.clone()), None, 6usize);
        let sampler = MemorySource::with_config(
            indexed_wave(6, 512),
            MemorySourceConfig {
                channels: ChannelLayout::Multi(6),
                timeline: Some(transport),
                window: VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                },
                ..Default::default()
            },
        );
        unit.insert_voice(
            SlotId(1),
            Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback::default(),
                channel_index: None,
            },
        );

        let mut out = [0.0f32; 6];
        unit.tick(&[], &mut out);
        for (c, &got) in out.iter().enumerate() {
            assert!(
                (got - (c + 1) as f32).abs() < 1e-3,
                "tick: channel {c} should carry {}, got {got} ({out:?})",
                c + 1
            );
        }
        assert!(
            out[2..].iter().all(|&s| s.abs() > 0.5),
            "channels 2..6 were dropped: {out:?}"
        );
    }

    /// The stretch branch is separate code from the direct read, and it is the
    /// one R6 warns about: a slot whose stretcher is narrower than the reader
    /// truncates silently, and ONLY when stretch is enabled. Nothing else in the
    /// suite exercises that combination at width 6.
    #[test]
    fn six_channel_clip_with_stretch_reaches_all_six_outputs() {
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let (mut unit, _h) = VoicePool::with_channels(Some(transport.clone()), None, 6usize);
        let sampler = MemorySource::with_config(
            indexed_wave(6, 4096),
            MemorySourceConfig {
                channels: ChannelLayout::Multi(6),
                timeline: Some(transport.clone()),
                window: VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                },
                ..Default::default()
            },
        );
        unit.insert_voice(
            SlotId(1),
            Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    // Off unity, so `needs_stretch()` takes the vocoder path.
                    stretch: StretchFactor::new(2.0),
                    ..Default::default()
                },
                channel_index: None,
            },
        );

        // The phase vocoder has FFT latency, so early frames are legitimately
        // silent; drive until every channel has produced something.
        let mut seen = [false; 6];
        let mut out = [0.0f32; 6];
        for n in 0..16_384 {
            unit.tick(&[], &mut out);
            for (c, &s) in out.iter().enumerate() {
                if s.abs() > 1e-6 {
                    seen[c] = true;
                }
            }
            if seen.iter().all(|&b| b) {
                break;
            }
            let _ = n;
        }
        assert!(
            seen.iter().all(|&b| b),
            "channels {:?} never produced output through the stretch path",
            seen.iter()
                .enumerate()
                .filter(|(_, &b)| !b)
                .map(|(c, _)| c)
                .collect::<Vec<_>>()
        );
    }

    /// The SENDER builds the stretch filter, not the drain.
    ///
    /// `VoicePoolHandle::send` runs on the control thread and fills
    /// `AddVoice::stretch` when the voice asks for stretching; `drain_commands`
    /// (which runs inside `tick`/`process`) only moves it in. If construction
    /// ever moves back into the drain, an `AddVoice` sent with `stretch: None`
    /// would still end up with a filter — so this asserts the filter is present
    /// only because the send path put it there.
    #[test]
    fn send_builds_the_stretch_filter_not_the_drain() {
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let (mut unit, handle) = VoicePool::with_channels(Some(transport.clone()), None, 6usize);

        let mk = |stretch: StretchFactor| {
            let sampler = MemorySource::with_config(
                indexed_wave(6, 128),
                MemorySourceConfig {
                    channels: ChannelLayout::Multi(6),
                    timeline: Some(transport.clone()),
                    window: VoiceWindow {
                        start: Beat::new(0.0),
                        duration: None,
                    },
                    ..Default::default()
                },
            );
            Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch,
                    ..Default::default()
                },
                channel_index: None,
            }
        };

        // A stretching voice: `send` must attach a filter, at the READER's width.
        // Observe the queued command BEFORE the drain sees it — that is what
        // distinguishes "the sender built it" from "the drain built it", and it
        // is the only observation that can: after draining, a filter is present
        // either way.
        let peeked = {
            let (probe_tx, probe_rx) = bounded(4);
            let probe = VoicePoolHandle {
                tx: probe_tx,
                retired: bounded(0).1,
                channels: ChannelLayout::Multi(6),
                sample_rate: 44100.0,
            };
            probe.send(VoiceCommand::AddVoice {
                id: SlotId(9),
                voice: Box::new(mk(StretchFactor::new(2.0))),
                stretch: None,
            });
            match probe_rx.try_recv() {
                Ok(VoiceCommand::AddVoice { stretch, .. }) => stretch,
                other => panic!("expected a queued AddVoice, got {other:?}"),
            }
        };
        let peeked = peeked.expect(
            "send must attach the filter BEFORE queueing — if this is None the \
             construction has moved back into the audio-thread drain",
        );
        assert_eq!(
            peeked.channels(),
            ChannelLayout::Multi(6),
            "the sender must build at the reader's width"
        );

        handle.send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(mk(StretchFactor::new(2.0))),
            stretch: None,
        });
        // A non-stretching voice: no filter, because none is needed.
        handle.send(VoiceCommand::AddVoice {
            id: SlotId(2),
            voice: Box::new(mk(StretchFactor::new(1.0))),
            stretch: None,
        });

        let mut out = [0.0f32; 6];
        unit.tick(&[], &mut out); // drains

        let stretching = unit.voices.iter().find(|s| s.id == SlotId(1)).unwrap();
        let plain = unit.voices.iter().find(|s| s.id == SlotId(2)).unwrap();

        let filter = stretching
            .stretch
            .as_ref()
            .expect("send must have built a filter for the stretching voice");
        assert_eq!(
            filter.channels(),
            ChannelLayout::Multi(6),
            "the filter must match the reader's width, not a default"
        );
        assert!(
            plain.stretch.is_none(),
            "a non-stretching voice must not carry a filter — that is the whole \
             point of building lazily"
        );
    }

    /// A slot whose intent says stretch but whose filter has not arrived reads
    /// DRY, not silent.
    ///
    /// `active_stretch` requires both the intent and the unit. Degrading to a
    /// dry read means a late filter costs one block of un-stretched audio rather
    /// than a gap — and, critically, never an allocation in the callback.
    #[test]
    fn a_missing_stretch_filter_reads_dry_not_silent() {
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let (mut unit, _h) = VoicePool::with_channels(Some(transport.clone()), None, 6usize);

        let sampler = MemorySource::with_config(
            indexed_wave(6, 512),
            MemorySourceConfig {
                channels: ChannelLayout::Multi(6),
                timeline: Some(transport),
                window: VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                },
                ..Default::default()
            },
        );
        // Insert DIRECTLY with no filter, simulating one that has not arrived.
        unit.insert_voice_with_stretch(
            SlotId(1),
            Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Default::default()
                },
                channel_index: None,
            },
            None,
        );

        let mut out = [0.0f32; 6];
        unit.tick(&[], &mut out);
        for (c, &s) in out.iter().enumerate() {
            assert!(
                (s - (c + 1) as f32).abs() < 1e-3,
                "channel {c} should read dry ({}), got {s} — a missing filter \
                 must not silence the slot",
                c + 1
            );
        }
    }
}
