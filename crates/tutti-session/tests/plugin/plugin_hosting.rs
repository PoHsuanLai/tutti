//! What a load asks of the catalog and of the ref's `Hosting`, through the
//! session: the reference CLAP plugin (`clap_probe`), loaded by `plugin_load`.

use crate::support::{clap_probe, plugin_server};
use tutti_plugin::catalog::{Catalog, CatalogConfig, Hosting, PluginRef};
use tutti_plugin_db::PluginDb;
use tutti_plugin_types::BridgeError;
use tutti_session::{PluginLoadRequest, Session};
use tutti_types::{MaxBlock, Samples};

fn session() -> Session {
    Session::builder()
        .max_block(MaxBlock::of(Samples(512)))
        .build()
        .expect("an offline session builds")
}

fn catalog() -> Catalog {
    Catalog::new(
        PluginDb::new(),
        CatalogConfig::new("/nonexistent", tutti_plugin::catalog::NO_SCAN_DIRS),
        |_| Ok(()),
    )
}

fn set_server(path: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: nextest runs each test in its own process, and this is set from
    // the test's own thread before anything spawns a server.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", path) };
}

/// **A blacklisted plugin is refused at load, not at open**: the catalog opens nothing, so the
/// refusal is the load's. A load that
/// names a catalog that blacklists the plugin fails with the recorded reason,
/// before any server is looked for (none can be found here, which would be
/// `ServerNotFound`); the same ref loaded with no catalog is admitted, and so
/// is the load once the user has unblacklisted it.
#[test]
fn a_blacklisted_plugin_is_refused_at_load() {
    let path = clap_probe();
    let plugin = PluginRef::file(&path).expect("a plugin file");
    let mut catalog = catalog();
    catalog.blacklist(&path, "SIGSEGV during probe");
    let mut s = session();
    let main = tutti_fixtures::main_thread();

    set_server("/nonexistent/plugin-server");
    let load = s
        .plugin_load(PluginLoadRequest::new(plugin.clone()))
        .with_catalog(&catalog);
    match s.add_plugin(main, load.run()) {
        Err(BridgeError::Blacklisted {
            path: refused,
            reason,
        }) => {
            assert_eq!(refused, path);
            assert_eq!(reason, "SIGSEGV during probe");
        }
        other => panic!("expected a Blacklisted refusal at load, got {other:?}"),
    }

    // No catalog named: no blacklist consulted, and the plugin loads.
    set_server(plugin_server());
    s.add_plugin_blocking(main, PluginLoadRequest::new(plugin.clone()))
        .expect("a load that names no catalog consults no blacklist");

    // The user unblacklists it: the same catalog now admits it.
    assert!(catalog.unblacklist(&path));
    let load = s
        .plugin_load(PluginLoadRequest::new(plugin))
        .with_catalog(&catalog);
    s.add_plugin(main, load.run())
        .expect("an unblacklisted plugin loads");
}

/// **The blacklist is asked when the load runs, not when it is made**: a plugin
/// the catalog blacklists between `with_catalog` and `run` is refused, and one
/// it unblacklists in that window is admitted.
#[test]
fn a_plugin_blacklisted_after_the_load_is_made_is_refused_when_it_runs() {
    let path = clap_probe();
    let plugin = PluginRef::file(&path).expect("a plugin file");
    let mut catalog = catalog();
    let mut s = session();
    let main = tutti_fixtures::main_thread();

    set_server("/nonexistent/plugin-server");
    let load = s
        .plugin_load(PluginLoadRequest::new(plugin.clone()))
        .with_catalog(&catalog);
    catalog.blacklist(&path, "hung in its load");
    match s.add_plugin(main, load.run()) {
        Err(BridgeError::Blacklisted { reason, .. }) => assert_eq!(reason, "hung in its load"),
        other => panic!("expected a Blacklisted refusal at run, got {other:?}"),
    }

    let load = s
        .plugin_load(PluginLoadRequest::new(plugin))
        .with_catalog(&catalog);
    assert!(catalog.unblacklist(&path));
    let loaded = s.add_plugin(main, load.run());
    assert!(
        !matches!(loaded, Err(BridgeError::Blacklisted { .. })),
        "an unblacklisted plugin was refused"
    );
}

/// **A hosting the format cannot have is refused at load**, whether the load
/// runs on a worker or the main thread: VST2 asked to be served is
/// `NotOutOfProcess`, and the request is not changed to in process.
#[test]
fn vst2_asked_for_out_of_process_is_refused_at_load() {
    let plugin = PluginRef::file("Synth.vst")
        .expect("a plugin file")
        .with_hosting(Hosting::OutOfProcess);
    let mut s = session();
    let load = s.plugin_load(PluginLoadRequest::new(plugin));
    assert!(
        !load.opens_in_process(),
        "nothing is left for the main thread"
    );
    match s.add_plugin(tutti_fixtures::main_thread(), load.run()) {
        Err(BridgeError::NotOutOfProcess { format, .. }) => assert_eq!(format, "VST2"),
        other => panic!("expected NotOutOfProcess, got {other:?}"),
    }
}

/// **`Hosting::InProcess` makes the session host a CLAP plugin in its own
/// process**: the load is left for the main thread (`opens_in_process`), no
/// server is started (none can be found here), and the hosted plugin is the
/// one asked for, wired like any other.
#[test]
fn a_clap_plugin_asked_in_process_is_hosted_here() {
    set_server("/nonexistent/plugin-server");
    let plugin = PluginRef::file(clap_probe())
        .expect("a plugin file")
        .with_hosting(Hosting::InProcess);
    let mut s = session();
    let load = s.plugin_load(PluginLoadRequest::new(plugin));
    assert!(load.opens_in_process());

    let node = s
        .add_plugin(tutti_fixtures::main_thread(), load.run())
        .expect("the reference plugin loads in this process");
    let hosted = s.plugin(node).expect("it is hosted");
    assert_eq!(hosted.diagnostics().server_pid(), None);
    assert_eq!(
        hosted.descriptor().id,
        tutti_clap_test_plugin::PLUGIN_ID.to_str().expect("utf-8")
    );
}
