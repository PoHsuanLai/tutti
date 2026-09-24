//! The analysis tap's ECS wrapper.
//!
//! The tap is born in [`build_into`](crate::engine::build_into), which shares it
//! with the RT callback and inserts it here. Unlike the meter it is **not**
//! opened at build: while closed, the audio thread's push is one atomic load and
//! a return, so a host that never analyses pays nothing.
//!
//! Holding it as a resource is what makes it reachable at all: the callback's
//! clone is a producer, so without a handle on this side nothing could ask for
//! the consumer end.

use bevy_ecs::prelude::*;

use tutti_core::AudioTap;

/// The master output's lock-free analysis tap.
///
/// Opt-in. Call `open()` through the [`Deref`](std::ops::Deref) to get the
/// consumer end and start paying for the copy; `close()` to stop. The consumer
/// is owned and drained by whoever opened it — it is a ring-buffer half, not
/// something an ECS resource can hold for you.
///
/// One consumer at a time: `open()` on a live tap returns
/// [`TapBusy`](tutti_core::TapBusy) instead of minting a second ring,
/// so a system that opens for recording cannot silently kill a system that
/// opened for analysis. `close()` first to hand it over deliberately.
///
/// ```rust
/// use bevy_app::prelude::*;
/// use bevy_ecs::prelude::*;
/// use bevy_tutti::prelude::*;
///
/// fn start_analysis(tap: Res<AudioTapRes>) {
///     let consumer = tap.open().expect("tap is free");
///     std::thread::spawn(move || { /* drain `consumer` */ });
/// }
///
/// let mut app = App::new();
/// // `build_into` inserts this — closed, unlike the meter.
/// app.insert_resource(AudioTapRes::default());
/// assert!(!app.world().resource::<AudioTapRes>().is_open());
///
/// app.add_systems(Startup, start_analysis);
/// app.update();
///
/// let tap = app.world().resource::<AudioTapRes>();
/// assert!(tap.is_open());
/// // One consumer at a time: a second opener is refused rather than silently
/// // minting a ring the first one's owner will never see.
/// assert!(tap.open().is_err());
/// ```
///
/// To *record* the master rather than analyse it, wrap the consumer in
/// `TapIn` (`audio-io`) — the same ring, adapted to the `AudioIn` a pump takes:
///
/// ```rust
/// # #[cfg(feature = "audio-io")] {
/// use bevy_app::prelude::*;
/// use bevy_ecs::prelude::*;
/// use bevy_tutti::prelude::*;
///
/// /// Where the take lands. A real host reads this off its project settings.
/// #[derive(Resource)]
/// struct TakePath(std::path::PathBuf);
///
/// fn record_master(tap: Res<AudioTapRes>, path: Res<TakePath>, mut commands: Commands) {
///     // `create` returns an `Option` — `None` is "the path could not be
///     // opened", which is the caller's to report.
///     let wav = WavOut::create(&path.0, 48_000.0, ChannelLayout::STEREO, BitDepth::Float32)
///         .expect("a writable path");
///     let src = TapIn::new(tap.open().expect("tap is free"));
///     commands.spawn(AudioPump::start(src, wav, Samples(1024)));
/// }
///
/// let dir = tempfile::tempdir().expect("a temp dir");
/// let mut app = App::new();
/// app.add_audio_pump::<f32>();
/// app.insert_resource(AudioTapRes::default());
/// app.insert_resource(TakePath(dir.path().join("take.wav")));
/// app.add_systems(Startup, record_master);
/// app.update();
///
/// // The tap is open and one pump is running against it.
/// assert!(app.world().resource::<AudioTapRes>().is_open());
/// assert_eq!(app.world_mut().query::<&AudioPump<f32>>().iter(app.world()).count(), 1);
/// # }
/// ```
#[derive(Resource, Clone, Default)]
pub struct AudioTapRes(pub AudioTap);

impl std::ops::Deref for AudioTapRes {
    type Target = AudioTap;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
