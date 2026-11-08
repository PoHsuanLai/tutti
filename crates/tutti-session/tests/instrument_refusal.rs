//! An effect plugin is not an instrument: `Session::instrument_plugin` refuses it,
//! naming the role the plugin declared. (The reference probe declares itself an
//! effect; the shared plugin's descriptor lists both.)
//!
//! Mutation (run): `Session::instrument` skipping the role check makes this
//! fail with "an effect plugin was accepted".

mod instrument_support;
use instrument_support::subjects::Plugin;
use tutti_plugin::catalog::PluginRole;
use tutti_session::{Error, NotInstrument};

#[test]
fn an_effect_plugin_is_refused() {
    let (mut session, loaded) = Plugin::effect();
    match session.instrument_plugin(loaded) {
        Err(Error::NotInstrument {
            why: NotInstrument::Plugin(PluginRole::Effect),
            ..
        }) => {}
        Err(other) => panic!("refused for another reason: {other}"),
        Ok(_) => panic!("an effect plugin was accepted"),
    }
}
