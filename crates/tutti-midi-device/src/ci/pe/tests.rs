//! Property Exchange Header Data (M2-103 §6.1, §7, §8.4 to §8.6, §11) and Mcoded7
//! (§6.1.7). Each test names the property of the spec it holds.

use std::num::NonZeroU32;

use super::mcoded7::{self, Mcoded7Error};
use super::*;
use crate::MaxSysexSize;

fn parse(text: &str) -> Result<PeHeader, PeHeaderError> {
    PeHeader::parse(text.as_bytes())
}

fn request(text: &str) -> PeRequest {
    match parse(text) {
        Ok(PeHeader::Request(request)) => request,
        other => panic!("not a request: {other:?}"),
    }
}

fn reply(text: &str) -> PeReply {
    match parse(text) {
        Ok(PeHeader::Reply(reply)) => reply,
        other => panic!("not a reply: {other:?}"),
    }
}

fn big() -> MaxSysexSize {
    MaxSysexSize::of(MaxSysexSize::MAX)
}

fn name(text: &str) -> PropertyName {
    PropertyName::new(text).unwrap()
}

// §8.6.2 Table 21: a List Resource inquiry carries offset and limit together.
#[test]
fn a_pagination_request_reads_offset_and_limit() {
    let got = request(r#"{"resource":"SongList","offset":20,"limit":3}"#);
    assert_eq!(got.resource.as_str(), "SongList");
    assert_eq!(
        got.page,
        Some(PePage {
            offset: 20,
            limit: NonZeroU32::new(3).unwrap()
        })
    );
}

// §8.6.2 Table 19: offset is an integer of at least 0, limit an integer of at
// least 1, and both are present if either is.
#[test]
fn pagination_limits_are_those_of_table_19() {
    assert!(
        request(r#"{"resource":"A","offset":0,"limit":1}"#)
            .page
            .is_some()
    );
    for bad in [
        r#"{"resource":"A","offset":-1,"limit":1}"#,
        r#"{"resource":"A","offset":0,"limit":0}"#,
        r#"{"resource":"A","offset":0.5,"limit":1}"#,
        r#"{"resource":"A","offset":"0","limit":1}"#,
        r#"{"resource":"A","offset":0}"#,
        r#"{"resource":"A","limit":1}"#,
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
}

// §7.3 Table 14, §7.4: a reply has status first; an error reply says why.
#[test]
fn reply_examples_read() {
    assert_eq!(reply(r#"{"status":200}"#), PeReply::new(PeStatus::Ok));
    let got = reply(r#"{"status":404,"message":"No such resource here."}"#);
    assert_eq!(got.status, PeStatus::NotFound);
    assert_eq!(got.message.unwrap().as_str(), "No such resource here.");
}

// §8.6.2 Table 20: totalCount is an integer of at least 0.
#[test]
fn total_count_is_an_unsigned_integer() {
    assert_eq!(
        reply(r#"{"status":200,"totalCount":128}"#).total_count,
        Some(128)
    );
    assert!(parse(r#"{"status":200,"totalCount":-1}"#).is_err());
    assert!(parse(r#"{"status":200,"totalCount":1.5}"#).is_err());
}

// §7.3 Table 14, §7.4.3: cacheTime is an integer of at least 0, in seconds.
#[test]
fn cache_time_is_a_non_negative_integer() {
    assert_eq!(
        reply(r#"{"status":200,"cacheTime":30}"#).cache_time_secs,
        Some(30)
    );
    assert_eq!(
        reply(r#"{"status":200,"cacheTime":0}"#).cache_time_secs,
        Some(0)
    );
    assert!(parse(r#"{"status":200,"cacheTime":-5}"#).is_err());
    assert!(parse(r#"{"status":200,"cacheTime":"30"}"#).is_err());
}

// §7.4.1 Table 15 and §12: the codes a header may carry, and no others.
#[test]
fn status_is_a_code_of_the_table() {
    let listed = [
        200, 201, 341, 342, 343, 400, 403, 404, 405, 406, 407, 413, 415, 445, 500, 100, 144, 408,
    ];
    for code in listed {
        let status = PeStatus::new(code).unwrap();
        assert_eq!(status.code(), code);
        assert_eq!(reply(&format!(r#"{{"status":{code}}}"#)).status, status);
    }
    for code in [0, 199, 202, 300, 344, 401, 499, 501, 999] {
        assert_eq!(PeStatus::new(code), None, "{code}");
        assert!(parse(&format!(r#"{{"status":{code}}}"#)).is_err());
    }
}

// §6.1.6 Table 12: the three standard encodings, spelled as the table does.
#[test]
fn mutual_encoding_is_one_of_the_three_standard_values() {
    for (text, want) in [
        ("ASCII", PeEncoding::Ascii),
        ("Mcoded7", PeEncoding::Mcoded7),
        ("zlib+Mcoded7", PeEncoding::ZlibMcoded7),
    ] {
        assert_eq!(want.as_str(), text);
        assert_eq!(PeEncoding::new(text), Some(want));
        let got = request(&format!(r#"{{"resource":"A","mutualEncoding":"{text}"}}"#));
        assert_eq!(got.mutual_encoding, Some(want));
    }
    for bad in ["ascii", "zlib", "Mcoded", "mcoded7", ""] {
        assert_eq!(PeEncoding::new(bad), None, "{bad}");
    }
    assert!(parse(r#"{"resource":"A","mutualEncoding":"gzip"}"#).is_err());
}

// §7.2 Table 13: resource is 1 to 36 ASCII alphanumerics; §8.4: or the X- prefix
// of a custom Resource.
#[test]
fn resource_name_limits() {
    assert!(ResourceName::new(&"a".repeat(36)).is_some());
    assert!(ResourceName::new(&"a".repeat(37)).is_none());
    assert!(ResourceName::new("").is_none());
    assert!(ResourceName::new("Channel List").is_none());
    assert!(ResourceName::new("Channel_List").is_none());
    assert!(ResourceName::new("Cha-nnel").is_none());
    assert!(ResourceName::new("Channel\u{e9}").is_none());
    assert!(ResourceName::new("X-MyResource").is_some());
    assert!(ResourceName::new("X-").is_none());
    assert!(ResourceName::new(&format!("X-{}", "a".repeat(34))).is_some());
    assert!(ResourceName::new(&format!("X-{}", "a".repeat(35))).is_none());
    assert!(parse(&format!(r#"{{"resource":"{}"}}"#, "a".repeat(37))).is_err());
    assert!(parse(r#"{"resource":""}"#).is_err());
}

// §7.2 Table 13, §8.5: resId is 1 to 36 ASCII alphanumerics or underscores.
#[test]
fn resource_id_limits() {
    assert!(ResourceId::new(&"a".repeat(36)).is_some());
    assert!(ResourceId::new(&"a".repeat(37)).is_none());
    assert!(ResourceId::new("").is_none());
    assert!(ResourceId::new("prog_1").is_some());
    assert!(ResourceId::new("prog-1").is_none());
    assert!(ResourceId::new("prog 1").is_none());
    assert!(ResourceId::new("prog\u{e9}").is_none());
    let got = request(r#"{"resource":"ProgramEdit","resId":"abc_9"}"#);
    assert_eq!(got.res_id.unwrap().as_str(), "abc_9");
}

// §11.1 Table 39: subscribeId is 1 to 8 characters of a-z, 0-9 and underscore.
#[test]
fn subscribe_id_limits() {
    assert!(SubscribeId::new("abcdefgh").is_some());
    assert!(SubscribeId::new("abcdefghi").is_none());
    assert!(SubscribeId::new("").is_none());
    assert!(SubscribeId::new("sub_328").is_some());
    assert!(SubscribeId::new("Sub328").is_none());
    assert!(SubscribeId::new("sub-1").is_none());
}

// §6.1, §7.1.1, §6.1.4: a property name is camelCase ASCII of at most 20
// characters, not only digits; a manufacturer's starts x-.
#[test]
fn property_name_limits() {
    assert!(PropertyName::new(&"a".repeat(20)).is_some());
    assert!(PropertyName::new(&"a".repeat(21)).is_none());
    assert!(PropertyName::new("").is_none());
    assert!(PropertyName::new("120").is_none());
    assert!(PropertyName::new("a120").is_some());
    assert!(PropertyName::new("my_key").is_none());
    assert!(PropertyName::new("my key").is_none());
    assert!(PropertyName::new("x-uniqueKey").is_some());
    assert!(PropertyName::new("x-").is_none());
    assert!(PropertyName::new("x-12").is_none());
    assert!(PropertyName::new(&format!("x-{}", "a".repeat(18))).is_some());
    assert!(PropertyName::new(&format!("x-{}", "a".repeat(19))).is_none());
    assert!(matches!(
        parse(r#"{"resource":"A","1":true}"#),
        Err(PeHeaderError::InvalidName(_))
    ));
}

// §7.5 Table 16: mediaType is a string of at most 75 characters.
#[test]
fn media_type_limits() {
    assert!(MediaType::new(&"a".repeat(75)).is_some());
    assert!(MediaType::new(&"a".repeat(76)).is_none());
    assert!(MediaType::new("").is_none());
    assert!(MediaType::new("caf\u{e9}").is_none());
    let got = request(r#"{"resource":"A","mediaType":"audio/midi"}"#);
    assert_eq!(got.media_type.unwrap().as_str(), "audio/midi");
}

// §7.3 Table 14: message is at most 512 bytes after escaping.
#[test]
fn message_is_at_most_512_bytes_escaped() {
    assert!(PeMessage::new(&"a".repeat(512)).is_some());
    assert!(PeMessage::new(&"a".repeat(513)).is_none());
    // A quote escapes to two bytes, a BMP character to six.
    assert!(PeMessage::new(&"\"".repeat(256)).is_some());
    assert!(PeMessage::new(&"\"".repeat(257)).is_none());
    assert!(PeMessage::new(&"\u{266a}".repeat(85)).is_some());
    assert!(PeMessage::new(&"\u{266a}".repeat(86)).is_none());
    // Outside the BMP: two UTF-16 units, twelve bytes.
    assert!(PeMessage::new(&"\u{1f3b9}".repeat(42)).is_some());
    assert!(PeMessage::new(&"\u{1f3b9}".repeat(43)).is_none());
    let long = format!(r#"{{"status":400,"message":"{}"}}"#, "a".repeat(513));
    assert!(parse(&long).is_err());
}

// §7.1.1: requests start with resource, replies with status and subscription
// messages with command.
#[test]
fn the_first_property_gives_the_shape() {
    assert!(matches!(
        parse(r#"{"resource":"A"}"#),
        Ok(PeHeader::Request(_))
    ));
    assert!(matches!(parse(r#"{"status":200}"#), Ok(PeHeader::Reply(_))));
    assert!(matches!(
        parse(r#"{"command":"end","subscribeId":"s1"}"#),
        Ok(PeHeader::Subscription(_))
    ));
    for bad in [r#"{}"#, r#"{"resId":"a","resource":"A"}"#, r#"{"x-a":1}"#] {
        assert_eq!(parse(bad), Err(PeHeaderError::FirstProperty), "{bad}");
    }
}

// §7.1.1: a header has no whitespace outside a string and is all on one line.
#[test]
fn whitespace_outside_a_string_is_refused() {
    for bad in [
        "{\"resource\": \"A\"}",
        "{ \"resource\":\"A\"}",
        "{\"resource\":\"A\"}\n",
        "{\"resource\":\"A\",\n\"resId\":\"b\"}",
        "{\"resource\":\"A\"} ",
        "{\"resource\":\"A\"\t}",
    ] {
        assert!(
            matches!(
                PeHeader::parse(bad.as_bytes()),
                Err(PeHeaderError::Malformed { .. })
            ),
            "{bad:?}"
        );
    }
    // A space inside a string is text.
    assert!(parse(r#"{"status":400,"message":"two words"}"#).is_ok());
}

// §7.1.1: a property is a number, a boolean or a string.
#[test]
fn a_property_value_is_a_number_boolean_or_string() {
    for bad in [
        r#"{"resource":"A","x-a":null}"#,
        r#"{"resource":"A","x-a":{"b":1}}"#,
        r#"{"resource":"A","x-a":[1]}"#,
        r#"{"resource":"A","x-a":'b'}"#,
        r#"{"resource":"A","x-a":01}"#,
        r#"{"resource":"A","x-a":+1}"#,
        r#"{"resource":"A","x-a":1.}"#,
        r#"{"resource":"A","x-a":.5}"#,
        r#"{"resource":"A","x-a":1e}"#,
        r#"{"resource":"A","x-a":0x7F}"#,
        r#"{"resource":"A","x-a":TRUE}"#,
        r#"{"resource":"A","x-a":NaN}"#,
        r#"{"resource":"A","x-a":1e999}"#,
    ] {
        assert!(
            matches!(parse(bad), Err(PeHeaderError::Malformed { .. })),
            "{bad}"
        );
    }
    let got = request(r#"{"resource":"A","x-i":-12,"x-d":2.5,"x-e":1e3,"x-b":false,"x-s":"t"}"#);
    assert_eq!(got.extra.get("x-i"), Some(&PeValue::Integer(-12)));
    assert_eq!(
        got.extra.get("x-d"),
        Some(&PeValue::Decimal(PeDecimal::new(2.5).unwrap()))
    );
    assert_eq!(
        got.extra.get("x-e"),
        Some(&PeValue::Decimal(PeDecimal::new(1000.0).unwrap()))
    );
    assert_eq!(got.extra.get("x-b"), Some(&PeValue::Bool(false)));
    assert_eq!(got.extra.get("x-s"), Some(&PeValue::String("t".into())));
}

#[test]
fn truncated_and_garbled_headers_are_malformed() {
    for bad in [
        r#"{"resource":"A""#,
        r#"{"resource":"A","#,
        r#"{"resource":"A",}"#,
        r#"{"resource":"A"}}"#,
        r#"{"resource":"A"}x"#,
        r#"{resource:"A"}"#,
        r#"["resource"]"#,
        r#""resource""#,
        r#"{"resource""#,
        r#"{"resource":}"#,
        r#"{"resource":"A\x"}"#,
        r#"{"resource":"A\u12"}"#,
        r#"{"resource":"A\uzzzz"}"#,
        "{\"resource\":\"A\u{1}\"}",
        "",
    ] {
        assert!(
            matches!(parse(bad), Err(PeHeaderError::Malformed { .. })),
            "{bad:?}"
        );
    }
}

// §6.1.1: Header Data is 7-bit ASCII; anything else is a JSON \u escape.
#[test]
fn a_non_ascii_byte_is_refused() {
    let mut bytes = br#"{"status":400,"message":"caf"#.to_vec();
    bytes.extend_from_slice("\u{e9}".as_bytes());
    bytes.extend_from_slice(br#""}"#);
    assert_eq!(
        PeHeader::parse(&bytes),
        Err(PeHeaderError::NotAscii { at: 28 })
    );
    assert!(matches!(
        PeHeader::parse(b"{\"resource\":\"A\x80\"}"),
        Err(PeHeaderError::NotAscii { .. })
    ));
}

// §6.1.1: characters outside 7-bit ASCII are UTF-16 units escaped with \u; a
// character past the BMP is a surrogate pair; a slash may be written either way.
#[test]
fn escapes_read_as_json_reads_them() {
    let got = reply(r#"{"status":400,"message":"Beats\u266a \ud83c\udfb9 a\/b \"q\" \\ \n"}"#);
    assert_eq!(
        got.message.unwrap().as_str(),
        "Beats\u{266a} \u{1f3b9} a/b \"q\" \\ \n"
    );
    assert_eq!(
        reply(r#"{"status":400,"message":"a/b"}"#).message,
        PeMessage::new("a/b")
    );
    for lone in [r#"\ud83c"#, r#"\udfb9"#, r#"\ud83c\u0041"#] {
        assert!(
            parse(&format!(r#"{{"status":400,"message":"{lone}"}}"#)).is_err(),
            "{lone}"
        );
    }
}

#[test]
fn the_writer_escapes_everything_outside_printable_ascii() {
    let mut reply = PeReply::new(PeStatus::BadRequest);
    reply.message = PeMessage::new("Beats\u{266a} \u{1f3b9} \"q\" \\ \n\t a/b");
    let bytes = PeHeader::Reply(reply).encode(big()).unwrap();
    assert_eq!(
        bytes,
        br#"{"status":400,"message":"Beats\u266a \ud83c\udfb9 \"q\" \\ \n\t a/b"}"#
    );
    assert!(bytes.is_ascii());
}

// §6.1.4: an x- property and a property the spec does not name are kept.
#[test]
fn unknown_properties_are_kept_in_order() {
    let got = request(r#"{"resource":"DeviceInfo","x-uniqueKey":"v","future":7}"#);
    let names: Vec<_> = got.extra.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["x-uniqueKey", "future"]);
    let bytes = PeHeader::Request(got).encode(big()).unwrap();
    assert_eq!(
        bytes,
        br#"{"resource":"DeviceInfo","x-uniqueKey":"v","future":7}"#
    );
}

#[test]
fn a_property_of_another_shape_is_not_an_extra() {
    assert_eq!(
        parse(r#"{"resource":"A","status":200}"#),
        Err(PeHeaderError::Misplaced("status".into()))
    );
    assert!(matches!(
        parse(r#"{"status":200,"offset":1,"limit":1}"#),
        Err(PeHeaderError::Misplaced(_))
    ));
    assert!(matches!(
        parse(r#"{"command":"end","subscribeId":"a","resource":"A"}"#),
        Err(PeHeaderError::Misplaced(_))
    ));
    let mut extra = PeExtra::default();
    assert!(extra.insert(name("status"), PeValue::Bool(true)).is_err());
    assert!(extra.insert(name("x-a"), PeValue::Bool(true)).is_ok());
    assert!(extra.insert(name("x-a"), PeValue::Integer(1)).is_ok());
    assert_eq!(extra.iter().count(), 1);
    assert_eq!(extra.get("x-a"), Some(&PeValue::Integer(1)));
}

#[test]
fn a_property_twice_is_refused() {
    assert_eq!(
        parse(r#"{"resource":"A","resId":"a","resId":"b"}"#),
        Err(PeHeaderError::Duplicate("resId".into()))
    );
}

// §11.1 Table 39: command is start, partial, full, notify or end; subscribeId
// is required in all but start, which the Responder answers with one.
#[test]
fn subscription_messages_read() {
    let PeHeader::Subscription(PeSubscription::Start(start)) =
        parse(r#"{"command":"start","resource":"Mode","resId":"m1"}"#).unwrap()
    else {
        panic!("not a start")
    };
    assert_eq!(start.resource.as_str(), "Mode");
    assert_eq!(start.res_id.unwrap().as_str(), "m1");
    assert_eq!(
        reply(r#"{"status":200,"subscribeId":"sub12"}"#)
            .subscribe_id
            .unwrap()
            .as_str(),
        "sub12"
    );
    for (text, want) in [
        ("partial", PeUpdateCommand::Partial),
        ("full", PeUpdateCommand::Full),
        ("notify", PeUpdateCommand::Notify),
    ] {
        let got = parse(&format!(
            r#"{{"command":"{text}","subscribeId":"sub12","mutualEncoding":"Mcoded7"}}"#
        ));
        let Ok(PeHeader::Subscription(PeSubscription::Update {
            command,
            subscribe_id,
            mutual_encoding,
            ..
        })) = got
        else {
            panic!("not an update: {got:?}")
        };
        assert_eq!(command, want);
        assert_eq!(subscribe_id.as_str(), "sub12");
        assert_eq!(mutual_encoding, Some(PeEncoding::Mcoded7));
    }
    for (text, want) in [
        ("initiator", Some(PeEndedBy::Initiator)),
        ("responder", Some(PeEndedBy::Responder)),
    ] {
        let got = parse(&format!(
            r#"{{"command":"end","subscribeId":"s1","endedBy":"{text}"}}"#
        ));
        let Ok(PeHeader::Subscription(PeSubscription::End { ended_by, .. })) = got else {
            panic!("not an end: {got:?}")
        };
        assert_eq!(ended_by, want);
    }
    for bad in [
        r#"{"command":"full"}"#,
        r#"{"command":"end"}"#,
        r#"{"command":"start"}"#,
        r#"{"command":"pause","subscribeId":"s1"}"#,
        r#"{"command":"end","subscribeId":"s1","endedBy":"both"}"#,
        r#"{"command":"end","subscribeId":"TooLongId9"}"#,
        r#"{"command":1}"#,
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn the_writer_puts_the_required_property_first() {
    let mut start = PeRequest::new(ResourceName::new("Mode").unwrap());
    start.res_id = ResourceId::new("m1");
    assert_eq!(
        PeHeader::Subscription(PeSubscription::Start(start))
            .encode(big())
            .unwrap(),
        br#"{"command":"start","resource":"Mode","resId":"m1"}"#
    );
    let mut reply = PeReply::new(PeStatus::Ok);
    reply.total_count = Some(128);
    assert_eq!(
        PeHeader::Reply(reply).encode(big()).unwrap(),
        br#"{"status":200,"totalCount":128}"#
    );
}

fn everything() -> Vec<PeHeader> {
    let mut full = PeRequest::new(ResourceName::new("X-Custom").unwrap());
    full.res_id = ResourceId::new("a_1");
    full.mutual_encoding = Some(PeEncoding::ZlibMcoded7);
    full.flow_control = true;
    full.page = Some(PePage {
        offset: 4_000_000_000,
        limit: NonZeroU32::new(u32::MAX).unwrap(),
    });
    full.set_partial = true;
    full.media_type = MediaType::new("application/x-thing; v=1");
    full.extra
        .insert(
            name("x-text"),
            PeValue::String("q\"\\/\u{e9}\u{1f3b9}\u{1}".into()),
        )
        .unwrap();
    full.extra
        .insert(name("x-int"), PeValue::Integer(i64::MIN))
        .unwrap();
    full.extra
        .insert(
            name("x-dec"),
            PeValue::Decimal(PeDecimal::new(1.0).unwrap()),
        )
        .unwrap();
    full.extra
        .insert(
            name("x-big"),
            PeValue::Decimal(PeDecimal::new(1.5e300).unwrap()),
        )
        .unwrap();
    full.extra
        .insert(name("x-bool"), PeValue::Bool(true))
        .unwrap();

    let mut answer = PeReply::new(PeStatus::Terminate);
    answer.message = PeMessage::new("hint: \u{266a}");
    answer.mutual_encoding = Some(PeEncoding::Mcoded7);
    answer.cache_time_secs = Some(u32::MAX);
    answer.total_count = Some(0);
    answer.subscribe_id = SubscribeId::new("s_1");
    answer.media_type = MediaType::new("audio/midi");
    answer
        .extra
        .insert(name("future"), PeValue::Bool(false))
        .unwrap();

    vec![
        PeHeader::Request(PeRequest::new(ResourceName::new("ResourceList").unwrap())),
        PeHeader::Request(full.clone()),
        PeHeader::Reply(PeReply::new(PeStatus::Ok)),
        PeHeader::Reply(answer),
        PeHeader::Subscription(PeSubscription::Start(full)),
        PeHeader::Subscription(PeSubscription::Update {
            command: PeUpdateCommand::Full,
            subscribe_id: SubscribeId::new("sub1").unwrap(),
            mutual_encoding: Some(PeEncoding::Ascii),
            media_type: MediaType::new("x/y"),
            extra: PeExtra::default(),
        }),
        PeHeader::Subscription(PeSubscription::End {
            subscribe_id: SubscribeId::new("sub1").unwrap(),
            ended_by: Some(PeEndedBy::Responder),
            extra: PeExtra::default(),
        }),
    ]
}

#[test]
fn what_the_writer_writes_the_reader_reads() {
    for header in everything() {
        let bytes = header.encode(big()).unwrap();
        assert!(bytes.is_ascii());
        assert_eq!(PeHeader::parse(&bytes), Ok(header));
    }
}

// M2-101 §8.3.2: a header has no Property Data to chunk, so the message holds it
// whole: 24 bytes of message around the header, and at most 16383 of it.
#[test]
fn a_header_is_held_to_the_receivable_sysex_size() {
    let with = |len: usize| {
        let mut request = PeRequest::new(ResourceName::new("A").unwrap());
        request
            .extra
            .insert(name("x-a"), PeValue::String("a".repeat(len)))
            .unwrap();
        PeHeader::Request(request)
    };
    let base = with(0).encode(big()).unwrap().len();
    let small = MaxSysexSize::of(128);
    let fits = with(128 - 24 - base).encode(small).unwrap();
    assert_eq!(fits.len(), 104);
    assert_eq!(
        with(128 - 24 - base + 1).encode(small),
        Err(PeHeaderTooLong {
            len: 105,
            limit: 104
        })
    );
    // The 14-bit length is the other bound.
    let top = with(PE_MAX_HEADER_LEN - base).encode(big()).unwrap();
    assert_eq!(top.len(), PE_MAX_HEADER_LEN);
    assert_eq!(
        with(PE_MAX_HEADER_LEN - base + 1).encode(big()),
        Err(PeHeaderTooLong {
            len: PE_MAX_HEADER_LEN + 1,
            limit: PE_MAX_HEADER_LEN
        })
    );
    assert!(PeHeader::parse(&top).is_ok());
    let mut over = top;
    over.push(b' ');
    assert_eq!(
        PeHeader::parse(&over),
        Err(PeHeaderError::TooLong {
            len: PE_MAX_HEADER_LEN + 1
        })
    );
}

// Mcoded7 (§6.1.7). The group layout, from the description there: seven data
// bytes become a sign byte then their low seven bits; the sign byte holds the
// data bytes' top bits, the first at bit 6 down to the seventh at bit 0.

#[test]
fn mcoded7_known_vectors() {
    // Three bytes, a short group: tops 1,0,1 sit at bits 6,5,4, so the sign byte
    // is 0b0101_0000; the lows are 0x00, 0x01, 0x7F.
    assert_eq!(
        mcoded7::encode_vec(&[0x80, 0x01, 0xFF]),
        [0x50, 0x00, 0x01, 0x7F]
    );
    // Seven bytes with tops 0,1,0,1,0,1,0: sign 0b0010_1010; every low is 0.
    assert_eq!(
        mcoded7::encode_vec(&[0x00, 0x80, 0x00, 0x80, 0x00, 0x80, 0x00]),
        [0x2A, 0, 0, 0, 0, 0, 0, 0]
    );
    // Seven bytes all 0xFF: sign 0x7F, lows 0x7F.
    assert_eq!(mcoded7::encode_vec(&[0xFF; 7]), [0x7F; 8]);
    // Eight bytes: a full group, then a group of one: sign bit 6 only.
    assert_eq!(
        mcoded7::encode_vec(&[0x7F, 0, 0, 0, 0, 0, 0, 0x81]),
        [0, 0x7F, 0, 0, 0, 0, 0, 0, 0x40, 0x01]
    );
    // One byte: the sign byte, then the low bits.
    assert_eq!(mcoded7::encode_vec(&[0x92]), [0x40, 0x12]);
    assert_eq!(mcoded7::encode_vec(&[]), Vec::<u8>::new());
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(37) | 0x80 & (i as u8 * 11))
        .collect()
}

#[test]
fn mcoded7_round_trips_at_the_group_edges() {
    for len in [0, 1, 2, 6, 7, 8, 13, 14, 15, 21] {
        let data = pattern(len);
        let wire = mcoded7::encode_vec(&data);
        assert_eq!(wire.len(), mcoded7::encoded_len(len), "{len}");
        assert!(wire.iter().all(|b| b & 0x80 == 0), "{len}");
        assert_eq!(mcoded7::decoded_len(wire.len()), Some(len), "{len}");
        assert_eq!(
            mcoded7::decode_vec(&wire).as_deref(),
            Ok(&data[..]),
            "{len}"
        );
    }
    // Every byte value, at each place of a group.
    let all: Vec<u8> = (0..=255).collect();
    assert_eq!(
        mcoded7::decode_vec(&mcoded7::encode_vec(&all)).as_deref(),
        Ok(&all[..])
    );
}

#[test]
fn mcoded7_lengths_follow_the_group_layout() {
    for (data, wire) in [(0, 0), (1, 2), (6, 7), (7, 8), (8, 10), (14, 16), (15, 18)] {
        assert_eq!(mcoded7::encoded_len(data), wire);
        assert_eq!(mcoded7::decoded_len(wire), Some(data));
    }
    // A group has a sign byte and then data, so no data encodes to 1 mod 8 bytes.
    for wire in [1, 9, 17] {
        assert_eq!(mcoded7::decoded_len(wire), None);
    }
}

#[test]
fn mcoded7_writes_into_the_callers_buffer() {
    let mut out = [0xEE; 8];
    assert_eq!(mcoded7::encode(&[0x80, 0x01, 0xFF], &mut out), Ok(4));
    assert_eq!(out, [0x50, 0x00, 0x01, 0x7F, 0xEE, 0xEE, 0xEE, 0xEE]);
    let mut data = [0xEE; 5];
    assert_eq!(mcoded7::decode(&out[..4], &mut data), Ok(3));
    assert_eq!(data, [0x80, 0x01, 0xFF, 0xEE, 0xEE]);
    assert_eq!(
        mcoded7::encode(&[1, 2, 3], &mut out[..3]),
        Err(Mcoded7Error::OutputTooShort { need: 4, have: 3 })
    );
    assert_eq!(
        mcoded7::decode(&out[..4], &mut data[..2]),
        Err(Mcoded7Error::OutputTooShort { need: 3, have: 2 })
    );
}

#[test]
fn mcoded7_refuses_a_byte_with_the_top_bit_set_and_a_cut_group() {
    assert_eq!(
        mcoded7::decode_vec(&[0x00, 0x80]),
        Err(Mcoded7Error::NotSevenBit { at: 1 })
    );
    assert_eq!(mcoded7::decode_vec(&[0x00]), Err(Mcoded7Error::Truncated));
    assert_eq!(
        mcoded7::decode_vec(&[0x7F; 9]),
        Err(Mcoded7Error::Truncated)
    );
}

// The unused sign bits of a short group are not data: the decoder leaves them.
#[test]
fn mcoded7_ignores_the_unused_sign_bits_of_a_short_group() {
    assert_eq!(
        mcoded7::decode_vec(&[0x50 | 0x0F, 0x00, 0x01, 0x7F]),
        Ok(vec![0x80, 0x01, 0xFF])
    );
}

proptest::proptest! {
    #[test]
    fn mcoded7_round_trips_any_bytes(data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..200)) {
        let wire = mcoded7::encode_vec(&data);
        proptest::prop_assert!(wire.iter().all(|b| b & 0x80 == 0));
        proptest::prop_assert_eq!(mcoded7::decode_vec(&wire), Ok(data));
    }
}
