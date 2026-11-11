//! **A 440 Hz tone on the default device, with no ECS.** The session opens
//! the device first and builds the graph at the rate it reports; `update()`
//! commits the edits. Run (needs a sound card):
//! `cargo run -p tutti --features device --example headless_engine`
use tutti::graph::ForkByClone;
use tutti::nodes::testing::Osc;
use tutti::prelude::*;

/// What `Session::update` found: faults and dropouts (a refused commit, a
/// plugin that died, a lost block). A host logs them; here they are printed.
fn report(events: Vec<SessionEvent>) {
    for event in events {
        eprintln!("session event: {event:?}");
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut live = SessionBuilder::default().build_device(Default::default())?;
    let tone = Osc::sine(Hz(440.0)).with_amplitude(Amplitude(0.2));
    let (tone, _) = live.graph().add(ForkByClone(tone));
    live.graph().set_outputs_from(tone);
    report(live.update());
    live.play()?;
    std::thread::sleep(std::time::Duration::from_secs(2));
    println!("running: {}", live.device().is_running()); // dropping `live` stops it
    Ok(())
}
