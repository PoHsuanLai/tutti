//! `PolySynth`'s decisions against the ones it made before its note logic
//! moved into `tutti-voicing`.
//!
//! `decisions/old.txt` is what the code at #324 (`7689503c`) did, captured by
//! playing `decisions/streams.rs` on it with a probe that listed the voice table
//! after every message: one line a stream, one 32-bit hash (FNV-1a) a message of
//! that table as text (`start order:channel:key:phase`, sorted). The same file
//! plays the same streams here. The probe, for the next time (it is a `pub`
//! `#[doc(hidden)]` method on `PolySynth` in the old code, listing every slot
//! that holds a voice):
//!
//! ```ignore
//! (slot.voice_id(), slot.channel(), slot.note(), phase) // phase: 0 key held,
//! // 1 held by a pedal, 2 releasing; sorted
//! ```
//!
//! A stream is the same on both sides until a difference, and every
//! difference has a known change before it: a bug of the old allocator the
//! streams found, or a decided behaviour change.

#[path = "decisions/streams.rs"]
mod streams;

use std::collections::BTreeMap;

use streams::Op;

fn table(s: &tutti_polysynth::PolySynth) -> streams::Table {
    use tutti_voicing::Phase;
    let mut t: Vec<_> = s
        .sounding()
        .into_iter()
        .map(|g| {
            (
                g.seq,
                g.channel.get(),
                g.key.get(),
                match g.phase {
                    Phase::Key => 0,
                    Phase::Pedal => 1,
                    Phase::Releasing => 2,
                },
            )
        })
        .collect();
    t.sort_unstable();
    t
}

fn old() -> Vec<Vec<u32>> {
    include_str!("decisions/old.txt")
        .lines()
        .map(|l| {
            l.split_whitespace()
                .map(|h| u32::from_str_radix(h, 16).expect("a hash"))
                .collect()
        })
        .collect()
}

/// What the old code did wrong, or what was decided, that a stream can show.
/// (#324 had already fixed the others the streams found at an earlier commit:
/// a repeated key left the first voice sounding, CC 121 and CC 123 ignored a
/// pedal, and CC 123 restarted a release that was running.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Fix {
    /// A key struck while an earlier voice of it sounds: the earlier one is
    /// released and the new one lives, with the same id. When the releasing
    /// one was stolen, the old allocator removed that id, the *live* voice's
    /// too, so its note-off found nothing and the key stuck.
    StaleId,
    /// Mono and legato are one voice for each channel (decided, #324 follow-up
    /// E4). The old allocator had one slot and one legato memory for all
    /// channels: a second channel's note took the slot, the first channel's
    /// note-off then released the second's voice, and a note-off on one
    /// channel cleared the other's legato memory.
    MonoPerChannel,
}

/// The fixes streams `ops[..=upto]` can have triggered, read from the table
/// the stream had before each message (the same on both sides until the first
/// difference, so the new run's own tables serve).
fn triggers(ops: &[Op], tables: &[String], upto: usize, mono: bool) -> Vec<(Fix, usize)> {
    let parse = |line: &str| -> Vec<(u32, u8, u8, u8)> {
        line.split(" | ")
            .nth(1)
            .unwrap_or("")
            .split_whitespace()
            .map(|v| {
                let f: Vec<u32> = v.split(':').map(|n| n.parse().unwrap()).collect();
                (f[0], f[1] as u8, f[2] as u8, f[3] as u8)
            })
            .collect()
    };
    let mut found = vec![];
    let mut channels_used = 0u8;
    for j in 0..=upto {
        let before = if j == 0 {
            vec![]
        } else {
            parse(&tables[j - 1])
        };
        match ops[j] {
            Op::On(c, _) | Op::Off(c, _)
                if mono && {
                    channels_used |= 1 << c;
                    channels_used == 0b11
                } =>
            {
                found.push((Fix::MonoPerChannel, j));
            }
            Op::On(c, k) if !mono && before.iter().any(|v| (v.1, v.2) == (c, k)) => {
                found.push((Fix::StaleId, j));
            }
            _ => {}
        }
    }
    found.sort();
    found.dedup_by_key(|f| f.0);
    found
}

#[test]
fn decisions_against_the_code_before_the_shared_note_logic() {
    let old = old();
    assert_eq!(old.len(), streams::STREAMS);
    let (mut identical, mut messages, mut before_first_fix, mut clean) = (0, 0, 0, 0);
    let mut by_fix: BTreeMap<Vec<Fix>, usize> = BTreeMap::new();
    let mut unexplained = vec![];
    for (i, want) in old.iter().enumerate() {
        let lines = streams::play(i, table);
        messages += lines.len();
        let (ops, cfg) = streams::stream(i);
        let mono = cfg.voice_mode != tutti_polysynth::VoiceMode::Poly;
        let hash = |l: &String| streams::fnv(l.split(" | ").nth(1).unwrap_or(""));
        let first_diff = lines.iter().zip(want).position(|(l, w)| hash(l) != *w);
        let fixes = triggers(&ops, &lines, first_diff.unwrap_or(ops.len() - 1), mono);
        let first_fix = fixes.iter().map(|f| f.1).min().unwrap_or(ops.len());
        before_first_fix += first_fix.min(first_diff.unwrap_or(first_fix));
        match first_diff {
            None => {
                identical += 1;
                clean += usize::from(fixes.is_empty());
            }
            Some(j) if fixes.is_empty() || first_fix > j => unexplained.push(format!(
                "stream {i} message {j} ({:?}, {:?}): new {}",
                ops[j], cfg.voice_mode, lines[j]
            )),
            Some(_) => {
                *by_fix
                    .entry(fixes.iter().map(|f| f.0).collect())
                    .or_default() += 1;
            }
        }
    }
    println!(
        "{messages} messages in {} streams; {identical} streams identical to the end; \
         {before_first_fix} messages precede their stream's first fix and all match; \
         {clean} streams trigger no fix; \
         differences by the fixes before them: {by_fix:?}",
        streams::STREAMS
    );
    assert!(
        unexplained.is_empty(),
        "differences no fix explains:\n{}",
        unexplained.join("\n")
    );
}

#[test]
#[ignore]
fn dump() {
    let i: usize = std::env::var("STREAM").unwrap().parse().unwrap();
    for (j, l) in streams::play(i, table).iter().enumerate() {
        println!("{j:2} {l}");
    }
}
