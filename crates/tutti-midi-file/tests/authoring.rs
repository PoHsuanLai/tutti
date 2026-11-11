//! Writing a file and playing one: the `Track` builder, the timeline a clip
//! plays, the typed refusals, and the byte-stream doors.
//!
//! The expected figures are written from the specifications (SMF 1.0 for the
//! tick grid and the SysEx event, M2-104 section 7.7 for SysEx7 packets), not
//! read back from the code under test.

use std::io::Cursor;
use std::sync::Arc;

use tutti_midi_file::{
    ConfigError, Error, FileEvent, Meta, MidiFile, OutOfOrder, Refusal, SmfFormat, TextKind, Tick,
    Track,
};
use tutti_types::{Beat, Bpm};
use tutti_ump::{
    Address, Message, MidiChannel, MidiGroup, NoteNumber, SysexStatus, U7, Ump, Voice1,
};

const TPQ: u16 = 480;

fn on(tick: u64, key: u8) -> (Tick, FileEvent) {
    (
        Tick(tick),
        Message::note_on(MidiChannel::FIRST, NoteNumber::of(key), U7::of(100)).into(),
    )
}

fn file(tracks: Vec<Track>) -> MidiFile {
    let format = if tracks.len() == 1 {
        SmfFormat::SingleTrack
    } else {
        SmfFormat::Parallel
    };
    MidiFile::smf(format, TPQ, tracks).unwrap()
}

// ---- Tick ------------------------------------------------------------------

/// A tick is a count: `+` and `-` with a `u64` and `+=` move it by that many
/// ticks, and a quarter note is `ticks_per_quarter` of them.
#[test]
fn a_tick_moves_by_counts_and_a_quarter_is_the_division() {
    let mut t = Tick(1_000);
    assert_eq!(t + 480, Tick(1_480));
    assert_eq!(t - 400, Tick(600));
    t += 24;
    assert_eq!(t, Tick(1_024));
    assert_eq!(Tick::quarters(3, 96), Tick(288));
    assert_eq!(Tick(720).beats(480), Beat(1.5));
    assert_eq!(Tick::quarters(7, TPQ).beats(TPQ), Beat(7.0));
}

// ---- the builder -----------------------------------------------------------

/// A note is a Note On and a Note Off `len` ticks later on its channel, and
/// the track ends where the last event does.
#[test]
fn a_note_is_an_on_and_an_off_len_ticks_later() {
    let ch = MidiChannel::of(4);
    let track = Track::new().note(Tick(100), 50, ch, NoteNumber::of(60), U7::of(90));
    let at = |m| Address::first(ch) == m;
    assert_eq!(track.events.len(), 2);
    let (t0, e0) = &track.events[0];
    let (t1, e1) = &track.events[1];
    assert_eq!((*t0, *t1), (Tick(100), Tick(150)));
    assert!(matches!(
        e0.message(),
        Some(Message::Midi1(a, Voice1::NoteOn { note, velocity }))
            if at(*a) && note.get() == 60 && velocity.get() == 90
    ));
    assert!(matches!(
        e1.message(),
        Some(Message::Midi1(a, Voice1::NoteOff { note, .. })) if at(*a) && note.get() == 60
    ));
    assert_eq!(track.end, Tick(150));
}

/// Notes added in any order, a chord and overlapping notes all leave the
/// events in non-decreasing tick order, which the file layer requires.
#[test]
fn notes_in_any_order_leave_the_events_in_tick_order() {
    let ch = MidiChannel::FIRST;
    let n = |k| NoteNumber::of(k);
    let track = Track::new()
        .note(Tick(480), 480, ch, n(64), U7::of(100))
        .note(Tick(0), 960, ch, n(60), U7::of(100)) // overlaps the first
        .note(Tick(0), 480, ch, n(67), U7::of(100)); // a chord with it
    let ticks: Vec<u64> = track.events.iter().map(|(t, _)| t.0).collect();
    assert_eq!(ticks, [0, 0, 480, 480, 960, 960]);
    assert_eq!(track.end, Tick(960));
    // And a file takes it as it is.
    MidiFile::smf(SmfFormat::SingleTrack, TPQ, vec![track]).unwrap();
}

/// `tempo` is a Set Tempo of that figure at that tick (`Meta::tempo`), `meta`
/// any meta event, both placed by tick.
#[test]
fn tempo_and_meta_are_events_at_their_ticks() {
    let track = Track::new()
        .meta(Tick(960), Meta::text(TextKind::Marker, "B"))
        .tempo(Tick(0), Bpm::of(90.0));
    assert_eq!(
        track.events,
        [
            (Tick(0), FileEvent::Meta(Meta::tempo(Bpm::of(90.0)))),
            (
                Tick(960),
                FileEvent::Meta(Meta::text(TextKind::Marker, "B"))
            ),
        ]
    );
    assert_eq!(track.end, Tick(960));
}

/// `push` appends: an event before the last is refused with both ticks, and
/// the track is as it was; at the same tick is allowed.
#[test]
fn push_refuses_an_event_before_the_last() {
    let track = Track::new()
        .push(
            Tick(10),
            Message::note_on(MidiChannel::FIRST, NoteNumber::of(60), U7::of(1)),
        )
        .unwrap()
        .push(Tick(10), Meta::tempo(Bpm::of(100.0)))
        .unwrap();
    assert_eq!(track.events.len(), 2);
    assert_eq!(track.end, Tick(10));
    let err = track
        .clone()
        .push(Tick(9), Meta::tempo(Bpm::of(100.0)))
        .unwrap_err();
    assert_eq!(
        err,
        OutOfOrder {
            last: Tick(10),
            at: Tick(9)
        }
    );
    assert!(err.to_string().contains("tick 9"), "{err}");
    // The typed error crosses `?` into the crate's.
    let as_error: Error = err.into();
    assert!(matches!(as_error, Error::OutOfOrder(e) if e == err));
}

/// Collecting pairs puts them in tick order, stable: events at one tick keep
/// the order they were given in, so a Note Off before a Note On stays so.
#[test]
fn collecting_events_sorts_them_by_tick_keeping_ties_in_order() {
    let (a, b, c) = (on(480, 1), on(0, 2), on(480, 3));
    let track: Track = [a.clone(), b.clone(), c.clone()].into_iter().collect();
    assert_eq!(track.events, [b.clone(), a.clone(), c.clone()]);
    assert_eq!(track.end, Tick(480));
    // Extending an existing track merges into the same order.
    let mut more = Track::new().push(b.0, b.1.clone()).unwrap();
    more.extend([c.clone(), a.clone()]);
    assert_eq!(more.events, [b, c, a]);
    assert_eq!(more.end, Tick(480));
}

// ---- the timeline ----------------------------------------------------------

/// The timeline of an SMF is its messages as their packets at tick over
/// division beats, in tick order across tracks, with the tempo and text meta
/// events left out (the tempo is the file's map).
#[test]
fn the_timeline_is_the_messages_at_their_beats_and_not_the_meta_events() {
    let tempo_track = Track::new()
        .tempo(Tick(0), Bpm::of(60.0))
        .meta(Tick(0), Meta::text(TextKind::TrackName, "tempo"));
    let notes = Track::new()
        .note(
            Tick(480),
            240,
            MidiChannel::FIRST,
            NoteNumber::of(60),
            U7::of(100),
        )
        .note(
            Tick(0),
            120,
            MidiChannel::of(1),
            NoteNumber::of(62),
            U7::of(80),
        );
    let f = file(vec![tempo_track, notes]);
    let line: Vec<(Beat, Ump)> = f.timeline().unwrap().collect();
    let words = |ump: &Ump| ump.0[0];
    let got: Vec<(f64, u32)> = line.iter().map(|(b, u)| (b.get(), words(u))).collect();
    // M2-104 7.3: type 2, group 0, status, channel, note, velocity.
    assert_eq!(
        got,
        [
            (0.0, 0x2091_3E50),  // channel 1, note 62, velocity 80
            (0.25, 0x2081_3E40), // its off, release 64
            (1.0, 0x2090_3C64),  // channel 0, note 60, velocity 100
            (1.5, 0x2080_3C40),
        ]
    );
}

/// A SysEx event goes out as its SysEx7 packets (M2-104 section 7.7) on
/// Group 1, all at the event's beat: ten payload bytes are a Start of six
/// and an End of four. A divided message (`F0` with no closing `F7`) and one
/// with a byte of 128 or more are not reassembled or forwarded.
#[test]
fn a_sysex_event_is_its_sysex7_packets_at_its_beat() {
    let mut sysex = vec![0xF0];
    sysex.extend(1..=10u8);
    sysex.push(0xF7);
    let track = Track::new()
        .meta(Tick(0), Meta::tempo(Bpm::of(120.0)))
        .push(Tick(960), FileEvent::SysEx(sysex[1..].into()))
        .unwrap()
        .push(Tick(960), FileEvent::SysEx(vec![1, 2, 3].into())) // divided: no F7
        .unwrap()
        .push(Tick(960), FileEvent::SysEx(vec![0x90, 0xF7].into())) // 8-bit data
        .unwrap();
    let f = file(vec![track]);
    let line: Vec<(Beat, Ump)> = f.timeline().unwrap().collect();
    assert_eq!(line.len(), 2);
    assert!(line.iter().all(|(b, _)| *b == Beat(2.0)));
    let packets: Vec<_> = line
        .iter()
        .map(|(_, u)| match u.decode() {
            Ok(Message::Sysex7(group, p)) => (group, p.status(), p.data().to_vec()),
            other => panic!("not SysEx7: {other:?}"),
        })
        .collect();
    assert_eq!(
        packets,
        [
            (MidiGroup::FIRST, SysexStatus::Start, vec![1, 2, 3, 4, 5, 6]),
            (MidiGroup::FIRST, SysexStatus::End, vec![7, 8, 9, 10]),
        ]
    );
}

/// A format 2 file has no one timeline; each of its sequences has its own, on
/// its own division.
#[test]
fn a_format_2_file_has_no_timeline_but_each_track_has() {
    let a = Track::new().note(
        Tick(480),
        480,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        U7::of(100),
    );
    let f = MidiFile::smf(SmfFormat::Sequential, TPQ, vec![a, Track::new()]).unwrap();
    assert!(f.timeline().is_none());
    let beats: Vec<Beat> = f.tracks()[0]
        .timeline(f.ticks_per_quarter())
        .map(|(b, _)| b)
        .collect();
    assert_eq!(beats, [Beat(1.0), Beat(2.0)]);
}

/// A Clip File's packets are the timeline as they are stored (a MIDI 2.0 note
/// stays MIDI 2.0), and its configuration header is not on it.
#[test]
fn a_clip_files_timeline_is_its_packets_as_stored_without_the_header() {
    let program = Message::program_change2(MidiChannel::FIRST, U7::of(5));
    let note = Message::note_on2(MidiChannel::FIRST, NoteNumber::of(60), 0xFFFF);
    let track = Track {
        header: vec![(Tick(0), program.into())],
        events: vec![(Tick(240), note.into())],
        end: Tick(240),
        ..Track::new()
    };
    let f = MidiFile::clip(TPQ, track).unwrap();
    let line: Vec<(Beat, Ump)> = f.timeline().unwrap().collect();
    assert_eq!(line, [(Beat(0.5), Ump::from(note))]);
}

// ---- the tempo map ---------------------------------------------------------

/// The tempo map is shared, not copied: a clone of the file and the accessor
/// for the one timeline's track hand out the one map.
#[test]
fn the_tempo_map_is_one_shared_value() {
    let f = file(vec![Track::new().tempo(Tick(0), Bpm::of(75.0))]);
    let again = f.clone();
    assert!(Arc::ptr_eq(f.tempo_map(), again.tempo_map()));
    assert!(Arc::ptr_eq(f.tempo_map(), f.tempo_map_of(0).unwrap()));
    assert_eq!(f.tempo_map().tempo_at(Beat(0.0)), Bpm::of(75.0));
}

// ---- typed refusals --------------------------------------------------------

/// Each rule a track list breaks is its own variant, with the track and the
/// tick that broke it.
#[test]
fn a_refused_track_list_names_the_rule_and_where() {
    let config = |r: Result<MidiFile, Error>| match r {
        Err(Error::Config(c)) => c,
        other => panic!("not a config error: {other:?}"),
    };
    assert_eq!(
        config(MidiFile::smf(SmfFormat::Parallel, TPQ, vec![])),
        ConfigError::NoTracks
    );
    assert_eq!(
        config(MidiFile::smf(SmfFormat::Parallel, 0, vec![Track::new()])),
        ConfigError::Division {
            ticks_per_quarter: 0
        }
    );
    assert_eq!(
        config(MidiFile::smf(
            SmfFormat::SingleTrack,
            TPQ,
            vec![Track::new(), Track::new()]
        )),
        ConfigError::SingleTrack { tracks: 2 }
    );
    // The second track goes back from tick 20 to tick 5.
    let back = Track {
        events: vec![on(20, 60), on(5, 61)],
        end: Tick(20),
        ..Track::new()
    };
    assert_eq!(
        config(MidiFile::smf(
            SmfFormat::Parallel,
            TPQ,
            vec![Track::new(), back]
        )),
        ConfigError::OutOfOrder {
            track: 1,
            at: Tick(5)
        }
    );
    // An SMF holds Group 1 only.
    let g2 = Message::note_on(
        Address::new(MidiGroup::of(1), MidiChannel::FIRST),
        NoteNumber::of(60),
        U7::of(1),
    );
    let t = Track::new().push(Tick(0), g2).unwrap();
    assert_eq!(
        config(MidiFile::smf(SmfFormat::SingleTrack, TPQ, vec![t])),
        ConfigError::Refused {
            track: 0,
            in_header: false,
            refusal: Refusal::SmfGroup
        }
    );
}

// ---- bytes in and out ------------------------------------------------------

/// A file written to a stream is the file `to_bytes` gives, and a stream or a
/// byte slice reads back the same file.
#[test]
fn a_file_round_trips_through_a_stream_and_a_slice() {
    let f = file(vec![Track::new().tempo(Tick(0), Bpm::of(100.0)).note(
        Tick(0),
        480,
        MidiChannel::FIRST,
        NoteNumber::of(60),
        U7::of(100),
    )]);
    let mut out = Vec::new();
    f.write_to(&mut out).unwrap();
    assert_eq!(out, f.to_bytes().unwrap());
    assert_eq!(MidiFile::read_from(Cursor::new(&out)).unwrap(), f);
    assert_eq!(MidiFile::try_from(out.as_slice()).unwrap(), f);
    // Bytes of neither container are refused as the other doors refuse them.
    assert!(matches!(
        MidiFile::try_from(&b"RIFF...."[..]),
        Err(Error::UnknownFormat)
    ));
}

/// Text meta events read as text when they are UTF-8, and as nothing when
/// they are not or are not text.
#[test]
fn text_str_is_the_utf8_text_of_a_text_event() {
    assert_eq!(Meta::text(TextKind::Lyric, "la").text_str(), Some("la"));
    let latin1 = Meta::Text {
        kind: TextKind::Text,
        bytes: vec![0xE9].into(),
    };
    assert_eq!(latin1.text_str(), None);
    assert_eq!(Meta::tempo(Bpm::of(120.0)).text_str(), None);
}
