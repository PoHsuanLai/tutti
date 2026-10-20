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
//! cargo run -p tutti-session --features clap,export --example plugin_midi_export
//! cargo run -p tutti-session --features clap,export --example plugin_midi_export -- \
//!     Synth.clap song.mid song.wav
//! ```
//!
//! CI runs it as a test (`test = true`), which plays the phrase.

#[path = "../tests/support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig};
use tutti_core::{Bpm, ChannelLayout, SampleRate, Samples};
use tutti_export::{ExportConfig, Written};
use tutti_midi_file::MidiFile;
use tutti_midi_runtime::TimedMidiEvent;
use tutti_plugin::catalog::PluginId;
use tutti_session::{ExportOutput, ExportRequest, PluginLoadRequest, Session};

const RATE: SampleRate = SampleRate(48_000.0);

/// §5.2, as written there: `plugin` plays `midi` into `out`.
fn play(plugin: &Path, midi: &Path, out: &Path) -> Result<Written, Box<dyn std::error::Error>> {
    let mut s = Session::builder().build_offline(RATE, ChannelLayout::STEREO, Samples(512))?;

    let job = s.plugin_load(PluginLoadRequest {
        id: PluginId::from_path(plugin),
        state: None,
    });
    let synth = s.add_plugin(job.run())?;
    s.graph().set_outputs_from(synth);

    let file = MidiFile::read(midi)?; // an SMF or a Clip File
    let tempo = file.tempo().unwrap_or(Bpm(120.0)); // the tempo at beat 0
    s.midi()
        .play_clip(synth, file.timed().map(TimedMidiEvent::from))?;
    s.transport().settings.set_tempo(tempo);
    s.update();

    let mut config = ExportConfig::default();
    config.render.sample_rate = RATE;
    // The file's length in seconds, integrated over its tempo map.
    config.render.duration_seconds = file.seconds_at(file.end());
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
    let ExportOutput::File(written) = s.finish_export(job.run())? else {
        unreachable!("a file request writes a file");
    }; // and Realtime from here
    Ok(written)
}

/// Two bars: a note on each beat, 120 BPM for the first four beats and 60
/// for the last four. Written through `midly` (re-exported by
/// `tutti-midi-file`): this crate's own writer sets one tempo only.
fn phrase(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use tutti_midi_file::midly::{
        num::u28, Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind,
    };
    const TPQ: u32 = 480;
    let event = |delta: u32, kind| TrackEvent {
        delta: u28::new(delta),
        kind,
    };
    let note = |delta: u32, key: u8, on: bool| {
        let (key, vel) = (key.into(), 100.into());
        event(
            delta,
            TrackEventKind::Midi {
                channel: 0.into(),
                message: if on {
                    MidiMessage::NoteOn { key, vel }
                } else {
                    MidiMessage::NoteOff { key, vel }
                },
            },
        )
    };
    let tempo = |delta: u32, us_per_quarter: u32| {
        event(
            delta,
            TrackEventKind::Meta(MetaMessage::Tempo(us_per_quarter.into())),
        )
    };
    let mut track = vec![tempo(0, 500_000)]; // 120 BPM
    for beat in 0..8u8 {
        if beat == 4 {
            track.push(tempo(0, 1_000_000)); // 60 BPM from beat 4
        }
        track.push(note(0, 60 + beat, true));
        track.push(note(TPQ, 60 + beat, false));
    }
    track.push(event(0, TrackEventKind::Meta(MetaMessage::EndOfTrack)));
    let smf = Smf {
        header: Header::new(Format::SingleTrack, Timing::Metrical(480.into())),
        tracks: vec![track],
    };
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes)?;
    std::fs::write(path, bytes)?;
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
    /// throughout (`file.end()` at 120 BPM: 4 s) → 2 s short.
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
