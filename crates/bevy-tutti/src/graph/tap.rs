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

use tutti_core::metering::AudioTap;

/// The master output's lock-free analysis tap.
///
/// Opt-in. Call `open()` through the [`Deref`](std::ops::Deref) to get the
/// consumer end and start paying for the copy; `close()` to stop. The consumer
/// is owned and drained by whoever opened it — it is a ring-buffer half, not
/// something an ECS resource can hold for you.
///
/// One consumer at a time: `open()` on a live tap returns
/// [`TapBusy`](tutti_core::metering::TapBusy) instead of minting a second ring,
/// so a system that opens for recording cannot silently kill a system that
/// opened for analysis. `close()` first to hand it over deliberately.
///
/// ```rust,ignore
/// fn start_analysis(tap: Res<AudioTapRes>) {
///     let consumer = tap.open().expect("tap is free");
///     std::thread::spawn(move || { /* drain `consumer` */ });
/// }
/// ```
///
/// To *record* the master rather than analyse it, wrap the consumer in
/// `TapIn` (`audio-io`) — the same ring, adapted to the `AudioIn` a pump takes:
///
/// ```rust,ignore
/// fn record_master(tap: Res<AudioTapRes>, mut commands: Commands) {
///     let wav = WavOut::create(&path, sample_rate, ChannelLayout::Stereo, BitDepth::Float32)?;
///     let src = TapIn::new(tap.open().expect("tap is free"));
///     commands.spawn(AudioPump::start(src, wav, 1024));
/// }
/// ```
#[derive(Resource, Clone, Default)]
pub struct AudioTapRes(pub AudioTap);

impl std::ops::Deref for AudioTapRes {
    type Target = AudioTap;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
