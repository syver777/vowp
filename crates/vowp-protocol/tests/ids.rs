//! Id grammar and monotonicity tests. Spec: `docs/design/crates/vowp-protocol.md` §6.

use std::str::FromStr;

use pretty_assertions::assert_eq;
use vowp_protocol::prelude::*;

fn assert_invalid_id(result: Result<impl std::fmt::Debug, ProtocolError>) {
    let err = result.expect_err("expected an invalid id error");
    assert!(
        matches!(err, ProtocolError::InvalidId { .. }),
        "unexpected error: {err:?}"
    );
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn id_parse_roundtrip_all_types() {
    macro_rules! check_ulid {
        ($ty:ty) => {{
            let id = <$ty>::new();
            let text = id.to_string();
            assert_eq!(text.len(), 26, "unexpected ULID length: {text}");
            assert_eq!(text, text.to_ascii_uppercase(), "not canonical: {text}");
            assert_eq!(<$ty>::parse(&text).unwrap(), id);
            assert_eq!(<$ty>::parse(&text.to_ascii_lowercase()).unwrap(), id);
            assert_eq!(<$ty>::from_str(&text).unwrap(), id);
            assert_eq!(id.as_str(), text);
            assert_invalid_id(<$ty>::parse(&text[..25]));
            assert_invalid_id(<$ty>::parse(&format!("{}!", &text[..25])));
            assert_invalid_id(<$ty>::parse("IIIIIIIIIIIIIIIIIIIIIIIIII"));
            // Values above the 128-bit ULID maximum would be silently truncated.
            assert_invalid_id(<$ty>::parse("80000000000000000000000000"));
            assert_invalid_id(<$ty>::parse("Z0000000000000000000000000"));
            // The maximum itself is canonical and round-trips unchanged.
            let max = "7ZZZZZZZZZZZZZZZZZZZZZZZZZ";
            assert_eq!(<$ty>::parse(max).unwrap().to_string(), max);
        }};
    }

    check_ulid!(SessionId);
    check_ulid!(EventId);
    check_ulid!(RunId);
    check_ulid!(SwitchId);
    check_ulid!(InboxId);
    check_ulid!(ReleaseId);
    check_ulid!(ScheduleId);

    let capsule = CapsuleId::new();
    let capsule_text = capsule.to_string();
    assert_eq!(capsule_text.len(), 30);
    assert_eq!(CapsuleId::parse(&capsule_text).unwrap(), capsule);
    assert_eq!(CapsuleId::parse(&capsule_text.to_ascii_lowercase()).unwrap(), capsule);
    assert_invalid_id(CapsuleId::parse(&capsule_text["cap_".len()..]));
    assert_invalid_id(CapsuleId::parse(&format!("cap_{}", &capsule.ulid()[..25])));
    assert_invalid_id(CapsuleId::parse(&format!(
        "{}!",
        &capsule_text[..capsule_text.len() - 1]
    )));

    for text in ["sample-job", "a", "0", "a-b", "sample-dependency"] {
        assert_eq!(JobId::parse(text).unwrap().to_string(), text);
        assert_eq!(NodeName::parse(text).unwrap().to_string(), text);
        assert_eq!(ProjectName::parse(text).unwrap().to_string(), text);
    }

    let job = JobId::parse("sample-job").unwrap();
    assert_eq!(JobId::parse(job.as_str()).unwrap(), job);
    assert_eq!(JobId::from_str("sample-job").unwrap(), job);
    assert_eq!(<JobId as AsRef<str>>::as_ref(&job), "sample-job");
    assert_eq!(String::from(job.clone()), "sample-job");
    assert_eq!(JobId::try_from(String::from("sample-job")).unwrap(), job);

    // Serde round-trips through the bare string and validates on deserialisation.
    let json = serde_json::to_string(&job).unwrap();
    assert_eq!(json, "\"sample-job\"");
    assert_eq!(serde_json::from_str::<JobId>(&json).unwrap(), job);
    assert!(serde_json::from_str::<JobId>("\"A\"").is_err());
}

#[test]
fn slug_ids_reject_bad_input() {
    let too_long = "a".repeat(65);
    let bad = ["", "-a", "A", too_long.as_str(), "a b", "a-", "a_b", "æ-b"];

    macro_rules! check_slug {
        ($ty:ty) => {{
            for input in bad {
                assert_invalid_id(<$ty>::parse(input));
            }
            for input in ["a", "sample-job", "0", "a-b-c", "a--b"] {
                assert!(<$ty>::parse(input).is_ok(), "rejected valid slug {input:?}");
            }
            let sixty_four = "a".repeat(64);
            assert!(<$ty>::parse(&sixty_four).is_ok());
            assert_invalid_id(<$ty>::parse(&"a".repeat(65)));
        }};
    }

    check_slug!(JobId);
    check_slug!(NodeName);
    check_slug!(ProjectName);
}

#[test]
fn ulid_new_is_monotonic() {
    let mut previous = EventId::new();
    for i in 0..10_000 {
        let next = EventId::new();
        assert!(
            next.to_string() > previous.to_string(),
            "id #{i} did not sort after its predecessor: {next} <= {previous}"
        );
        previous = next;
    }
}

#[test]
fn capsule_id_prefix() {
    let id = CapsuleId::new();
    let text = id.to_string();
    assert!(text.starts_with("cap_"), "missing prefix: {text}");
    assert_eq!(id.ulid().len(), 26);
    assert_eq!(id.ulid(), &text["cap_".len()..]);
    assert_eq!(
        CapsuleId::parse(id.ulid()).unwrap_err().to_string(),
        format!("invalid id capsule: {:?}", id.ulid())
    );
}

#[test]
fn prelude_exports_foundation_types() {
    let _: Option<SessionId> = None;
    let _: Option<JobId> = None;
    let _: Option<Timestamp> = None;
    let _: Option<Executor> = None;
    let _: Option<Cost> = None;
    let _: Option<LimitKind> = None;
    let _: Option<ErrorKind> = None;
    let _: Option<Level> = None;
}
