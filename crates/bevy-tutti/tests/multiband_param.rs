//! An engine node with indexed params needs no registration in the host:
//! `MultibandCompressorNode` is a `SessionNode` already, so `AudioParam` with a
//! packed `P` writes one band's cell through `spawn_audio_node`.

use bevy_app::prelude::*;
use bevy_tutti::AudioEngineState;
use bevy_tutti::graph::{
    AudioGraphRes, AudioParam, AudioParamAppExt, GraphReconcilePlugin, SpawnAudioNode, TransportRes,
};
use tutti_graph::ParamNode;
use tutti_nodes::MultibandCompressorNode;
use tutti_transport::Transport;
use tutti_types::ChannelLayout;
use tutti_types::{Db, UnitAddr, UnitParam};

const MAKEUP1: UnitAddr = UnitAddr::new(UnitParam::GainDb, 1);
type Makeup1 = AudioParam<Db, { UnitAddr::new(UnitParam::GainDb, 1).to_u16() }>;

/// **A band-1 `AudioParam` writes band 1's make-up cell of the engine node**
/// and leaves band 0's cell alone.
///
/// Mutation (run): `MultibandCompressorNode::param_set` addressing band 1's
/// make-up with band 0's cell → the band-0 cell takes the value and band 1's
/// stays → fails.
#[test]
fn an_audio_param_writes_one_bands_makeup() {
    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.insert_resource(AudioGraphRes::headless(0, 1));
    app.insert_resource(TransportRes(Transport::new(48_000.0)));
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(GraphReconcilePlugin);
    app.add_audio_param::<Db, { UnitAddr::new(UnitParam::GainDb, 1).to_u16() }>();

    let node = MultibandCompressorNode::new(ChannelLayout::STEREO, 3).unwrap();
    let set = node.param_set();
    app.world_mut()
        .commands()
        .spawn_audio_node(node)
        .insert(Makeup1::new(Db(4.5)));
    app.world_mut().flush();
    app.update();

    let band0 = UnitAddr::new(UnitParam::GainDb, 0);
    assert_eq!(set.get(MAKEUP1), Some(4.5));
    assert_eq!(set.get(band0), Some(0.0), "band 0 was not touched");
}
