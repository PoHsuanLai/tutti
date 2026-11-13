//! A plugin's own event sources, served by the main-thread view's `update` for
//! a plugin hosted in this process: the descriptors and timers it asked the
//! host to run (CLAP's `posix-fd-support` and `timer-support`) are looked at
//! without waiting, and what is ready is handed back, so an in-process host's
//! editors get them without the app knowing any format.
//!
//! The reference CLAP plugin, hosted in process (`Hosting::InProcess`), with an
//! embeddable editor open: an X11 window id is what its editor embeds into, so
//! the module is one `cfg` on its declaration.

use std::sync::OnceLock;
use std::time::Duration;

use tutti_clap_test_plugin::{
    CMD_REGISTER_FD, CMD_REGISTER_TIMER, ClapProbe, ClapSwitch, GuiMode, Site,
};
use tutti_fixtures::StandInWindow;
use tutti_plugin::catalog::{Hosting, PluginRef};
use tutti_plugin_types::ContentScale;
use tutti_session::{PluginLoadRequest, Session};
use tutti_types::{ChannelLayout, MaxBlock, SampleRate, Samples};

use crate::support::clap_probe;

fn probe() -> &'static ClapProbe {
    static PROBE: OnceLock<ClapProbe> = OnceLock::new();
    PROBE.get_or_init(|| {
        ClapProbe::open(
            &tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
            &clap_probe(),
        )
    })
}

fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}

/// A session with the reference plugin hosted in this process, an editor open
/// on it, and the plugin's latched command run by the editor's `show`.
fn session_with_open_editor(command: u32) -> Session {
    let probe = probe();
    probe.set_gui_mode(GuiMode::Embeddable);
    probe.thread_reset();
    probe.thread_command(command);
    let mut s = Session::builder()
        .build_offline(
            SampleRate(48_000.0),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(512)),
        )
        .expect("an offline session builds");
    let plugin = PluginRef::file(clap_probe())
        .expect("a plugin file")
        .with_hosting(Hosting::InProcess);
    let load = s.plugin_load(PluginLoadRequest::new(plugin));
    let node = s
        .add_plugin(&main_thread(), load.run())
        .expect("the reference plugin loads in this process");
    s.main(main_thread())
        .plugin(node)
        .expect("it is hosted")
        .editor()
        .open(&StandInWindow, ContentScale::IDENTITY)
        .expect("the editor opens");
    s
}

/// **A descriptor the plugin registered is served by the view's `update` the
/// moment it is readable**, and by no other call: the view looks at the
/// plugin's descriptors without waiting and hands back what is ready.
#[test]
fn an_in_process_editors_descriptor_is_served_by_the_views_update() {
    let mut s = session_with_open_editor(CMD_REGISTER_FD);
    let probe = probe();
    assert!(
        probe.thread_capture().fd_registered,
        "the plugin registered a descriptor when its editor was shown"
    );

    let mut ui = s.main(main_thread());
    let _ = ui.update();
    assert_eq!(
        probe.thread_capture().fd_fires,
        0,
        "nothing is readable, so nothing is called"
    );
    for fires in 1..=3 {
        assert!(probe.fd_poke(), "the plugin's pipe takes a byte");
        let _ = ui.update();
        assert_eq!(
            probe.thread_capture().fd_fires,
            fires,
            "one update after the byte, one call of `on_fd`"
        );
    }
    let _ = ui.update();
    assert_eq!(
        probe.thread_capture().fd_fires,
        3,
        "drained, so not called again"
    );
}

/// **A timer the plugin registered is served by the view's `update` once its
/// period has passed**, and not before.
#[test]
fn an_in_process_editors_timer_is_served_by_the_views_update_at_its_period() {
    probe().set(ClapSwitch::TimerPeriod, 200);
    let mut s = session_with_open_editor(CMD_REGISTER_TIMER);
    let probe = probe();
    assert!(probe.thread_capture().timer_registered);
    let ticks = || probe.thread_capture().sites[Site::OnTimer as usize].visits;

    let mut ui = s.main(main_thread());
    let _ = ui.update();
    assert_eq!(ticks(), 0, "not before its period");
    std::thread::sleep(Duration::from_millis(250));
    let _ = ui.update();
    assert_eq!(ticks(), 1, "once its period has passed");
    let _ = ui.update();
    assert_eq!(ticks(), 1, "and its period restarts");
    probe.set(ClapSwitch::TimerPeriod, 0);
}
