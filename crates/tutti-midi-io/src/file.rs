use crate::error::{Error, Result};
use midly::{
    Format, Header, MetaMessage, MidiMessage, Smf, Timing, Track, TrackEvent, TrackEventKind,
};
use std::path::Path;
use tracing::debug;

/// A MIDI voice event positioned in musical time (beats from file start).
#[derive(Debug, Clone, Copy)]
pub struct SmfTimedEvent {
    /// Absolute time in beats from start of file.
    pub time_beats: f64,
    /// MIDI channel (0-15).
    pub channel: u8,
    /// The channel-voice message.
    pub msg: MidiMessage,
}

#[derive(Debug, Clone)]
pub struct ParsedMidiFile {
    pub events: Vec<SmfTimedEvent>,
    pub ticks_per_beat: u16,
    /// Default tempo in BPM (from first tempo event, or 120 if none).
    pub tempo_bpm: f64,
    pub duration_beats: f64,
}

impl ParsedMidiFile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let data = std::fs::read(path.as_ref())?;
        Self::parse(&data)
    }

    pub fn parse(data: &[u8]) -> Result<Self> {
        let smf = Smf::parse(data)?;

        let ticks_per_beat = match smf.header.timing {
            Timing::Metrical(tpb) => tpb.as_int(),
            Timing::Timecode(_, _) => return Err(Error::MidiUnsupportedTiming),
        };

        debug!(
            "Parsing MIDI file: {} tracks, {} tpb",
            smf.tracks.len(),
            ticks_per_beat
        );

        let mut all_events = Vec::new();
        let mut tempo_bpm = 120.0;
        let mut found_tempo = false;

        for track in &smf.tracks {
            all_events.extend(parse_track(track, ticks_per_beat));
            if !found_tempo {
                if let Some(t) = extract_tempo(track) {
                    tempo_bpm = t;
                    found_tempo = true;
                    debug!("Found tempo: {} BPM", tempo_bpm);
                }
            }
        }

        sort_by_time(&mut all_events);
        let duration_beats = all_events.last().map(|e| e.time_beats).unwrap_or(0.0);
        debug!(
            "Parsed {} events, {:.2} beats",
            all_events.len(),
            duration_beats
        );

        Ok(Self {
            events: all_events,
            ticks_per_beat,
            tempo_bpm,
            duration_beats,
        })
    }

    pub fn get_events_in_range(&self, start_beats: f64, end_beats: f64) -> &[SmfTimedEvent] {
        let start = self.events.partition_point(|e| e.time_beats < start_beats);
        let end = self.events[start..].partition_point(|e| e.time_beats < end_beats) + start;
        &self.events[start..end]
    }
}

// --- Parsing helpers ---

fn parse_track(track: &Track, ticks_per_beat: u16) -> Vec<SmfTimedEvent> {
    let mut events = Vec::new();
    let mut tick = 0u64;

    for event in track.iter() {
        tick += u64::from(event.delta.as_int());
        if let TrackEventKind::Midi { channel, message } = &event.kind {
            events.push(SmfTimedEvent {
                time_beats: tick as f64 / f64::from(ticks_per_beat),
                channel: channel.as_int(),
                msg: *message,
            });
        }
    }

    events
}

fn extract_tempo(track: &Track) -> Option<f64> {
    track.iter().find_map(|e| match &e.kind {
        TrackEventKind::Meta(MetaMessage::Tempo(t)) => Some(60_000_000.0 / f64::from(t.as_int())),
        _ => None,
    })
}

fn sort_by_time(events: &mut [SmfTimedEvent]) {
    events.sort_by(|a, b| a.time_beats.partial_cmp(&b.time_beats).unwrap());
}

// --- Writing ---

#[derive(Debug, Clone)]
pub struct MidiWriteOptions {
    pub ticks_per_beat: u16,
    pub tempo_bpm: Option<f64>,
    pub time_signature: Option<(u8, u8)>,
}

impl Default for MidiWriteOptions {
    fn default() -> Self {
        Self {
            ticks_per_beat: 480,
            tempo_bpm: None,
            time_signature: None,
        }
    }
}

/// Single track produces format 0; multiple tracks produce format 1.
pub fn write_midi_file(
    path: impl AsRef<Path>,
    tracks: &[Vec<SmfTimedEvent>],
    options: &MidiWriteOptions,
) -> Result<()> {
    let data = encode_midi_file(tracks, options)?;
    std::fs::write(path, data)?;
    Ok(())
}

/// Encode MIDI file to bytes in memory.
pub fn encode_midi_file(
    tracks: &[Vec<SmfTimedEvent>],
    options: &MidiWriteOptions,
) -> Result<Vec<u8>> {
    if tracks.is_empty() {
        return Err(Error::InvalidConfig("No tracks provided".into()));
    }

    let format = if tracks.len() == 1 {
        Format::SingleTrack
    } else {
        Format::Parallel
    };
    let header = Header::new(format, Timing::Metrical(options.ticks_per_beat.into()));
    let mut smf = Smf::new(header);

    for (i, track_events) in tracks.iter().enumerate() {
        smf.tracks.push(build_track(track_events, options, i == 0));
    }

    let mut buf = Vec::new();
    smf.write_std(&mut buf)
        .map_err(|e| Error::MidiFileParse(format!("Failed to encode MIDI: {}", e)))?;
    Ok(buf)
}

fn build_track<'a>(
    events: &[SmfTimedEvent],
    options: &MidiWriteOptions,
    include_meta: bool,
) -> Vec<TrackEvent<'a>> {
    let tpb = f64::from(options.ticks_per_beat);
    let mut track: Vec<TrackEvent<'a>> = Vec::new();

    if include_meta {
        if let Some(bpm) = options.tempo_bpm {
            let us = (60_000_000.0 / bpm) as u32;
            track.push(meta_event(MetaMessage::Tempo(us.into())));
        }
        if let Some((num, denom_pow)) = options.time_signature {
            track.push(meta_event(MetaMessage::TimeSignature(
                num, denom_pow, 24, 8,
            )));
        }
    }

    let mut sorted: Vec<&SmfTimedEvent> = events.iter().collect();
    sort_by_time_ref(&mut sorted);

    let mut last_tick: u32 = 0;
    for event in &sorted {
        let abs_tick = (event.time_beats * tpb) as u32;
        let delta = abs_tick.saturating_sub(last_tick);
        last_tick = abs_tick;

        track.push(TrackEvent {
            delta: delta.into(),
            kind: TrackEventKind::Midi {
                channel: (event.channel & 0x0F).into(),
                message: event.msg,
            },
        });
    }

    track.push(meta_event(MetaMessage::EndOfTrack));
    track
}

fn meta_event(msg: MetaMessage<'static>) -> TrackEvent<'static> {
    TrackEvent {
        delta: 0.into(),
        kind: TrackEventKind::Meta(msg),
    }
}

fn sort_by_time_ref(events: &mut [&SmfTimedEvent]) {
    events.sort_by(|a, b| a.time_beats.partial_cmp(&b.time_beats).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty_midi() {
        let data = [
            0x4D, 0x54, 0x68, 0x64, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x01, 0x01, 0xE0,
            0x4D, 0x54, 0x72, 0x6B, 0x00, 0x00, 0x00, 0x04, 0x00, 0xFF, 0x2F, 0x00,
        ];

        let file = ParsedMidiFile::parse(&data).unwrap();
        assert_eq!(file.ticks_per_beat, 480);
        assert_eq!(file.events.len(), 0);
    }

    #[test]
    fn test_write_and_read_roundtrip() {
        let events = vec![
            SmfTimedEvent {
                time_beats: 0.0,
                channel: 0,
                msg: MidiMessage::NoteOn {
                    key: 60.into(),
                    vel: 100.into(),
                },
            },
            SmfTimedEvent {
                time_beats: 1.0,
                channel: 0,
                msg: MidiMessage::NoteOff {
                    key: 60.into(),
                    vel: 0.into(),
                },
            },
        ];

        let options = MidiWriteOptions {
            ticks_per_beat: 480,
            tempo_bpm: Some(120.0),
            time_signature: Some((4, 2)),
        };

        let data = encode_midi_file(&[events], &options).unwrap();
        let parsed = ParsedMidiFile::parse(&data).unwrap();

        assert_eq!(parsed.ticks_per_beat, 480);
        assert!((parsed.tempo_bpm - 120.0).abs() < 0.1);
        assert_eq!(parsed.events.len(), 2);
        assert!((parsed.events[0].time_beats - 0.0).abs() < 0.001);
        assert!((parsed.events[1].time_beats - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_write_empty_tracks_error() {
        let result = encode_midi_file(&[], &MidiWriteOptions::default());
        assert!(result.is_err());
    }
}
