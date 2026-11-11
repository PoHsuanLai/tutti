use tutti_ump::Form;

use super::*;

fn collect<const W: usize>(bytes: &[u8]) -> Vec<(Form, [u8; W])> {
    let mut out = Vec::new();
    packets::<W>(bytes, |form, packet| out.push((form, packet)));
    out
}

// The longest text of each kind, from the packet counts of M2-104 §7.1.4,
// §7.1.5 and §7.1.9.
//
// Mutation record: `Name<N>::from_bytes` refusing `> N` as `>= N` fails the
// 98/91/42 accepts; as `> N + 1` fails the refusals.
#[test]
fn a_name_holds_exactly_what_its_packets_carry() {
    assert!(EndpointName::new(&"a".repeat(98)).is_some());
    assert!(EndpointName::new(&"a".repeat(99)).is_none());
    assert!(BlockName::new(&"a".repeat(91)).is_some());
    assert!(BlockName::new(&"a".repeat(92)).is_none());
    assert!(ProductInstanceId::new(&"a".repeat(42)).is_some());
    assert!(ProductInstanceId::new(&"a".repeat(43)).is_none());
}

// Mutation record: dropping the NUL check in `from_bytes` fails this.
#[test]
fn a_nul_cannot_be_in_a_name_because_it_ends_one() {
    assert!(EndpointName::new("a\0b").is_none());
    assert_eq!(EndpointName::new("").unwrap().as_str(), "");
    assert!(EndpointName::new("").unwrap().is_empty());
    assert_eq!(EndpointName::new("Pièce 5").unwrap().as_str(), "Pièce 5");
}

// M2-104 §1.8: "MIDI messages only use normalized UTF-8 encoded Unicode
// characters, following the NFC (Normalization Form C) standard", and "Unicode
// text in MIDI messages shall not include a Byte Order Mark." A name is composed
// to NFC (at the constructor and at the wire) and a leading BOM is refused.
#[test]
fn a_name_is_held_in_nfc() {
    // "e" and a combining acute accent compose to the one character "é".
    let decomposed = "Cafe\u{301}";
    let composed = "Caf\u{e9}";
    assert_eq!(EndpointName::new(decomposed).unwrap().as_str(), composed);
    assert_eq!(
        BlockName::new(decomposed).unwrap().as_bytes(),
        composed.as_bytes()
    );
    // The wire's bytes are read the same way.
    assert_eq!(
        EndpointName::from_bytes(decomposed.as_bytes())
            .unwrap()
            .as_str(),
        composed
    );
    // A name that is NFC already is kept as it is.
    assert_eq!(EndpointName::new(composed).unwrap().as_str(), composed);
    // The bound is on the composed bytes: 49 "e"+accent are 147 bytes as sent
    // and 98 composed, which fits an Endpoint Name; a fiftieth does not.
    assert!(EndpointName::new(&"e\u{301}".repeat(49)).is_some());
    assert!(EndpointName::new(&"e\u{301}".repeat(50)).is_none());
}

#[test]
fn a_leading_byte_order_mark_is_refused() {
    let bom = "\u{FEFF}Piano";
    assert!(EndpointName::new(bom).is_none());
    assert!(BlockName::new(bom).is_none());
    assert!(EndpointName::from_bytes(bom.as_bytes()).is_none());
    assert!(BlockName::from_bytes(b"\xEF\xBB\xBFPiano").is_none());
    assert!(EndpointName::new("Piano").is_some());
}

// §7.1.5: ASCII 32 to 126. Mutation record: `32..=126` as `33..=126` fails the
// space, as `32..=127` fails DEL, as `0..=126` fails the control byte.
#[test]
fn an_instance_id_is_printable_ascii() {
    assert!(ProductInstanceId::new(" ~").is_some());
    assert!(ProductInstanceId::new("\u{1f}").is_none());
    assert!(ProductInstanceId::new("\u{7f}").is_none());
    assert!(ProductInstanceId::new("é").is_none());
}

// §7.1.4: "a Complete message (if less than 15 bytes)", else a Start, up to five
// Continue and an End. Mutation record: `div_ceil` as plain division leaves the
// tail unsent (the 15-byte case); the `(_, 1)` arm removed makes a short name
// a Start with no End.
#[test]
fn packets_are_complete_start_continue_end() {
    assert!(collect::<14>(b"").is_empty());
    let one = collect::<14>(&[b'a'; 14]);
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].0, Form::Complete);
    let two = collect::<14>(&[b'a'; 15]);
    assert_eq!(
        two.iter().map(|p| p.0).collect::<Vec<_>>(),
        [Form::Start, Form::End]
    );
    assert_eq!(two[1].1[..2], [b'a', 0]);
    let seven = collect::<14>(&[b'a'; 98]);
    assert_eq!(
        seven.iter().map(|p| p.0).collect::<Vec<_>>(),
        [
            Form::Start,
            Form::Continue,
            Form::Continue,
            Form::Continue,
            Form::Continue,
            Form::Continue,
            Form::End
        ]
    );
    assert!(seven[6].1.iter().all(|&b| b == b'a'));
}

fn assemble<const N: usize>(parts: &[(Form, &[u8])]) -> Vec<Option<Vec<u8>>> {
    let mut asm = Assembler::<N>::new();
    parts
        .iter()
        .map(|(form, bytes)| asm.feed(*form, bytes).map(<[u8]>::to_vec))
        .collect()
}

// Mutation record: not cutting at the first NUL returns the padding; clearing
// `len` on `Start` only (not on `Complete`) fails the second case.
#[test]
fn the_assembler_cuts_at_the_padding_and_restarts_on_a_start() {
    let out = assemble::<98>(&[
        (Form::Start, b"AAAAAAAAAAAAAA"),
        (Form::End, b"BB\0\0\0\0\0\0\0\0\0\0\0\0"),
    ]);
    assert_eq!(out, [None, Some(b"AAAAAAAAAAAAAABB".to_vec())]);
    let out = assemble::<98>(&[
        (Form::Start, b"XXXXXXXXXXXXXX"),
        (Form::Complete, b"hi\0\0\0\0\0\0\0\0\0\0\0\0"),
    ]);
    assert_eq!(out, [None, Some(b"hi".to_vec())]);
}

// Mutation record: dropping the `open` check makes the orphan End return text;
// dropping the capacity check panics on the overlong message; not closing the
// message on overflow lets the empty End complete it.
#[test]
fn the_assembler_drops_what_has_no_start_or_does_not_fit() {
    assert_eq!(assemble::<98>(&[(Form::End, b"zz")]), [None]);
    assert_eq!(assemble::<98>(&[(Form::Continue, b"zz")]), [None]);
    let long = [b'a'; 14];
    let mut parts = vec![(Form::Start, &long[..])];
    parts.extend([(Form::Continue, &long[..]); 5]);
    parts.extend([(Form::Continue, &long[..]); 2]); // 98 bytes, then 112 > 98
    // The End is empty: were the message still open it would complete with the
    // 98 bytes before the overflow.
    parts.push((Form::End, &[]));
    assert!(assemble::<98>(&parts).iter().all(Option::is_none));
}

#[test]
fn the_packets_of_a_name_reassemble_to_it() {
    for len in [1, 13, 14, 15, 28, 29, 91, 98] {
        let text = "ab".repeat(49)[..len].to_owned();
        let mut asm = Assembler::<98>::new();
        let mut done = None;
        packets::<14>(text.as_bytes(), |form, b| {
            if let Some(t) = asm.feed(form, &b) {
                done = Some(t.to_vec());
            }
        });
        assert_eq!(done.as_deref(), Some(text.as_bytes()), "{len}");
    }
}

// §7.1.4, §7.1.5, §7.1.9: a name at its limit is seven, seven and three packets.
//
// Mutation record: a limit one higher fails `a_name_holds_exactly_what_its_packets_carry`;
// a 13-byte split of the Endpoint Name makes it eight packets.
#[test]
fn the_longest_names_are_seven_seven_and_three_packets() {
    assert_eq!(
        collect::<14>(EndpointName::new(&"a".repeat(98)).unwrap().as_bytes()).len(),
        7
    );
    assert_eq!(
        collect::<13>(BlockName::new(&"a".repeat(91)).unwrap().as_bytes()).len(),
        7
    );
    assert_eq!(
        collect::<14>(
            ProductInstanceId::new(&"a".repeat(42))
                .unwrap()
                .as_str()
                .as_bytes()
        )
        .len(),
        3
    );
}
