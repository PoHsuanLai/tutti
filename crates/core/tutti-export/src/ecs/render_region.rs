//! Offline render of a single graph node over a beat range.
//!
//! Spectral (and any "what does this point in the graph actually sound like"
//! consumer) needs the audio *at a tap point*, post-everything-upstream — not
//! a raw source file. This module renders exactly that: spawn an entity
//! with [`StartRegionRender`] naming a `NodeId` and a beat range; the start
//! system clones the live net, repoints its output bus at that node (via
//! [`Net::clone_isolated`]), and renders it offline on a worker
//! thread. When the render finishes, the result lands on the same entity as a
//! [`RegionRenderComplete`] component carrying the PCM.
//!
//! Uses the standard message → spawn-task → poll-completion pattern, with two
//! notable traits: the node isolation, and the
//! render runs on the shared [`AsyncComputeTaskPool`] via the `to_buffers`
//! (in-memory) terminal — a bounded, Bevy-managed pool rather than `Run::spawn`'s
//! raw OS thread, so it cannot pin every core and starve the real-time audio
//! callback.
//!
//! ## Voice population is a downstream hole
//!
//! `Prepare` replaces every voice pool in the clone with a fresh, channel-less
//! [`VoicePool::detached`], so the render shares no voice state — and
//! crucially no live command `Receiver` — with the audio thread. That leaves the
//! readers **empty**, so the render runs in three ordered steps
//! ([`RegionRenderSystems`]): this crate clones + isolates + swaps in fresh
//! readers bound to the offline transport (`Prepare`), a voice-aware downstream
//! crate fills them from ECS (`Populate`), then this crate hands the net to the
//! worker (`Spawn`). bevy-tutti stays voice-vocabulary-free; only the middle step
//! knows what a voice is.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_tasks::{AsyncComputeTaskPool, Task};
use std::sync::Arc;

use crate::{Error as ExportError, Rendered};
use bevy_tasks::{block_on, futures_lite::future};
use tutti_core::dsp::Net;
use tutti_core::NodeId;
use tutti_core::{
    AudioUnit, OfflineTimeline, OfflineTimelineConfig, SampleRate, Timeline, TransportClock,
};

use tutti_core::ecs::engine_ready;
use tutti_core::ecs::{AudioConfig, AudioGraphRes};
use tutti_sampler::VoiceNode;
use tutti_sampler::VoicePool;

/// Ordering anchor for the three-step region render. A voice-aware downstream
/// crate schedules its voice-population system in [`Self::Populate`]; this crate
/// owns the surrounding `Prepare` (clone + isolate + rebind) and `Spawn`
/// (hand the filled net to the worker) steps. The set is `.chain()`ed so the
/// empty-but-isolated net is always filled before it reaches the worker.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum RegionRenderSystems {
    Prepare,
    Populate,
    Spawn,
    /// Drain finished render tasks. Chained *after* `Spawn` so a render that
    /// completes this frame frees its slot before next frame's `Prepare` counts
    /// in-flight renders against [`RegionRenderConfig::max_in_flight`] — without
    /// it the throttled backlog would drain one frame slower per render.
    Poll,
}

/// Admission policy for offline region renders.
///
/// Each admitted [`StartRegionRender`] does one **main-thread** deep clone of
/// the live net (`clone_isolated` → `DynClone` of every DSP node) in
/// [`prepare_region_render_system`]. When a consumer enters a mode that taps
/// many nodes at once (e.g. spectral view, one tap per track), every view
/// misses its cache and spawns a request in the *same frame*; admitting them all
/// would run N clones serialized on the main thread, stalling the frame long
/// enough to underrun the realtime audio callback (audible glitch).
///
/// `max_in_flight` caps how many renders occupy a slot
/// ([`RegionRenderNet`] parked for `Populate`, or [`RegionRenderInProgress`]
/// running on the pool) at once, so the per-frame clone burst is bounded and the
/// backlog drains as a trickle. Default `1`.
///
/// We cap admissions rather than move the clone off-thread: the clone borrows
/// `&self` on the graph resource, which cannot cross into a `'static` task, and
/// the snapshot needed to work around that *is itself* the deep clone — so
/// off-thread cloning buys nothing once the burst is throttled. Revisit only if
/// one clone per frame proves too costly for very large graphs.
#[derive(Resource, Debug, Clone, Copy)]
pub struct RegionRenderConfig {
    pub max_in_flight: usize,
}

impl Default for RegionRenderConfig {
    fn default() -> Self {
        Self { max_in_flight: 1 }
    }
}

/// Make the cloned net hermetic and bind it to the render's offline timeline.
///
/// Two concerns, kept distinct:
///
/// 1. **Sever shared live inputs** ([`AudioUnit::isolate`]) — generic, no DAW
///    vocabulary. Every node gets `isolate()`d; units that share a live inbox
///    by `Arc` (synths' MIDI receiver, …) mint fresh dead state there, so the
///    worker thread can't drain events the live graph needs. Pure-DSP units
///    no-op. This is the half that fixes the cross-thread theft, and it scales:
///    a new shared-input node declares its own `isolate()` and is covered here
///    automatically — the render never type-switches on it.
///
/// 2. **Re-point at offline data** — inherently external (needs the render's
///    transport / ECS voices), so it stays an explicit per-type step:
///    - **Voice pools** ([`VoicePool`]) are severed by the generic
///      `isolate()` above — it drops the live command channel and clears the
///      cloned voices, leaving the reader born empty and channel-less — and then
///      just re-pointed at the offline transport here. The render's voices are
///      rebuilt from ECS in the `Populate` step via
///      [`VoicePool::insert_voice`].
///    - **Bare standalone voices** ([`VoiceNode`]) keep their cloned content
///      (their clone is already independent) and are just re-pointed at the
///      offline transport so they read the render's playhead, not the (undriven)
///      live one. `dawai-spectral`'s resynth is the sole producer of these bare
///      voice nodes; this arm and that producer move together (a mismatch would
///      silently render the correction with the live transport — see the
///      `offline_rebinds_bare_voice_node_transport` test).
fn rebind_net_transport(
    net: &mut Net,
    transport: &Arc<dyn Timeline>,
    start_beat: f64,
    tempo: impl Into<tutti_core::Bpm>,
) {
    let tempo = tempo.into();
    let ids: Vec<NodeId> = net.ids().copied().collect();
    for id in ids {
        // 1b. The clock is severed by `isolate()` but keeps whatever beat the
        //     LIVE playhead happened to be at. Re-seat it on the render's own
        //     timeline, or every edge-driven node (LFO, AutomationLane) renders
        //     from the wrong beat. This is the beat-signal counterpart of the
        //     `Arc` rebinds below.
        if let Some(clock) = net
            .node_mut(id)
            .as_any_mut()
            .downcast_mut::<TransportClock>()
        {
            *clock = clock.at_tempo(tempo).starting_at(start_beat);
            continue;
        }

        // 2. Type-specific transport re-point. `isolate()` already severed the
        //    live inputs (the reader's command channel + voices); all that's left
        //    is to aim each transport-aware unit at the render's offline
        //    playhead. Voice pools and bare voices both carry a transport.
        let node = net.node_mut(id);
        if let Some(reader) = node.as_any_mut().downcast_mut::<VoicePool>() {
            reader.replace_transport(transport.clone());
        } else if let Some(voice) = node.as_any_mut().downcast_mut::<VoiceNode>() {
            voice.replace_transport(transport.clone());
        }
    }
}

/// Trigger component: spawn an entity with this to render `target`'s output
/// over `[start_beat, start_beat + len_beats]` at `tempo`.
///
/// The start system consumes this and replaces it with
/// [`RegionRenderInProgress`]; when the worker finishes, that becomes
/// [`RegionRenderComplete`].
///
/// `priority` orders admission when more requests are pending than the
/// [`RegionRenderConfig::max_in_flight`] cap allows: higher admits first. A
/// consumer that taps many nodes at once (spectral view, one per track) sets the
/// focused/visible view higher so it paints first; peers default to `0`.
#[derive(Component, Debug, Clone)]
pub struct StartRegionRender {
    pub target: NodeId,
    pub start_beat: f64,
    pub len_beats: f64,
    pub tempo: f64,
    pub priority: i32,
}

/// In-flight region render. Holds the off-thread render task the poll system
/// drives each frame. The render runs on the shared [`AsyncComputeTaskPool`]
/// (bounded, Bevy-managed) rather than an unbounded raw OS thread, so a render
/// — even several at once — can't pin every core and starve the real-time
/// audio callback. Not `Reflect`: `Task` is foreign to `bevy_reflect`.
#[derive(Component)]
pub struct RegionRenderInProgress {
    task: Task<Result<Rendered, ExportError>>,
    target: NodeId,
    start_beat: f64,
    len_beats: f64,
    tempo: f64,
}

/// Rendered PCM for a region, attached to the requesting entity on completion.
///
/// `samples_*` are the isolated target's stereo output. Consumers (spectral)
/// copy these into their own `RenderedRegion` keyed by target; the `Arc` keeps
/// the buffers cheap to share with the analysis + re-inject stages.
#[derive(Component, Debug, Clone)]
pub struct RegionRenderComplete {
    pub samples_l: Arc<[f32]>,
    pub samples_r: Arc<[f32]>,
    pub sample_rate: f64,
    pub target: NodeId,
    pub start_beat: f64,
    pub len_beats: f64,
    pub tempo: f64,
}

/// Attached instead of [`RegionRenderComplete`] when the render fails (e.g. the
/// target has no outputs, or the worker panicked).
#[derive(Component, Debug, Clone)]
pub struct RegionRenderFailed {
    pub target: NodeId,
    pub error: String,
}

/// Carrier between [`prepare_region_render_system`] and
/// [`spawn_region_render_system`]: the isolated, transport-rebound clone of the
/// live net, parked on the request entity while the `Populate` step fills its
/// voice pools from ECS. Not `Reflect`: `Net` is foreign to `bevy_reflect`.
#[derive(Component)]
pub struct RegionRenderNet {
    net: Net,
    /// The offline transport the voice pools (and the export) are bound to.
    /// Public so the `Populate` step binds freshly-built samplers to the same
    /// timeline.
    pub transport: Arc<OfflineTimeline>,
    pub target: NodeId,
    pub start_beat: f64,
    pub len_beats: f64,
    pub tempo: f64,
}

impl RegionRenderNet {
    /// Mutable access to a node in the cloned net, for the `Populate` step to
    /// downcast its voice pools and insert voices.
    pub fn node_mut(&mut self, node: NodeId) -> &mut dyn AudioUnit {
        self.net.node_mut(node)
    }
}

/// Step 1 (`Prepare`): clone + isolate the live net at `target`, build the
/// offline transport, rebind every transport-aware unit (and detach the voice
/// readers' command channels), then park it as a [`RegionRenderNet`]. Clip
/// population happens downstream in `Populate`; the worker spawns in `Spawn`.
/// A render slot is occupied by either a parked net (`RegionRenderNet`) or a
/// running worker (`RegionRenderInProgress`).
type RegionRenderSlotFilter = Or<(With<RegionRenderNet>, With<RegionRenderInProgress>)>;

pub fn prepare_region_render_system(
    mut commands: Commands,
    graph: Res<AudioGraphRes>,
    config: Res<AudioConfig>,
    render_config: Res<RegionRenderConfig>,
    // Both a parked-for-Populate net and a running task occupy a slot — the
    // expensive clone has already happened for either.
    in_flight: Query<(), RegionRenderSlotFilter>,
    // No `Added<>`: a request we decline this frame (over the cap) must still
    // match next frame. Presence of `StartRegionRender` *is* the "pending" flag;
    // `remove`ing it on admit below is what marks it admitted. We admit in
    // descending `priority` order so a consumer can paint its focused tap first
    // (peers tie at 0 — Bevy archetype order among them, don't assume FIFO).
    query: Query<(Entity, &StartRegionRender)>,
) {
    // Count slots as of frame start, then track admissions locally: renders
    // inserted via `commands` this frame aren't visible to `in_flight` until the
    // next command-buffer flush, so without the local counter we'd admit a full
    // `max_in_flight` *every* frame.
    let mut occupied = in_flight.iter().count();

    // Highest priority first. Cheap: at most one request per tapped node, and
    // we only sort when something is pending.
    let mut pending: Vec<(Entity, &StartRegionRender)> = query.iter().collect();
    pending.sort_by_key(|(_, s)| std::cmp::Reverse(s.priority));

    for (entity, start) in pending {
        if occupied >= render_config.max_in_flight {
            break; // leave StartRegionRender in place; retry next frame
        }

        let mut ecmd = commands.entity(entity);
        ecmd.remove::<StartRegionRender>();

        // Named Tracy zones (drop guards tightly so each step is a distinct
        // zone): the clone is the suspected main-thread stall — a deep clone of
        // the whole live net (DynClone every DSP node) — but `reset` and
        // `rebind` also walk every node, so measure all three separately to see
        // which dominates the spectral-entry glitch.
        let clone = {
            let _span = bevy_log::info_span!("region_render::clone_isolated").entered();
            graph.0.clone_isolated(start.target)
        };
        // `clone_isolated` hands back a `PendingClone`, which implements
        // nothing — isolating is the only way to get a usable `Net`, so the
        // clone cannot be reset or ticked while it still shares live state.
        let Some(mut net) = clone.map(|pending| pending.isolate()) else {
            // No-output target never occupied a slot — don't count it.
            ecmd.insert(RegionRenderFailed {
                target: start.target,
                error: "target node has no outputs".into(),
            });
            continue;
        };

        // Reset every DSP node's internal state (filter memory, reverb tails,
        // delay lines). The clone inherited the live nodes' state as-of clone
        // time; rendering from that would make the result depend on *when* the
        // user entered spectral mode (a reverb tail mid-decay) — nondeterministic
        // and cache-breaking (same cone-hash, different audio). A spectral tap is
        // "what this node sounds like over [start_beat, …] from a clean start",
        // so we always run FX from reset state.
        {
            let _span = bevy_log::info_span!("region_render::net_reset").entered();
            net.reset();
        }

        // The offline transport the render advances. The clone's samplers still
        // point at the live transport (which the offline driver never drives),
        // so we rebind every transport-aware unit to this one — without it the
        // voice sources read a stale playhead and render silence. The export is
        // handed the same transport in `Spawn`, tying both ends together.
        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: start.start_beat,
            tempo: start.tempo.into(),
            sample_rate: SampleRate(config.sample_rate),
            loop_range: None,
        }));
        let reader_transport: Arc<dyn Timeline> = timeline.clone();
        {
            let _span = bevy_log::info_span!("region_render::rebind_net_transport").entered();
            rebind_net_transport(&mut net, &reader_transport, start.start_beat, start.tempo);
        }

        ecmd.insert(RegionRenderNet {
            net,
            transport: timeline,
            target: start.target,
            start_beat: start.start_beat,
            len_beats: start.len_beats,
            tempo: start.tempo,
        });
        occupied += 1; // a real slot is now occupied
    }
}

/// Step 3 (`Spawn`): the voice pools are now populated; hand the net to the
/// offline export worker and swap [`RegionRenderNet`] for
/// [`RegionRenderInProgress`].
pub fn spawn_region_render_system(
    mut commands: Commands,
    config: Res<AudioConfig>,
    mut query: Query<(Entity, &mut RegionRenderNet), Added<RegionRenderNet>>,
) {
    for (entity, mut render) in query.iter_mut() {
        // Move the net out of the component (it goes to the worker by value).
        let net = std::mem::replace(&mut render.net, Net::new(0, 0));
        let timeline = render.transport.clone();

        // Configure the export, then run it on the shared compute pool instead
        // of `Run::spawn`'s raw OS thread. `Run::run()` is a synchronous
        // `FnOnce(..) -> Result<_> + Send`, so it executes fine inside a task;
        // running it on the bounded pool (the same one the STFT step and the
        // wave cache use) keeps the render from starving the audio callback.
        let run = crate::Export::graph(net, config.sample_rate)
            .start_beat(render.start_beat)
            .duration_beats(render.len_beats, render.tempo)
            .transport(timeline)
            .to_buffers();
        let task = AsyncComputeTaskPool::get().spawn(async move { run.run() });

        commands
            .entity(entity)
            .remove::<RegionRenderNet>()
            .insert(RegionRenderInProgress {
                task,
                target: render.target,
                start_beat: render.start_beat,
                len_beats: render.len_beats,
                tempo: render.tempo,
            });
    }
}

pub fn region_render_poll_system(
    mut commands: Commands,
    mut query: Query<(Entity, &mut RegionRenderInProgress)>,
) {
    for (entity, mut render) in query.iter_mut() {
        // Non-blocking poll of the off-thread render task via the B0 helper
        // (same convention every Tutti subsystem follows). `None` → still
        // running; poll again next frame.
        let Some(result) = block_on(future::poll_once(&mut render.task)) else {
            continue;
        };
        match result {
            Ok(rendered) => {
                let complete = RegionRenderComplete {
                    samples_l: Arc::from(rendered.left),
                    samples_r: Arc::from(rendered.right),
                    sample_rate: rendered.sample_rate,
                    target: render.target,
                    start_beat: render.start_beat,
                    len_beats: render.len_beats,
                    tempo: render.tempo,
                };
                commands
                    .entity(entity)
                    .remove::<RegionRenderInProgress>()
                    .insert(complete);
            }
            Err(error) => {
                let target = render.target;
                commands
                    .entity(entity)
                    .remove::<RegionRenderInProgress>()
                    .insert(RegionRenderFailed {
                        target,
                        error: error.to_string(),
                    });
            }
        }
    }
}

/// Bevy plugin: offline per-node region render.
pub struct TuttiRegionRenderPlugin;

impl Plugin for TuttiRegionRenderPlugin {
    fn build(&self, app: &mut App) {
        use RegionRenderSystems::{Poll, Populate, Prepare, Spawn};
        app.init_resource::<RegionRenderConfig>()
            .configure_sets(Update, (Prepare, Populate, Spawn, Poll).chain())
            .add_systems(
                Update,
                (
                    prepare_region_render_system
                        .in_set(Prepare)
                        .run_if(engine_ready),
                    spawn_region_render_system
                        .in_set(Spawn)
                        .run_if(engine_ready),
                    region_render_poll_system.in_set(Poll),
                ),
            );
        // `Populate` is intentionally left empty here — a voice-aware downstream
        // crate (dawai-spectral) fills the cloned net's readers in that slot.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tutti_core::Beat;
    use tutti_core::{BeatDuration, Bpm, SampleRate, Wave};
    use tutti_sampler::{
        Direction, MemorySource, Playback, SlotId, Voice, VoiceCommand, VoicePool, VoiceSource,
    };

    struct MockTransport {
        playing: AtomicBool,
        beat: AtomicU64,
        tempo: AtomicU64,
    }
    impl MockTransport {
        fn new(playing: bool) -> Arc<Self> {
            Arc::new(Self {
                playing: AtomicBool::new(playing),
                beat: AtomicU64::new(0.0f64.to_bits()),
                tempo: AtomicU64::new(120.0f64.to_bits()),
            })
        }
    }
    impl Timeline for MockTransport {
        fn is_rolling(&self) -> bool {
            self.playing.load(Ordering::Relaxed)
        }
        fn beat(&self) -> tutti_core::Beat {
            tutti_core::Beat(f64::from_bits(self.beat.load(Ordering::Relaxed)))
        }
        fn tempo(&self) -> Bpm {
            Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
        }
    }

    /// `rebind_net_transport` must replace each voice reader in the cloned net
    /// with a fresh, empty, channel-less reader — and must NOT disturb the live
    /// reader's command channel (no command theft from the audio thread).
    #[test]
    fn rebind_swaps_fresh_reader_and_does_not_steal_live_commands() {
        let live_transport = MockTransport::new(true);

        // A live reader + its handle, placed in a net feeding the output.
        let (reader, handle) = VoicePool::with_transport(live_transport.clone(), None);
        let mut net = Net::new(0, 2);
        let id = net.push(Box::new(reader));
        net.pipe_output(id);

        // Clone the net (as the render does) and rebind it to an offline
        // transport — this should swap the cloned reader for a fresh one.
        let offline = MockTransport::new(true) as Arc<dyn Timeline>;
        let mut clone = net.clone();
        rebind_net_transport(&mut clone, &offline, 0.0, tutti_core::Bpm(120.0));

        let cloned_reader = clone
            .node_mut(id)
            .as_any_mut()
            .downcast_mut::<VoicePool>()
            .expect("still a voice pool after rebind");
        assert_eq!(
            cloned_reader.voice_count(),
            0,
            "render clone's reader must be born empty"
        );

        // The live handle still feeds the *original* reader, not the clone.
        let wave = Arc::new(Wave::from_samples(
            44100.0,
            &(0..64).map(|i| (i as f32 + 1.0) / 64.0).collect::<Vec<_>>(),
        ));
        let sampler =
            MemorySource::with_transport(wave, live_transport.clone(), Beat::new(0.0), None);
        handle.send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback::default(),
                channel_index: None,
            }),
            // Unstretched: the filter is built by the sender only for a voice
            // that arrives already needing one.
            stretch: None,
        });

        net.set_sample_rate(SampleRate(44100.0));
        net.allocate();
        let live_reader = net
            .node_mut(id)
            .as_any_mut()
            .downcast_mut::<VoicePool>()
            .expect("original is still a voice pool");
        let mut out = [0.0f32; 2];
        live_reader.tick(&[], &mut out); // drains the live channel
        assert_eq!(
            live_reader.voice_count(),
            1,
            "live reader must still receive its commands"
        );

        // And the detached clone, ticked, must NOT have stolen that command.
        let cloned_reader = clone
            .node_mut(id)
            .as_any_mut()
            .downcast_mut::<VoicePool>()
            .unwrap();
        let mut out_clone = [0.0f32; 2];
        cloned_reader.tick(&[], &mut out_clone);
        assert_eq!(
            cloned_reader.voice_count(),
            0,
            "render clone must never receive live commands"
        );
    }

    use bevy_ecs::world::World;
    use tutti_core::dsp::dc;
    use tutti_core::ecs::AudioConfig;

    /// The clock node is severed by `isolate()` but keeps the LIVE playhead's
    /// beat. `rebind_net_transport` must re-seat it on the render's own
    /// timeline — otherwise every edge-driven node (LFO, AutomationLane) reads
    /// the wrong beat for the whole render.
    #[test]
    fn rebind_reseats_the_clock_on_the_render_start_beat() {
        use tutti_core::{AtomicBool, AtomicF64, Ordering};

        let tempo = Arc::new(AtomicF64::new(120.0));
        let paused = Arc::new(AtomicBool::new(false));
        let writeback = Arc::new(AtomicF64::new(0.0));

        let mut net = Net::new(0, 2);
        let live_clock = TransportClock::new(
            tutti_core::transport::ClockLinks {
                tempo: Arc::clone(&tempo),
                paused,
                seek: Default::default(),
                loop_span: None,
                position_writeback: Some(Arc::clone(&writeback)),
                steady_time: None,
            },
            44100.0,
        );
        let clock_id = net.push(Box::new(live_clock));
        net.pipe_output(clock_id);

        // Advance the "live" clock away from 0 so a stale beat is detectable.
        {
            let clock = net
                .node_mut(clock_id)
                .as_any_mut()
                .downcast_mut::<TransportClock>()
                .unwrap();
            let mut out = [0.0f32; 2];
            for _ in 0..44100 {
                clock.tick(&[], &mut out);
            }
            assert!(
                clock.current_beat() > Beat::new(1.0),
                "live clock should have moved"
            );
        }

        let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: 32.0,
            tempo: tutti_core::Bpm(120.0),
            sample_rate: SampleRate(44100.0),
            loop_range: None,
        }));
        let reader: Arc<dyn Timeline> = timeline;
        rebind_net_transport(&mut net, &reader, 32.0, tutti_core::Bpm(120.0));

        let clock = net
            .node_mut(clock_id)
            .as_any_mut()
            .downcast_mut::<TransportClock>()
            .unwrap();
        // `Beat - Beat` yields a `BeatDuration`; `Beat - f64` is deliberately not
        // an operator (a position minus a scalar has no origin — see the omission
        // ledger in units.rs).
        assert!(
            (clock.current_beat() - Beat::new(32.0)).abs() < BeatDuration::new(1e-6),
            "clock not re-seated on the render start beat: {:?}",
            clock.current_beat()
        );

        // And it must no longer write into the live playhead.
        let before = writeback.load(Ordering::Acquire);
        let mut out = [0.0f32; 2];
        for _ in 0..256 {
            clock.tick(&[], &mut out);
        }
        assert_eq!(
            writeback.load(Ordering::Acquire),
            before,
            "render clock stomped the live playhead"
        );
    }

    /// Build a real (tiny, CPAL-free) 2-output graph with one node piped to the
    /// output bus, so `clone_isolated(target)` succeeds in `prepare`.
    fn graph_res_with_one_target() -> (AudioGraphRes, NodeId) {
        let mut graph = Net::with_backend(2);
        let target = graph.master(dc(1.0)); // one node, wired to the output bus
        (AudioGraphRes(graph), target)
    }

    /// With `max_in_flight = 1`, two `StartRegionRender`s spawned in one frame
    /// must admit exactly one — the second stays pending (its `StartRegionRender`
    /// is left in place, NOT lost) — and once the first's slot frees, the second
    /// is admitted on the next run. Guards the two admission traps: persisted
    /// requests must survive (no `Added<>` reliance) and the per-frame local
    /// counter must stop over-admission.
    #[test]
    fn throttle_admits_up_to_cap_then_drains_backlog() {
        let (graph, target) = graph_res_with_one_target();

        let mut world = World::new();
        world.insert_resource(graph);
        world.insert_resource(AudioConfig {
            sample_rate: 48_000.0,
            channels: tutti_core::ChannelLayout::Stereo,
        });
        world.insert_resource(RegionRenderConfig { max_in_flight: 1 });

        let start = |t: NodeId| StartRegionRender {
            target: t,
            start_beat: 0.0,
            len_beats: 4.0,
            tempo: 120.0,
            priority: 0,
        };
        world.spawn(start(target));
        world.spawn(start(target));

        let mut schedule = bevy_ecs::schedule::Schedule::default();
        schedule.add_systems(prepare_region_render_system);

        // First run: exactly one admitted (one RegionRenderNet), one still pending.
        schedule.run(&mut world);
        let admitted = world.query::<&RegionRenderNet>().iter(&world).count();
        let pending = world.query::<&StartRegionRender>().iter(&world).count();
        assert_eq!(
            admitted, 1,
            "cap=1 must admit exactly one render this frame"
        );
        assert_eq!(
            pending, 1,
            "the over-cap request must remain pending, not be lost"
        );

        // Re-running while the slot is still occupied admits nothing more.
        schedule.run(&mut world);
        assert_eq!(
            world.query::<&RegionRenderNet>().iter(&world).count(),
            1,
            "no new admission while the single slot is occupied"
        );

        // Free the slot (simulate the parked net advancing past Populate/Spawn)
        // and re-run: the backlog drains — the second request is now admitted.
        let occupied: Vec<Entity> = world
            .query_filtered::<Entity, With<RegionRenderNet>>()
            .iter(&world)
            .collect();
        for e in occupied {
            world.entity_mut(e).despawn();
        }
        schedule.run(&mut world);
        assert_eq!(
            world.query::<&RegionRenderNet>().iter(&world).count(),
            1,
            "the previously-pending request is admitted once a slot frees"
        );
        assert_eq!(
            world.query::<&StartRegionRender>().iter(&world).count(),
            0,
            "backlog fully drained"
        );
    }

    /// A bare standalone [`VoiceNode`] correction node (as `dawai-spectral`'s
    /// resynth adds) must have its placement transport rebound to the offline
    /// transport by `rebind_net_transport`. This guards the silent-correctness
    /// coupling: resynth is the sole producer of these bare voice nodes, and
    /// this rebind arm is the sole consumer. If the `downcast_mut::<VoiceNode>()`
    /// arm ever stops matching resynth's node type, the arm silently never fires
    /// and the offline render would run the correction against the (undriven)
    /// live transport with NO compile error — rendering silence. Existing tests
    /// only cover the `VoicePool` arm, so this one closes the gap.
    #[test]
    fn offline_rebinds_bare_voice_node_transport() {
        use tutti_sampler::{LoopSetting, Playback, Voice, VoiceSource};

        // The live transport is rolling; the offline one is stopped — so the memory
        // source's `window_position()` returns `Some(..)` while bound to the live
        // clock and `None` once rebound to the stopped offline one. That is the
        // guard: a rebind that misses the source leaves this probe on the live
        // clock, and the offline render reads the wrong playhead.
        let live_transport = MockTransport::new(true);
        let wave = Arc::new(Wave::from_samples(
            44100.0,
            &(0..64).map(|i| (i as f32 + 1.0) / 64.0).collect::<Vec<_>>(),
        ));
        let sampler =
            MemorySource::with_transport(wave, live_transport.clone(), Beat::new(0.0), None);
        assert!(
            sampler.window_position().is_some(),
            "sanity: the memory source reads a live position before rebind"
        );
        // A standalone voice bound to the LIVE clock — the exact shape resynth
        // builds before the graph add.
        let voice = Voice {
            source: VoiceSource::Memory(sampler),
            play: Playback {
                loop_: LoopSetting::Off,
                direction: Direction::Forward,
                ..Playback::default()
            },
            channel_index: None,
        };

        // Put the bare voice node in a net feeding the output, clone it (as the
        // render does), and rebind to a STOPPED offline transport.
        let mut net = Net::new(0, 2);
        let id = net.push(Box::new(VoiceNode::from(voice)));
        net.pipe_output(id);

        let offline = MockTransport::new(false) as Arc<dyn Timeline>;
        let mut clone = net.clone();
        rebind_net_transport(&mut clone, &offline, 0.0, tutti_core::Bpm(120.0));

        let voice_node = clone
            .node_mut(id)
            .as_any_mut()
            .downcast_mut::<VoiceNode>()
            .expect("still a voice node after rebind");

        // And — the load-bearing half — the MEMORY SOURCE's OWN read clock must have
        // swapped too. The sampler reads its position from its own placement, so
        // if the rebind only touched `play.placement` this would still read the
        // (playing) live clock and the offline render would use the wrong
        // playhead with NO compile error.
        match &voice_node.voice().source {
            VoiceSource::Memory(sampler) => assert!(
                sampler.window_position().is_none(),
                "the memory source's own read clock must be rebound to the stopped \
                 offline transport (else the offline render reads the live \
                 playhead and renders the correction wrong)"
            ),
            other => panic!("expected a Memory source, got {other:?}"),
        }

        // And the same fact stated behaviourally: the rebound node renders
        // SILENCE against a stopped offline clock, while the un-rebound original
        // renders audio against the rolling live one.
        //
        // Deliberately redundant with the structural assertion above, which is
        // fragile in a specific direction: it asserts that a clock lives in a
        // particular place and that the rebind reached it. If the source ever
        // stops owning a clock (the standing plan for this type: position derives
        // from the playhead, the caller holds the cursor), `window_position()`
        // becomes permanently `None` and that assertion passes *vacuously* while
        // testing nothing. This one keeps failing for the right reason: it names
        // the property that actually matters — an offline render must not hear the
        // live playhead — without naming where the clock is kept.
        let mut rebound = clone.clone();
        let mut live = net.clone();
        let peak = |net: &mut Net| {
            net.reset();
            net.set_sample_rate(tutti_core::SampleRate(44_100.0));
            let mut worst = 0.0f32;
            let mut frame = [0.0f32; 2];
            for _ in 0..64 {
                net.tick(&[], &mut frame);
                worst = worst.max(frame[0].abs()).max(frame[1].abs());
            }
            worst
        };

        let live_peak = peak(&mut live);
        let rebound_peak = peak(&mut rebound);
        assert!(
            live_peak > 1e-6,
            "sanity: the un-rebound net must render audio from the rolling live \
             clock, else this comparison proves nothing (peak {live_peak})"
        );
        assert_eq!(
            rebound_peak, 0.0,
            "the rebound net must render exact silence against the stopped \
             offline clock; it rendered {rebound_peak} (live peak {live_peak})"
        );
    }

    /// With the cap reached, the highest-`priority` pending request is admitted
    /// first — so a consumer can paint its focused tap before its peers.
    #[test]
    fn higher_priority_request_admits_first() {
        let (graph, target) = graph_res_with_one_target();

        let mut world = World::new();
        world.insert_resource(graph);
        world.insert_resource(AudioConfig {
            sample_rate: 48_000.0,
            channels: tutti_core::ChannelLayout::Stereo,
        });
        world.insert_resource(RegionRenderConfig { max_in_flight: 1 });

        let start = |t: NodeId, priority: i32| StartRegionRender {
            target: t,
            start_beat: 0.0,
            len_beats: 4.0,
            tempo: 120.0,
            priority,
        };
        // Spawn the low-priority one first so archetype order would pick it under
        // the old (unsorted) admission — the sort must override that.
        let low = world.spawn(start(target, 0)).id();
        let high = world.spawn(start(target, 2)).id();

        let mut schedule = bevy_ecs::schedule::Schedule::default();
        schedule.add_systems(prepare_region_render_system);
        schedule.run(&mut world);

        // The high-priority request was admitted (its StartRegionRender removed);
        // the low-priority one is still pending.
        assert!(
            !world.entity(high).contains::<StartRegionRender>(),
            "highest-priority request must be admitted first"
        );
        assert!(
            world.entity(low).contains::<StartRegionRender>(),
            "lower-priority request must stay pending under the cap"
        );
    }
}
