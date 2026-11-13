//! A plugin host, end to end (design 029): scan, pick, load, wire, play,
//! set a parameter, save the state, read its health, diagnostics and presets, bounce.
//!
//! ```text
//! cargo build -p tutti-plugin-server -p tutti-plugin-probe
//! cargo run -p tutti --features clap,export --example plugin_host -- \
//!     "Plugin Name" out.wav /path/to/plugin/dir [more dirs...]
//! ```
//!
//! The scan puts every plugin it finds in a [`Catalog`]; the catalog says what
//! exists and what is blacklisted, and the session does the loading; the one whose name is
//! `argv[1]` becomes a [`PluginRef`] (its file *and* its id in it, so a bundle
//! that exports several loads the one that was picked); the session loads it,
//! hosts it, and bounces four beats of it to `out.wav`. An instrument plays a
//! note; the plugin's first parameter is set and dragged (`params()`); its state is saved with
//! `state().save()` and written beside the WAV.
//!
//! The plugin is a [`Plugin`](tutti::Plugin) view (`session.plugin(node)`), and what happens to
//! it is a [`PluginEvent`] in `update()`'s events. The host's thread claims the
//! main thread and calls `session.main(main).update()`, which services the
//! editors it opened (`ui.plugin(node)?.editor().open(&window, scale)`, with a
//! window of a winit app; this host has none) and raises
//! `PluginChange::EditorWantsSize` when one asks for a size.
//!
//! It is not run in CI (it needs a plugin directory and the two binaries
//! above built); `cargo build -p tutti --features clap,export --examples`
//! compiles it.

use std::path::PathBuf;

use tutti::plugin::catalog::{CatalogConfig, PluginDb};
use tutti::ump::{Address, Message, NoteNumber, U7, Ump, Voice1};
use tutti::{
    Beat, Bounce, Catalog, ChannelLayout, Format, Hosting, MaxBlock, MidiChannel, Normalized,
    ParamId, Pcm, PluginEvent, PluginLoadRequest, PluginRef, PresetError, PresetKind, Range, Root,
    SampleRate, Samples, Session, SessionEvent, TimedMidiEvent,
};

const RATE: SampleRate = SampleRate::SR_48K;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let (Some(name), Some(out)) = (args.next(), args.next()) else {
        return Err("usage: plugin_host NAME OUT.wav DIR [DIR...]".into());
    };
    let name = name.to_string_lossy().into_owned();
    let out = PathBuf::from(out);
    let dirs: Vec<PathBuf> = args.map(PathBuf::from).collect();

    // 1. Scan. The catalog's table is in memory (its persistence stores nothing,
    //    so nothing is kept between runs); the scan probes every file it finds in a
    //    `plugin-probe` subprocess, so a plugin that crashes the probe is
    //    blacklisted, not loaded into this process.
    let mut catalog = Catalog::new(
        PluginDb::new(),
        CatalogConfig::new(std::env::temp_dir(), dirs),
        |_| Ok(()),
    );
    let scanned = catalog.scan()?;
    eprintln!("scanned {} plugin file(s)", scanned.scanned);

    // 2. Pick by name. An entry knows its file, its format and its descriptor;
    //    the ref made from it is what the load and a saved state both name.
    let entry = catalog
        .entries()
        .find(|e| e.descriptor.name == name)
        .ok_or_else(|| format!("no plugin named {name:?} in the scan"))?;
    let instrument = entry.is_instrument();
    // `Hosting::Auto` is what a ref starts with: CLAP, VST3 and AU are served
    //    from a `plugin-server` process, VST2 runs here. `Hosting::InProcess`
    //    asks for this process where the format allows it.
    let plugin = PluginRef::from(&entry).with_hosting(Hosting::Auto);
    eprintln!("{name}: {:?} in {}", plugin.id(), plugin.path().display());

    // 3. Load it into an offline session. The load is a job (a server launch
    //    blocks); here it runs on this thread. The load names the catalog, so a
    //    plugin it blacklisted is refused here (`BridgeError::Blacklisted`),
    //    before any server starts. The thread that loads an in-process plugin
    //    is the main thread: claim the token once.
    let mut session = Session::builder().build_offline(
        RATE,
        ChannelLayout::STEREO,
        MaxBlock::of(Samples(512)),
    )?;
    let main = tutti::plugin::MainThread::claim()?;
    let loaded = session
        .plugin_load(PluginLoadRequest::new(plugin))
        .with_catalog(&catalog)
        .run();
    let node = session.add_plugin(&main, loaded)?;

    // 4. Wire it: the plugin's outputs feed the root's channels. `node` is a
    //    `Key<PluginNode>`: ports come from it as from any node's key
    //    (`node.out(0)`, `node.events_in()`), and with `modulation`
    //    `node.param(id)` is the reference a route targets.
    session.graph().connect(node, Root)?;

    // 5. Play. An instrument takes a note on its event input: middle C for the
    //    first two beats, as a clip on the project's timeline.
    if instrument {
        let note = |beat: f64, on: bool| {
            let (note, velocity) = (NoteNumber::of(60), U7::of(100));
            let voice = if on {
                Voice1::NoteOn { note, velocity }
            } else {
                Voice1::NoteOff { note, velocity }
            };
            let message = Message::Midi1(Address::first(MidiChannel::FIRST), voice);
            TimedMidiEvent::new(Beat(beat), Ump::from(message)).expect("a finite beat")
        };
        session
            .midi()
            .play_clip(node, Beat(0.0), [note(0.0, true), note(2.0, false)])?;
    }

    // 6. Set a parameter: the first one it lists, to the middle of its range.
    //    `params()` is the session's own view of what the plugin declares (read
    //    at load, read again when the plugin announces a rescan), so listing
    //    asks the plugin nothing; `get` resolves an id once and refuses one the
    //    plugin does not declare. A value set reads back at once, and `text` is
    //    the plugin's own wording of it. A drag is a gesture: `edit()` begins
    //    it, each `set` is a change, and dropping the guard ends it, so the
    //    session reports the host's begin, changes and end in `update()`'s
    //    events as it reports the plugin's own.
    let hosted = session.plugin(node)?;
    let params = hosted.params();
    let first: Option<ParamId> = params.iter().next().map(|p| p.id());
    if let Some(id) = first {
        let param = params.get(id)?;
        param.set(Normalized::clamped(0.5))?;
        eprintln!(
            "{} = {}",
            param.info().name(),
            param
                .text()
                .unwrap_or_else(|| param.normalized().get().to_string())
        );
        let drag = param.edit()?;
        for step in 0..=4 {
            drag.set(Normalized::clamped(f64::from(step) / 8.0))?;
        }
        drop(drag);
    }

    // 7. Save its state: now, from the plugin (`last_good` is the session's
    //    crash-recovery copy, up to a snapshot interval old). The state names
    //    the plugin it came from, so a project that keeps it restores it into
    //    that plugin and no other.
    let state = hosted.state().save()?;
    let state_path = out.with_extension("state");
    std::fs::write(&state_path, state.bytes())?;
    eprintln!(
        "saved {} bytes of {} to {}",
        state.bytes().len(),
        state.plugin().id().unwrap_or("the plugin"),
        state_path.display()
    );

    // 8. Health and presets. `health()` is one reading of the plugin's life
    //    (running, stalled, dead and about to be written off, or being
    //    reloaded). Presets are the main thread's: `ui.plugin(node)?.presets()`
    //    says whether the plugin lists them (`Browse`: load an entry by its id)
    //    or loads them from files (`File`, CLAP), and refuses a plugin with
    //    none (`PresetError::Unsupported`).
    eprintln!("health: {:?}", hosted.health());
    //    Diagnostics are the corner of the plugin's panel: its server (none in
    //    process), its dropouts, what it lost, and the latency the graph
    //    compensates for it, the same whichever hosting runs it. `buses()` hands
    //    out the graph's bus handles: a key input is wired by
    //    index (no format marks a sidechain): `connect(kick.bus_out(0),
    //    hosted.buses().input(1).unwrap())`.
    let diagnostics = hosted.diagnostics();
    eprintln!(
        "server {:?}, latency {:?}, lost MIDI {:?}, input buses: {}",
        diagnostics.server_pid(),
        diagnostics.latency(),
        diagnostics.midi_in(),
        hosted.buses().input_count()
    );
    {
        let mut ui = session.main(main);
        match ui.plugin(node)?.presets() {
            Ok(presets) => match presets.kind() {
                PresetKind::Browse => {
                    eprintln!("{} preset(s)", presets.len());
                    if let Some(first) = presets.iter().next() {
                        first.load()?;
                        eprintln!("loaded {:?}", first.name());
                    }
                }
                _ => eprintln!("loads preset files, lists none"),
            },
            Err(PresetError::Unsupported) => eprintln!("no presets"),
            Err(other) => return Err(other.into()),
        }
    }

    // 9. Events. What the plugin reports (its own edit of a parameter, a new
    //    tail) and what happens to it (died, reloading) is one stream: the
    //    session's `SessionEvent::Plugin`, named by node. A host reads it from
    //    `update()`; there is no callback to install and none to clear.
    //    `MainSession::update` is the main thread's `update`: it also drops the
    //    plugins the session retired and services every open editor.
    for event in session.main(main).update() {
        if let SessionEvent::Plugin(PluginEvent { node, change, .. }) = event {
            eprintln!("{node:?}: {change:?}");
        }
    }

    // 10. Bounce four beats to a 24-bit WAV. The plugin hears Offline while it
    //    renders and Realtime after.
    let job = session.bounce_start(
        Bounce::project()
            .range(Range::Beats(Beat(0.0), Beat(4.0)))
            .master(out.as_path(), Format::Wav(Pcm::Int24)),
    )?;
    let written = session.finish_bounce(job.run())?.into_master();
    println!("wrote {} ({} bytes)", written.path.display(), written.bytes);
    Ok(())
}
