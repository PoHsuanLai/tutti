//! Cancelling an export through the adapter, on a pool with threads of its
//! own (the render must be running when the cancel arrives).
//!
//! Run under `bevy_tasks/multi_threaded`, as CI's threaded step does
//! (`test(/^off_thread::/)`); on the default pool a render finishes inside the
//! frame that starts it, before anything can cancel it, and these return.

#![cfg(all(feature = "export", feature = "wav"))]

mod off_thread {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex};

    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;
    use bevy_tutti::export::{
        ExportCancelled, ExportClock, ExportDone, ExportInFlight, ExportPlugin, ExportRequest,
        ExportSource, ExportTarget,
    };
    use bevy_tutti::graph::{AudioConfig, AudioGraphRes, GraphSource};
    use tutti_export::{
        AudioFormat, BitDepth, ChannelLayout, EncodeConfig, ExportConfig, RenderConfig,
    };
    use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
    use tutti_types::{SampleRate, Samples};

    const RATE: f64 = 48_000.0;
    /// Blocks the render is long (1024 frames each).
    const BLOCKS: usize = 40;

    /// Whether the compute pool runs tasks on threads of its own.
    fn threaded() -> bool {
        let main = std::thread::current().id();
        let task =
            bevy_tasks::AsyncComputeTaskPool::get().spawn(async { std::thread::current().id() });
        bevy_tasks::block_on(task) != main
    }

    /// Counts the blocks it renders, and at its second tells the test it is
    /// mid-render and waits for the test to let it go on.
    #[derive(Clone)]
    struct Hold {
        blocks: Arc<AtomicUsize>,
        reached: Sender<()>,
        go: Arc<Mutex<Receiver<()>>>,
    }

    impl Node for Hold {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            io.output(0).fill(0.25);
            if self.blocks.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                let _ = self.reached.send(());
                let _ = self.go.lock().unwrap().recv();
            }
            Status::Modified
        }
        fn reset(&mut self) {}
    }

    fn request(target: ExportTarget) -> ExportRequest {
        ExportRequest::new(
            ExportSource::Mix,
            target,
            ExportConfig {
                render: RenderConfig {
                    sample_rate: SampleRate(RATE),
                    duration_seconds: (BLOCKS * 1024) as f64 / RATE,
                    ..Default::default()
                },
                encode: EncodeConfig {
                    format: AudioFormat::Wav,
                    bit_depth: BitDepth::Float32,
                    channels: ChannelLayout::STEREO,
                    topology: None,
                },
                ..Default::default()
            },
            ExportClock::frozen(),
        )
    }

    struct Rig {
        app: App,
        blocks: Arc<AtomicUsize>,
        reached: Receiver<()>,
        go: Sender<()>,
    }

    fn rig() -> Rig {
        let mut graph = AudioGraphRes::headless(0, 2);
        graph.graph().set_sample_rate(SampleRate(RATE));
        let blocks = Arc::new(AtomicUsize::new(0));
        let (reached_tx, reached) = channel();
        let (go, go_rx) = channel();
        let (node, _) = graph.graph().insert(ForkByClone(Hold {
            blocks: Arc::clone(&blocks),
            reached: reached_tx,
            go: Arc::new(Mutex::new(go_rx)),
        }));
        graph
            .graph()
            .set_output_source(0, GraphSource::Node(node, 0));
        graph
            .graph()
            .set_output_source(1, GraphSource::Node(node, 0));
        let mut app = App::new();
        app.add_plugins((bevy_app::TaskPoolPlugin::default(), ExportPlugin));
        app.insert_resource(graph);
        app.insert_resource(AudioConfig {
            sample_rate: SampleRate(RATE),
            channels: ChannelLayout::STEREO,
        });
        app.insert_resource(bevy_tutti::AudioEngineState::Running);
        Rig {
            app,
            blocks,
            reached,
            go,
        }
    }

    /// Starts `request` and returns once its render holds at its second
    /// block.
    fn start_held(r: &mut Rig, request: ExportRequest) -> Entity {
        let entity = r.app.world_mut().spawn(request).id();
        r.app.update();
        r.reached
            .recv()
            .expect("the render reached its second block");
        entity
    }

    /// Spawns a second export and updates until it reports; its result.
    fn run_next(r: &mut Rig) -> bool {
        let done = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&done);
        r.app
            .world_mut()
            .spawn(request(ExportTarget::Buffers))
            .observe(move |d: On<ExportDone>| *seen.lock().unwrap() = Some(d.result.is_ok()));
        for _ in 0..100_000 {
            r.app.update();
            if let Some(ok) = *done.lock().unwrap() {
                return ok;
            }
            std::thread::yield_now();
        }
        panic!("the next export never reported");
    }

    /// A cancelled export reads its progress while it runs, stops its render
    /// at the cancel, reports `ExportCancelled` (not `ExportDone`), leaves no
    /// file, and frees the session's token: the next export runs without
    /// waiting for the cancelled one's 40 blocks.
    ///
    /// Mutation (run): `ExportInFlight::cancel` doing nothing -> the render
    /// runs to its end (80 blocks with the next export's) and nothing is
    /// reported cancelled; fails.
    /// Mutation (run): `poll_exports` triggering `ExportDone` for a cancelled
    /// outcome -> the cancelled flag stays unset; fails.
    #[test]
    fn a_cancelled_export_stops_rendering_and_frees_the_session() {
        let mut r = rig();
        if !threaded() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        let events = Arc::new(Mutex::new((false, false)));
        let entity = start_held(
            &mut r,
            request(ExportTarget::File {
                path: path.clone(),
                normalize: None,
            }),
        );
        let (cancelled, finished) = (Arc::clone(&events), Arc::clone(&events));
        r.app
            .world_mut()
            .entity_mut(entity)
            .observe(move |_: On<ExportCancelled>| cancelled.lock().unwrap().0 = true)
            .observe(move |_: On<ExportDone>| finished.lock().unwrap().1 = true);

        let in_flight = r.app.world().get::<ExportInFlight>(entity).unwrap();
        let reading = in_flight.progress().expect("one block ticked");
        assert_eq!(reading.produced, Samples(1024), "held inside block 2");
        assert_eq!(reading.total, Samples(BLOCKS * 1024));
        assert_eq!((reading.pass.index(), reading.pass.count()), (0, 1));
        in_flight.cancel();
        r.go.send(()).unwrap();

        assert!(run_next(&mut r), "the next export ran");
        assert_eq!(
            *events.lock().unwrap(),
            (true, false),
            "cancelled, not done"
        );
        assert!(!path.exists(), "no partial file");
        assert!(
            !r.app.world().entity(entity).contains::<ExportInFlight>(),
            "no longer in flight"
        );
        // The first stopped within a block or two of the cancel; the second
        // rendered all of its own.
        assert!(
            r.blocks.load(Ordering::SeqCst) <= 3 + BLOCKS,
            "the cancelled render ran on ({} blocks)",
            r.blocks.load(Ordering::SeqCst)
        );
    }

    /// Despawning the request cancels it the same way.
    ///
    /// Mutation (run): the `Drop` of `ExportInFlight` removed -> the despawned
    /// render runs to its end; fails.
    #[test]
    fn a_despawned_export_stops_rendering_too() {
        let mut r = rig();
        if !threaded() {
            return;
        }
        let entity = start_held(&mut r, request(ExportTarget::Buffers));
        r.app.world_mut().entity_mut(entity).despawn();
        r.go.send(()).unwrap();
        assert!(run_next(&mut r), "the next export ran");
        assert!(r.blocks.load(Ordering::SeqCst) <= 3 + BLOCKS);
    }
}
