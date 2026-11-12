//! Which plugin a load opens, and whose state a restore accepts: a plugin is
//! its file *and* its id in it (`PluginRef`), checked at the request and at
//! the hosted plugin. The reference CLAP bundle exports two plugins, in a real
//! `plugin-server`.

use crate::support::{clap_probe, plugin_server};
use tutti_plugin::catalog::{PluginRef, PluginState};
use tutti_plugin::error::StateError;
use tutti_plugin_db::{PluginEntry, PluginFormat};
use tutti_plugin_types::{PluginClass, PluginDescriptor};
use tutti_session::{PluginLoadRequest, Session};
use tutti_types::{ChannelLayout, MaxBlock, SampleRate, Samples};

fn first_id() -> &'static str {
    tutti_clap_test_plugin::PLUGIN_ID.to_str().expect("utf-8")
}

fn second_id() -> &'static str {
    tutti_clap_test_plugin::SECOND_PLUGIN_ID
        .to_str()
        .expect("utf-8")
}

fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}

fn session() -> Session {
    // SAFETY: nextest runs each test in its own process, and this is set before
    // the first server is spawned, from the test's own thread.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", plugin_server()) };
    Session::builder()
        .build_offline(
            SampleRate(48_000.0),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(512)),
        )
        .expect("an offline session builds")
}

/// What a scan reports for the bundle's plugin `id`.
fn entry_ref(path: &std::path::Path, id: &str) -> PluginRef {
    let descriptor = PluginDescriptor::new(id, id, PluginClass::Unknown);
    PluginRef::from(&PluginEntry {
        path,
        format: PluginFormat::Clap,
        descriptor: &descriptor,
    })
}

/// **A scanned plugin loads as itself, not as its file's first.** The bundle's
/// second plugin, picked by the id a scan reports, is the one that is hosted;
/// the file alone still opens the first, and its record carries the id the
/// loaded plugin reports.
#[test]
fn the_second_plugin_of_a_bundle_loads_by_its_entry() {
    let path = clap_probe();
    let mut s = session();
    let main = main_thread();

    let second = s
        .add_plugin_blocking(&main, PluginLoadRequest::new(entry_ref(&path, second_id())))
        .expect("the second plugin loads");
    let hosted = s.plugin(second).unwrap();
    assert_eq!(hosted.handle().descriptor().id, second_id());
    assert_eq!(hosted.plugin().id(), Some(second_id()));

    let first = s
        .add_plugin_blocking(
            &main,
            PluginLoadRequest::new(PluginRef::file(&path).expect("a plugin file")),
        )
        .expect("the file's first plugin loads");
    let hosted = s.plugin(first).unwrap();
    assert_eq!(hosted.handle().descriptor().id, first_id());
    assert_eq!(
        hosted.plugin().id(),
        Some(first_id()),
        "the record names the plugin the file's first turned out to be"
    );
}

/// **A state saved from one plugin is refused by a request for the other, and
/// by the other's hosted plugin; its own takes it.** The refusal comes before
/// the plugin is asked: `OtherPlugin` names both.
#[test]
fn a_state_is_refused_by_another_plugin() {
    let path = clap_probe();
    let first_ref = entry_ref(&path, first_id());
    let second_ref = entry_ref(&path, second_id());
    let mut s = session();
    let main = main_thread();
    let first = s
        .add_plugin_blocking(&main, PluginLoadRequest::new(first_ref.clone()))
        .expect("loads");
    let second = s
        .add_plugin_blocking(&main, PluginLoadRequest::new(second_ref.clone()))
        .expect("loads");

    let from_first = s.plugin(first).unwrap().state().save().expect("saves");
    assert_eq!(from_first.plugin(), &first_ref, "the state names its owner");
    let from_second = s.plugin(second).unwrap().state().save().expect("saves");

    // The request.
    let refused = PluginLoadRequest::new(second_ref.clone())
        .restoring(from_first.clone())
        .expect_err("the first plugin's state is not the second's");
    assert_eq!(
        refused,
        StateError::OtherPlugin {
            expected: second_id().into(),
            found: first_id().into(),
        }
    );
    let request = PluginLoadRequest::new(first_ref)
        .restoring(from_first.clone())
        .expect("its own state is accepted");
    assert_eq!(request.state(), Some(&from_first));

    // The hosted plugin.
    let hosted = s.plugin(second).unwrap();
    let refused = hosted
        .state()
        .restore(&main, &from_first)
        .expect_err("a hosted plugin refuses another's state");
    assert!(
        matches!(refused, StateError::OtherPlugin { .. }),
        "{refused:?}"
    );
    hosted
        .state()
        .restore(&main, &from_second)
        .expect("and takes its own");
    let _: &PluginState = &from_second;
}
