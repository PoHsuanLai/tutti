//! **A 440 Hz tone on the default device, with no ECS.** The session opens
//! the device first and builds the graph at the rate it reports; `update()`
//! commits the edits. Run (needs a sound card):
//! `cargo run -p tutti --features device --example headless_engine`
use tutti::graph::ForkByClone;
use tutti::nodes::testing::Osc;
use tutti::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (mut session, device) = SessionBuilder::default().build_device(Default::default())?;
    let tone = Osc::sine(Hz(440.0)).with_amplitude(Amplitude(0.2));
    let (tone, _) = session.graph().add(ForkByClone(tone));
    session.graph().set_outputs_from(tone);
    session.update();
    session.transport().motion.try_send(MotionEvent::Play)?;
    std::thread::sleep(std::time::Duration::from_secs(2));
    println!("running: {}", device.is_running()); // dropping `device` stops it
    Ok(())
}
