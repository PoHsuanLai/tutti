//! # 03 - Transport Control
//!
//! Interactive transport control: play, stop, tempo, looping, metronome.
//!
//! A sequence plays different synth tones per beat. The loop range is beats 4-8,
//! so with loop ON you hear only the high section repeating. Toggle loop OFF
//! to hear the full 16-beat sequence.
//!
//! **Concepts:** Transport state, tempo, loop range, metronome, seeking
//!
//! ```bash
//! cargo run --example 03_transport_control
//! ```

use std::io::{self, Write};
use std::sync::Arc;
use tutti::core::{AtomicBool, MetronomeMode, Ordering, Shared};
use tutti::prelude::*;

fn main() -> tutti::Result<()> {
    let mut engine = TuttiEngine::builder().build()?;

    let freq = Shared::new(220.0);
    let freq_var = freq.clone();

    // Square wave is more obviously pitched than sine
    engine.graph.master(var(&freq_var) >> (square() * 0.1));
    engine.graph.commit();

    // Loop range is beats 4-8 (the "high" section)
    engine
        .transport
        .tempo(140.0)
        .loop_range(4.0, 8.0)
        .enable_loop();

    engine.transport.play();

    println!("Transport Control:");
    println!("  p=play  s=stop  l=toggle loop  m=toggle metronome");
    println!("  +=tempo up  -=tempo down  0=seek to start  q=quit");
    println!();
    println!("16-beat sequence:  LOW(0-3)  HIGH(4-7)  MID(8-11)  DESCEND(12-15)");
    println!("Loop range = beats 4-8, so loop ON repeats only the HIGH section.");
    println!();

    // 16 beats with very distinct pitch regions
    let notes: [f32; 16] = [
        // Beats 0-3: LOW rumble
        65.41, 73.42, 82.41, 98.00, // Beats 4-7: HIGH melody (this is the loop range)
        523.25, 587.33, 659.25, 783.99, // Beats 8-11: MID range
        220.00, 246.94, 261.63, 293.66, // Beats 12-15: DESCENDING
        440.00, 349.23, 261.63, 196.00,
    ];

    let running = Arc::new(AtomicBool::new(true));
    let running_bg = running.clone();
    let freq_bg = freq.clone();
    let transport_bg = engine.transport.clone();
    std::thread::spawn(move || {
        while running_bg.load(Ordering::Relaxed) {
            let beat = transport_bg.current_beat();
            let beat_index = (beat.floor() as usize) % notes.len();
            freq_bg.set(notes[beat_index]);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });

    loop {
        let t = &engine.transport;
        let beat = t.current_beat();
        let section = match (beat.floor() as usize) % 16 {
            0..=3 => "LOW",
            4..=7 => "HIGH",
            8..=11 => "MID",
            _ => "DESC",
        };
        print!(
            "\r[beat:{:5.1} {} | {} BPM | loop:{} | metro:{}] > ",
            beat,
            section,
            t.get_tempo(),
            if t.is_loop_enabled() { "ON " } else { "off" },
            if t.metronome().get_mode() != MetronomeMode::Off {
                "ON"
            } else {
                "off"
            }
        );
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        match input.trim() {
            "p" => {
                engine.transport.play();
            }
            "s" => {
                engine.transport.stop();
            }
            "l" => {
                engine.transport.toggle_loop();
            }
            "m" => {
                let m = engine.transport.metronome();
                if m.get_mode() == MetronomeMode::Off {
                    m.always();
                } else {
                    m.off();
                }
            }
            "+" => {
                let current = engine.transport.get_tempo().get();
                engine.transport.tempo(current + 10.0);
            }
            "-" => {
                let current = engine.transport.get_tempo().get();
                engine.transport.tempo((current - 10.0).max(30.0));
            }
            "0" => {
                engine.transport.seek(0.0);
            }
            "q" => break,
            _ => {}
        }
    }

    running.store(false, Ordering::Relaxed);
    Ok(())
}
