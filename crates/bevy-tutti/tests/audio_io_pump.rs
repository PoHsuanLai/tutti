//! The live I/O edge: the pump, what it records, and how it composes.
//!
//! - `audio_pump` — the pump's lifetime, which is the only thing that layer owns.
//! - `master_record` — recording the master output, the capture path end to end.
//! - `io_graph_composition` — the I/O edge composing with the graph reconciler.
//!
//! All three need `audio-io` and all three are about the same seam between a
//! graph and a device-shaped sink, so they are one file. Bodies and test names
//! are unchanged; each module keeps its own helpers.
//!
//! Note the `thread::sleep` polls inside: they are load-bearing here (the pump
//! runs on its own thread) and are deliberately left alone by this pass.

#![cfg(feature = "audio-io")]

/// The pump's lifetime, which is the only thing this layer owns.
///
/// `pump` itself is the engine's and is tested there. What is asserted here is
/// that the sink gets finalized **exactly once, on every path out** — because
/// `AudioOut::finalize` consumes `self`, can fail, and for a WAV decides whether
/// the file opens at all. A dropped finalize is not a degraded recording, it is
/// an unreadable one.
///
/// Every test writes a real `WavOut` into a `tempfile` and reads it back with
/// `hound`, so "was it finalized" is answered by the file rather than by a flag
/// this crate set. No audio device is involved.
/// (Was `tests/audio_pump.rs`.)
mod audio_pump {
    use std::path::PathBuf;

    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;

    use bevy_tutti::graph::{AudioPump, AudioPumpAppExt, PumpFinished};
    // Through `bevy_tutti::io`, not the engine crates directly: a host should not
    // need to name `tutti-core` or `tutti-io` to write a pump, and this pins that.
    use bevy_tutti::io::{AudioIn, BitDepth, ChannelLayout, OnEmpty, Samples, WavOut};

    const SAMPLE_RATE: f64 = 48_000.0;

    /// A finite source: hands out its frames in bounded chunks, then `0` forever.
    /// The shape a decoded file has.
    struct SliceSource {
        frames: Vec<[f32; 2]>,
        pos: usize,
    }

    impl AudioIn for SliceSource {
        const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;

        fn layout(&self) -> ChannelLayout {
            ChannelLayout::STEREO
        }

        fn poll_into(&mut self, out: &mut [f32]) -> Samples {
            // `out` is flat interleaved; the fixture holds `[f32; 2]` frames, and
            // the two have identical layout, so this is a reinterpretation not a
            // copy. The RETURN is frames.
            let (out, _odd) = out.as_chunks_mut::<2>();
            let n = (self.frames.len() - self.pos).min(out.len());
            out[..n].copy_from_slice(&self.frames[self.pos..self.pos + n]);
            self.pos += n;
            Samples(n)
        }
    }

    /// A live source: yields a little, then reports empty *without being
    /// exhausted*, the way a mic ring does between callback pushes.
    ///
    /// This is the fixture that makes `OnEmpty` observable — a pump that read its
    /// `0` as end-of-stream would stop here with frames still to come.
    struct LiveSource {
        polls: usize,
    }

    impl AudioIn for LiveSource {
        const ON_EMPTY: OnEmpty = OnEmpty::Starved;

        fn layout(&self) -> ChannelLayout {
            ChannelLayout::STEREO
        }

        fn poll_into(&mut self, out: &mut [f32]) -> Samples {
            self.polls += 1;
            // Every other poll is empty; the rest yield one frame.
            if self.polls % 2 == 1 {
                return Samples::ZERO;
            }
            if out.len() < 2 {
                return Samples::ZERO;
            }
            out[0] = 0.5;
            out[1] = -0.5;
            Samples(1)
        }
    }

    fn frames(n: usize) -> Vec<[f32; 2]> {
        (0..n)
            .map(|i| [i as f32 / n as f32, -(i as f32) / n as f32])
            .collect()
    }

    fn sink(path: &PathBuf) -> WavOut {
        WavOut::create(path, SAMPLE_RATE, ChannelLayout::STEREO, BitDepth::Float32)
            .expect("sink should open")
    }

    /// An app with the stereo-`f32` pump drain registered. No engine, no device —
    /// the pump is deliberately not gated on `engine_ready`.
    fn app() -> App {
        let mut app = App::new();
        app.add_audio_pump::<f32>();
        app
    }

    /// Run frames until every pump has been drained, or give up.
    ///
    /// The pump is on a real thread, so the drain needs the wall clock to advance;
    /// this is the join point, not a poll-until-true hack.
    fn run_until_drained(app: &mut App, max_frames: usize) -> bool {
        for _ in 0..max_frames {
            app.update();
            if app
                .world_mut()
                .query::<&AudioPump<f32>>()
                .iter(app.world())
                .next()
                .is_none()
            {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    fn read_back(path: &PathBuf) -> hound::WavReader<std::io::BufReader<std::fs::File>> {
        hound::WavReader::open(path).expect("a finalized WAV must be readable")
    }

    /// A finite source drains itself: the pump exits on end-of-stream, finalizes,
    /// and every frame reaches the file.
    ///
    /// No `stop()` call anywhere — the source's own `ON_EMPTY` ends it.
    #[test]
    fn a_finite_source_finalizes_without_being_told_to_stop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("finite.wav");

        let mut app = app();
        let data = frames(3000);
        let src = SliceSource {
            frames: data.clone(),
            pos: 0,
        };
        app.world_mut()
            .spawn(AudioPump::start(src, sink(&path), Samples(256)));

        assert!(
            run_until_drained(&mut app, 200),
            "a finite source must end on its own"
        );

        let reader = read_back(&path);
        assert_eq!(reader.spec().channels, 2);
        // Two samples (L, R) per stereo frame.
        assert_eq!(
            reader.len() as usize,
            data.len() * 2,
            "every frame the source held must reach the file"
        );
    }

    /// A live source runs until stopped, then finalizes.
    ///
    /// Its empty polls must not be mistaken for the end — the fixture is never
    /// exhausted, so a pump that stopped at the first `0` would write far less than
    /// it had time to.
    #[test]
    fn a_live_source_runs_until_stopped_then_finalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.wav");

        let mut app = app();
        let entity = app
            .world_mut()
            .spawn(AudioPump::start(
                LiveSource { polls: 0 },
                sink(&path),
                Samples(64),
            ))
            .id();

        // Let it move some frames across several park cycles.
        for _ in 0..8 {
            app.update();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        app.world()
            .entity(entity)
            .get::<AudioPump<f32>>()
            .expect("a live pump must still be running — it is never exhausted")
            .stop();

        assert!(
            run_until_drained(&mut app, 200),
            "a stopped pump must be joined and drained"
        );

        let reader = read_back(&path);
        assert!(
            reader.len() > 0,
            "a live source polled across many parks must have written something; \
             zero means its empty polls were read as end-of-stream"
        );
    }

    /// Despawning mid-pump still finalizes the sink.
    ///
    /// This is the observer's entire reason to exist: the entity leaves the drain's
    /// query, so nothing else can ever join that thread, and the sink it owns would
    /// never be closed — leaving a WAV whose header was never patched.
    #[test]
    fn despawning_mid_pump_still_produces_a_readable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("despawned.wav");

        let mut app = app();
        let entity = app
            .world_mut()
            .spawn(AudioPump::start(
                LiveSource { polls: 0 },
                sink(&path),
                Samples(64),
            ))
            .id();

        for _ in 0..4 {
            app.update();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // No `stop()` — the despawn is the only signal.
        app.world_mut().entity_mut(entity).despawn();
        app.update();

        let reader = read_back(&path);
        assert!(
            reader.len() > 0,
            "the despawn must finalize the sink, not detach the thread"
        );
    }

    /// The finalize result reaches the world as a message, so a host can react to a
    /// sink that failed to close.
    #[test]
    fn a_finished_pump_reports_through_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reported.wav");

        let mut app = app();
        let entity = app
            .world_mut()
            .spawn(AudioPump::start(
                SliceSource {
                    frames: frames(500),
                    pos: 0,
                },
                sink(&path),
                Samples(256),
            ))
            .id();

        assert!(run_until_drained(&mut app, 200), "the pump should finish");

        let messages = app.world().resource::<Messages<PumpFinished>>();
        let mut cursor = messages.get_cursor();
        let reported: Vec<&PumpFinished> = cursor.read(messages).collect();

        assert_eq!(reported.len(), 1, "exactly one report per pump");
        assert_eq!(
            reported[0].entity, entity,
            "naming the entity that carried it"
        );
        assert!(
            reported[0].result.is_ok(),
            "a WAV sink over a temp file should finalize cleanly: {:?}",
            reported[0].result
        );
    }

    /// Registering the same frame type twice drains once.
    ///
    /// `add_systems` does not deduplicate, so a host and a library plugin both
    /// asking for `<f32, 2>` would otherwise schedule two drains — and two
    /// `PumpFinished` reports for one pump.
    #[test]
    fn registering_a_frame_type_twice_still_drains_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dedup.wav");

        let mut app = App::new();
        app.add_audio_pump::<f32>();
        app.add_audio_pump::<f32>(); // the duplicate a second plugin would add

        app.world_mut().spawn(AudioPump::start(
            SliceSource {
                frames: frames(500),
                pos: 0,
            },
            sink(&path),
            Samples(256),
        ));

        assert!(run_until_drained(&mut app, 200), "the pump should finish");

        let messages = app.world().resource::<Messages<PumpFinished>>();
        let mut cursor = messages.get_cursor();
        assert_eq!(
            cursor.read(messages).count(),
            1,
            "one pump must report once however many times its frame type was registered"
        );
    }

    /// The README's registration + spawn shape, compiled.
    ///
    /// A README example that does not build is the defect this whole commit is
    /// about — the file documented a recording API that never existed. Pinning the
    /// shape here means the next rename breaks a test rather than the docs.
    #[test]
    fn the_documented_shape_compiles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("documented.wav");

        let mut app = App::new();
        app.add_audio_pump::<f32>();

        let src = SliceSource {
            frames: frames(128),
            pos: 0,
        };
        let wav = WavOut::create(&path, SAMPLE_RATE, ChannelLayout::STEREO, BitDepth::Float32)
            .expect("could not create WAV");
        let pump = app
            .world_mut()
            .spawn(AudioPump::start(src, wav, Samples(1024)))
            .id();

        // The stop path a host writes, through the component.
        if let Some(p) = app.world().entity(pump).get::<AudioPump<f32>>() {
            p.stop();
        }

        assert!(
            run_until_drained(&mut app, 200),
            "the documented pump drains"
        );
    }
}

/// Recording the master output: the capture path that had no expression.
///
/// `AudioTap` is the engine's lock-free copy of what reaches the speakers, and
/// `AudioPump` is the ECS-owned pump that drives a source into a sink. Both
/// shipped, and "record what I am hearing" still did not compile: `open()` hands
/// back a bare `HeapCons<(f32, f32)>`, and `AudioPump::start` wants an
/// `AudioIn`. The bound simply did not hold.
///
/// `TapIn` is the join. These pin that it holds, and that audio survives the
/// trip — a type that satisfied the bound but dropped every frame would compile
/// just as well.
/// (Was `tests/master_record.rs`.)
mod master_record {
    use std::path::PathBuf;

    use bevy_app::prelude::*;
    use bevy_tutti::graph::{AudioPump, AudioPumpAppExt, AudioTapRes};
    use bevy_tutti::io::{BitDepth, ChannelLayout, Samples, TapIn, WavOut};

    const SAMPLE_RATE: f64 = 48_000.0;

    fn sink(path: &PathBuf) -> WavOut {
        WavOut::create(path, SAMPLE_RATE, ChannelLayout::STEREO, BitDepth::Float32)
            .expect("sink should open")
    }

    /// An app with the stereo-`f32` pump drain registered — no engine, no device.
    fn app() -> App {
        let mut app = App::new();
        app.add_audio_pump::<f32>();
        app
    }

    /// Run frames until every pump has drained, or give up.
    ///
    /// The pump is on a real thread, so the drain needs wall clock to advance.
    fn run_until_drained(app: &mut App, max_frames: usize) -> bool {
        for _ in 0..max_frames {
            app.update();
            if app
                .world_mut()
                .query::<&AudioPump>()
                .iter(app.world())
                .next()
                .is_none()
            {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    /// The headline: a tap feeds a pump, and what the audio callback pushed lands
    /// in the file.
    ///
    /// The push here stands in for `meter_output`, which is what calls
    /// `AudioTap::push` on the real RT path — interleaved stereo, straight from the
    /// output buffer.
    #[test]
    fn what_the_callback_pushed_reaches_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("master.wav");

        let tap = AudioTapRes::default();
        let src = TapIn::new(tap.open().expect("a fresh tap opens"));

        let mut app = app();
        app.world_mut()
            .spawn(AudioPump::start(src, sink(&path), Samples(512)));

        // Play the audio callback: interleaved stereo, the shape `push` takes.
        let block: Vec<f32> = (0..256).flat_map(|i| [i as f32 / 256.0, -1.0]).collect();
        tap.push(&block, 256);

        // Give the pump a pass at the ring before stopping it.
        for _ in 0..4 {
            app.update();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for pump in app.world_mut().query::<&AudioPump>().iter(app.world()) {
            pump.stop();
        }
        assert!(run_until_drained(&mut app, 200), "pump should finish");

        let mut reader = hound::WavReader::open(&path).expect("finalized WAV should be readable");
        assert_eq!(reader.spec().channels, 2);

        let samples: Vec<f32> = reader
            .samples::<f32>()
            .map(|s| s.expect("sample"))
            .collect();
        assert!(
            !samples.is_empty(),
            "the tap fed the pump but nothing was written"
        );

        // Right channel is a constant -1.0, so a channel swap or an off-by-one in
        // the (f32, f32) -> [f32; 2] conversion shows up here rather than passing.
        for (i, pair) in samples.as_chunks::<2>().0.iter().enumerate().take(64) {
            assert!(
                (pair[0] - i as f32 / 256.0).abs() < 1e-6,
                "frame {i} left: expected ramp, got {}",
                pair[0]
            );
            assert!(
                (pair[1] + 1.0).abs() < 1e-6,
                "frame {i} right: expected -1.0, got {}",
                pair[1]
            );
        }
    }

    /// A closed tap starves the pump rather than ending it.
    ///
    /// `TapIn::ON_EMPTY` is `Starved`, so a pump on a silent graph keeps waiting.
    /// Were it `EndOfStream`, a recording armed a moment before playback started
    /// would finalize an empty file and look like a bug in the sink.
    #[test]
    fn a_silent_graph_does_not_end_the_recording() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("silent.wav");

        let tap = AudioTapRes::default();
        let src = TapIn::new(tap.open().expect("a fresh tap opens"));

        let mut app = app();
        app.world_mut()
            .spawn(AudioPump::start(src, sink(&path), Samples(512)));

        // Nothing pushed: the graph is running but silent.
        for _ in 0..6 {
            app.update();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let still_running = app
            .world_mut()
            .query::<&AudioPump>()
            .iter(app.world())
            .next()
            .is_some();
        assert!(
            still_running,
            "an empty tap means 'nothing yet', not 'finished' — the pump must \
             still be waiting"
        );

        for pump in app.world_mut().query::<&AudioPump>().iter(app.world()) {
            pump.stop();
        }
        assert!(run_until_drained(&mut app, 200), "pump should finish");
    }
}

/// The live I/O edge composes with the graph reconciler.
///
/// `crate::io` re-exports engine types without wrapping them, on the claim that
/// they are already the right shape for a host to hold directly. That claim is
/// only worth anything if a `MicMonitorNode` really does reconcile like any
/// other node — declared through `PortSources`/`MasterSources`, reached by
/// `Net::output_source`, and carrying audio once wired.
///
/// These read the engine back rather than trusting the component, for the same
/// reason `graph_wire.rs` does: the diff this layer performs is only meaningful
/// if the engine is what gets compared against.
/// (Was `tests/io_graph_composition.rs`.)
mod io_graph_composition {
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;
    use ringbuf::traits::{Producer as _, Split as _};

    use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin, MasterSources, PortSources};
    use bevy_tutti::io::{MicMonitorNode, MicRing};
    use bevy_tutti::AudioEngineState;
    use tutti_core::dsp::{pass, Net, Source};
    use tutti_core::AudioNode;
    use tutti_core::AudioUnit as _;

    /// An app wired the way `build_into` leaves one, minus the audio device.
    /// Same shape as `graph_wire.rs`'s harness — deliberately, so a difference in
    /// outcome is about the node under test and not the scaffolding.
    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes(Net::with_backend(2)));
        app.insert_resource(AudioEngineState::Running);
        app.add_plugins(GraphReconcilePlugin);
        app
    }

    /// A monitor node plus the producer end of the ring feeding it, so a test can
    /// play the part of the capture callback without a device.
    fn monitor_with_feed(capacity: usize) -> (MicMonitorNode, ringbuf::HeapProd<[f32; 2]>) {
        let (prod, cons) = ringbuf::HeapRb::<[f32; 2]>::new(capacity).split();
        let ring: MicRing = tutti_io::share_mic_ring(cons);
        (MicMonitorNode::new(ring), prod)
    }

    fn node_id(app: &App, entity: Entity) -> tutti_core::NodeId {
        app.world().get::<AudioNode>(entity).expect("AudioNode").0
    }

    /// Render `frames` from the graph, per-sample.
    ///
    /// `tick` rather than a `process` block for the reason `midi_soundfont_audio.rs`
    /// gives: `BufferVec` holds one SIMD block per channel, so `process` needs
    /// fundsp's buffer types rather than plain slices. `tick` polls the same units.
    fn render(app: &mut App, frames: usize) -> Vec<[f32; 2]> {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        (0..frames)
            .map(|_| {
                let mut frame = [0.0f32; 2];
                graph.0.tick(&[], &mut frame);
                frame
            })
            .collect()
    }

    /// The composition claim, at its narrowest: a monitor node is declared to the
    /// master exactly like any other node, and the reconciler wires it.
    ///
    /// If this fails, `io`'s "no wrapper needed" premise is wrong — the type would
    /// need adapter-side help to reach the graph at all.
    #[test]
    fn a_monitor_node_reaches_the_master_like_any_other_node() {
        let mut app = app();
        let (monitor, _prod) = monitor_with_feed(64);

        let id = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.0.add(monitor)
        };
        let entity = app.world_mut().spawn(AudioNode(id)).id();
        app.insert_resource(MasterSources::from(entity));
        app.update();

        let mon_id = node_id(&app, entity);
        for ch in 0..2 {
            assert_eq!(
                app.world().resource::<AudioGraphRes>().0.output_source(ch),
                Source::Local(mon_id, ch),
                "master channel {ch} must read from the monitor node"
            );
        }
    }

    /// A monitor can sit *upstream of an effect* rather than only at the master —
    /// the "through effects if you like" the mic docs promise.
    ///
    /// Asserted through `Net::source` on the effect's input port, which is what
    /// makes this about the fan-in declaration and not just about the master.
    #[test]
    fn a_monitor_node_can_feed_an_effect_chain() {
        let mut app = app();
        let (monitor, _prod) = monitor_with_feed(64);

        let (mon_id, fx_id) = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            (graph.0.add(monitor), graph.0.add(pass()))
        };
        let mon = app.world_mut().spawn(AudioNode(mon_id)).id();
        let fx = app.world_mut().spawn(AudioNode(fx_id)).id();

        app.world_mut()
            .entity_mut(fx)
            .insert(PortSources::from(mon));
        app.insert_resource(MasterSources::from(fx));
        app.update();

        assert_eq!(
            app.world().resource::<AudioGraphRes>().0.source(fx_id, 0),
            Source::Local(mon_id, 0),
            "the effect's input must read from the monitor"
        );
    }

    /// Frames pushed by a (simulated) capture callback come out of the graph.
    ///
    /// The two tests above pin the *topology*; this one pins that the topology
    /// carries audio. Without it, a monitor wired to a ring it never drains would
    /// pass both of them while producing silence — which is exactly the trap
    /// `io`'s module docs warn about, so it deserves an assertion rather than a
    /// paragraph.
    #[test]
    fn frames_pushed_by_the_capture_callback_reach_the_graph_output() {
        let mut app = app();
        let (monitor, mut prod) = monitor_with_feed(512);

        let id = {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            graph.0.add(monitor)
        };
        let entity = app.world_mut().spawn(AudioNode(id)).id();
        app.insert_resource(MasterSources::from(entity));
        app.update();

        // Play the capture callback: push a constant, distinguishable frame.
        for _ in 0..128 {
            prod.try_push([0.5, -0.5]).expect("ring has room");
        }

        let out = render(&mut app, 64);

        assert!(
            out.iter().all(|f| (f[0] - 0.5).abs() < 1e-6),
            "left must carry the pushed frames, got {:?}",
            &out[..4]
        );
        assert!(
            out.iter().all(|f| (f[1] + 0.5).abs() < 1e-6),
            "right must carry the pushed frames, got {:?}",
            &out[..4]
        );
    }

    /// An *undeclared* monitor is silently dropped — the first trap `io`'s docs
    /// name, pinned as behaviour so the warning cannot quietly stop being true.
    ///
    /// The node is in the graph but nothing reads it, so the master stays silent.
    /// This is the failure mode that looks like a connected monitor and produces
    /// nothing.
    #[test]
    fn an_undeclared_monitor_produces_silence_at_the_master() {
        let mut app = app();
        let (monitor, mut prod) = monitor_with_feed(512);

        {
            let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
            // Added to the graph — but never declared to `MasterSources`, which is
            // the step a host forgets.
            let _ = graph.0.add(monitor);
        }
        app.update();

        for _ in 0..128 {
            prod.try_push([0.5, -0.5]).expect("ring has room");
        }

        let out = render(&mut app, 64);

        assert!(
            out.iter().all(|f| f[0] == 0.0 && f[1] == 0.0),
            "an undeclared monitor must not reach the master — if this fails the \
             trap documented in `io`'s module docs has changed shape"
        );
    }
}
