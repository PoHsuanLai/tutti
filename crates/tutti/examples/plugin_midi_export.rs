//! Load a plugin, play a MIDI file, export to WAV, without Bevy (design 015
//! §5.2).
//!
//! The plugin is loaded as a job and added to an offline session; the file's
//! events become a clip on the plugin's event input; the export renders the
//! file's length (integrated over its tempo map), trims the latency the plan
//! compensated, and renders the plugin's declared tail. The plugin hears
//! Offline from `export` and Realtime from `finish_export`.
//!
//! With no arguments it plays a two-bar phrase it writes itself through the
//! reference CLAP plugin (`tutti-clap-test-plugin`), which needs the
//! `plugin-server` built first:
//!
//! ```text
//! cargo build -p tutti-plugin-server
//! cargo run -p tutti --features clap,export --example plugin_midi_export
//! cargo run -p tutti --features clap,export --example plugin_midi_export -- \
//!     Synth.clap song.mid song.wav
//! ```
//!
//! CI runs it as a test (`test = true`), which plays the phrase.
//!
//! It is the umbrella's, not `tutti-session`'s, so that it reads as the
//! design does: one `use tutti::prelude::*;` (the session's requests are in
//! the prelude with the features that gate them). `tutti-session` cannot
//! name `tutti`, which depends on it.

// The plugin and server lookup `tutti-session`'s plugin suite uses.
#[path = "../../tutti-session/tests/support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tutti::export::Written;
use tutti::prelude::*;

const RATE: SampleRate = SampleRate(48_000.0);

/// §5.2, as written there: `plugin` plays `midi` into `out`.
fn play(plugin: &Path, midi: &Path, out: &Path) -> Result<Written, Box<dyn std::error::Error>> {
    let mut s = Session::builder().build_offline(RATE, ChannelLayout::STEREO, Samples(512))?;

    let job = s.plugin_load(PluginLoadRequest {
        id: PluginId::from_path(plugin),
        state: None,
    });
    // The thread that opens an in-process plugin is the main thread: claim it once.
    let main = tutti::plugin::MainThread::claim()?;
    let synth = s.add_plugin(&main, job.run())?;
    s.graph().set_outputs_from(synth);

    let file = MidiFile::read(midi)?; // an SMF or a Clip File
    let tempo = file.tempo_map().tempo_at(Beat(0.0)); // the tempo at beat 0
    // Its messages on the file's timeline (a format 2 file has none), as the
    // packets they are: the clip translates them when it is loaded.
    let timeline = file.events().ok_or("a format 2 file has no one timeline")?;
    s.midi().play_clip(
        synth,
        timeline.filter_map(|(beat, event)| {
            let message = event.message()?;
            Some(TimedMidiEvent::from((
                beat,
                tutti::ump::Ump::from(*message),
            )))
        }),
    )?;
    s.transport().settings.set_tempo(tempo);
    s.update();

    let mut config = ExportConfig::default();
    config.render.sample_rate = RATE;
    // The file's length in seconds, integrated over its tempo map.
    config.render.duration_seconds = file.seconds_at(file.end_beat());
    let job = s.export(
        ExportRequest::file(out, config)
            .on_timeline(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
                tempo,
                sample_rate: RATE,
                ..Default::default()
            })))
            .trim_reported_latency() // RenderConfig::latency, from the plan
            .with_reported_tail(Samples(48_000 * 10)), // the plugin's declared tail
    )?; // the plugin hears Offline from here
    let Some(ExportOutput::File(written)) = s.finish_export(job.run())?.finished() else {
        unreachable!("a file request writes a file");
    }; // and Realtime from here
    Ok(written)
}

/// Two bars: a note on each beat, 120 BPM for the first four beats and 60
/// for the last four, as a file with a tempo map.
fn phrase(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use tutti::midi_file::{FileEvent, Meta, SmfFormat, Tick, Track};
    use tutti::ump::{Address, Message, NoteNumber, U7, Voice1};
    const TPQ: u16 = 480;
    let message = |tick: u64, key: u8, on: bool| {
        let (note, velocity) = (NoteNumber::of(key), U7::of(100));
        let voice = if on {
            Voice1::NoteOn { note, velocity }
        } else {
            Voice1::NoteOff { note, velocity }
        };
        let message = Message::Midi1(Address::first(MidiChannel::FIRST), voice);
        (Tick(tick), FileEvent::Message(message))
    };
    let tempo = |tick: u64, bpm: f64| (Tick(tick), FileEvent::Meta(Meta::tempo(Bpm::of(bpm))));
    let beat = |n: u64| n * u64::from(TPQ);
    let mut events = vec![tempo(0, 120.0)];
    for n in 0..8u64 {
        if n == 4 {
            events.push(tempo(beat(4), 60.0)); // 60 BPM from beat 4
        }
        let key = 60 + n as u8;
        events.push(message(beat(n), key, true));
        events.push(message(beat(n + 1), key, false));
    }
    MidiFile::smf(SmfFormat::SingleTrack, TPQ, vec![Track::new(events)])?.write(path)?;
    Ok(())
}

/// The phrase through the reference CLAP plugin, in `dir`.
fn reference(dir: &Path) -> Result<Written, Box<dyn std::error::Error>> {
    // SAFETY: set before the first server is spawned, on the only thread
    // that reads the environment.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", support::plugin_server()) };
    let midi = dir.join("phrase.mid");
    phrase(&midi)?;
    play(&support::clap_probe(), &midi, &dir.join("phrase.wav"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let written = match args.as_slice() {
        [plugin, midi, out] => play(plugin, midi, out)?,
        [] => reference(&std::env::temp_dir())?,
        _ => return Err("usage: plugin_midi_export [PLUGIN MIDI OUT]".into()),
    };
    println!("wrote {} ({} bytes)", written.path.display(), written.bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    /// **The §5.2 example plays**: the phrase through the reference CLAP
    /// plugin writes a WAV holding the file's length plus the plugin's tail.
    /// Fails, not skips, without `plugin-server`.
    ///
    /// The length is pinned in frames: four beats at 120 BPM and four at 60
    /// are 2 s + 4 s; past them, the probe's declared tail (8191 frames,
    /// `REPORTED_TAIL_SAMPLES`); the latency the plan compensated is trimmed
    /// from the front, not added. The export's default is 24-bit stereo,
    /// 6 bytes a frame, after a header of at least 44 bytes.
    ///
    /// Mutation (run, failed): the duration at the file's first tempo
    /// throughout (`file.end_beat()` at 120 BPM: 4 s) → 2 s short.
    #[test]
    fn the_example_plays() {
        let dir = std::env::temp_dir().join(format!("tutti-5-2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let written = super::reference(&dir).expect("the example runs");
        let frames = 6 * 48_000 + u64::from(tutti_clap_test_plugin::REPORTED_TAIL_SAMPLES);
        let header = written.bytes.checked_sub(frames * 6);
        assert!(
            header.is_some_and(|h| (44..256).contains(&h)),
            "{} bytes for {frames} frames of 24-bit stereo",
            written.bytes
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
