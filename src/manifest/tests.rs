use super::{MAX_MANIFEST_BYTES, Manifest, ManifestError};
use proptest::prelude::*;
use serde_json::{Value, json};
use std::{
    io::{Cursor, Read},
    time::Duration,
};

type TestResult = proptest::test_runner::TestCaseResult;

fn fixture() -> Value {
    json!({
        "version": 1, "run_id": "smoke",
        "system_prompt": "Use execute_target_command to work on the task, then submit your answer.",
        "task": "Run printf 'harness trial\\n' in the target and submit its output.",
        "max_model_turns": 8,
        "inference": {
            "completion_url": "http://10.99.1.1:11434/v1/chat/completions",
            "model": "qwen3:latest", "max_tokens": 1024,
            "connect_timeout_ms": 5000, "request_timeout_ms": 120000,
            "max_request_bytes": 1048576, "max_response_bytes": 1048576
        },
        "command": {
            "command_url": "http://10.99.2.2:8080/v1/command",
            "connect_timeout_ms": 5000, "request_timeout_ms": 60000,
            "max_request_bytes": 65536, "max_response_bytes": 262144
        }
    })
}

fn set(value: &mut Value, path: &str, replacement: Value) -> proptest::test_runner::TestCaseResult {
    *value
        .pointer_mut(path)
        .ok_or_else(|| TestCaseError::fail("missing fixture path"))? = replacement;
    Ok(())
}

fn all_agree(bytes: &[u8], valid: bool) -> proptest::test_runner::TestCaseResult {
    let parsed = Manifest::from_slice(bytes);
    let read = Manifest::from_reader(bytes);
    let deserialized = serde_json::from_slice::<Manifest>(bytes);
    prop_assert_eq!(parsed.is_ok(), valid);
    prop_assert_eq!(read.is_ok(), valid);
    prop_assert_eq!(deserialized.is_ok(), valid);
    if valid {
        prop_assert_eq!(parsed?, read?);
        prop_assert_eq!(Manifest::from_slice(bytes)?, deserialized?);
    }
    Ok(())
}

proptest! {
    #[test]
    fn valid_settings_preserve_text_numbers_and_endpoint_roles(
        suffix in any::<String>(), turns in 1..=u32::MAX, tokens in 1..=u32::MAX,
        a in 1..=u32::MAX, b in 1..=u32::MAX,
        request_bytes in 1..=u32::MAX, response_bytes in 1..=u32::MAX,
        port in 1..=u16::MAX, tls in any::<bool>(),
    ) {
        let text = format!(" \nλ{suffix}\t");
        let mut value = fixture();
        for path in ["/run_id", "/system_prompt", "/task", "/inference/model"] {
            set(&mut value, path, json!(text))?;
        }
        set(&mut value, "/max_model_turns", json!(turns))?;
        set(&mut value, "/inference/max_tokens", json!(tokens))?;
        let scheme = if tls { "https" } else { "http" };
        let inference_url = format!("{scheme}://127.0.0.1:{port}/v1/chat/completions");
        let command_url = format!("{scheme}://127.0.0.1:{port}/v1/command");
        set(&mut value, "/inference/completion_url", json!(inference_url))?;
        set(&mut value, "/command/command_url", json!(command_url))?;
        for prefix in ["inference", "command"] {
            for (field, number) in [("connect_timeout_ms", a.min(b)), ("request_timeout_ms", a.max(b)),
                ("max_request_bytes", request_bytes), ("max_response_bytes", response_bytes)] {
                set(&mut value, &format!("/{prefix}/{field}"), json!(number))?;
            }
        }
        let bytes = serde_json::to_vec(&value)?;
        all_agree(&bytes, true)?;
        let manifest = Manifest::from_slice(&bytes)?;
        for actual in [manifest.run_id(), manifest.system_prompt(), manifest.task(), manifest.inference().model()] {
            prop_assert_eq!(actual, &text);
        }
        prop_assert_eq!(manifest.version(), 1);
        prop_assert_eq!(manifest.max_model_turns().get(), turns);
        prop_assert_eq!(manifest.inference().max_tokens().get(), tokens);
        // The URL parser removes default ports; compare the interpreted ports.
        prop_assert_eq!(manifest.inference().completion_url().port_or_known_default(), Some(port));
        prop_assert_eq!(manifest.command().command_url().port_or_known_default(), Some(port));
        prop_assert_eq!(manifest.inference().completion_url().path(), "/v1/chat/completions");
        prop_assert_eq!(manifest.command().command_url().path(), "/v1/command");
        for limits in [manifest.inference().transport(), manifest.command().transport()] {
            prop_assert_eq!(limits.connect_timeout(), Duration::from_millis(u64::from(a.min(b))));
            prop_assert_eq!(limits.request_timeout(), Duration::from_millis(u64::from(a.max(b))));
            prop_assert_eq!(limits.max_request_bytes().get(), usize::try_from(request_bytes)?);
            prop_assert_eq!(limits.max_response_bytes().get(), usize::try_from(response_bytes)?);
        }
    }

    #[test]
    fn every_entry_point_enforces_text_semantics(text in prop_oneof![any::<String>(), Just(String::new()), Just(" \n\t".into())]) {
        for path in ["/run_id", "/system_prompt", "/task", "/inference/model"] {
            let mut value = fixture();
            set(&mut value, path, json!(text))?;
            let valid = if path == "/run_id" { !text.is_empty() } else { !text.trim().is_empty() };
            all_agree(&serde_json::to_vec(&value)?, valid)?;
        }
    }

    #[test]
    fn numeric_domains_cannot_be_bypassed(n in prop_oneof![any::<u64>(), 0..=u64::from(u32::MAX), Just(u64::from(u32::MAX) + 1), Just(0), Just(1)]) {
        for path in ["/version", "/max_model_turns", "/inference/max_tokens",
            "/inference/max_request_bytes", "/inference/max_response_bytes",
            "/command/max_request_bytes", "/command/max_response_bytes"] {
            let mut value = fixture();
            set(&mut value, path, json!(n))?;
            let valid = if path == "/version" { n == 1 }
                else if path.ends_with("bytes") { n > 0 && isize::try_from(n).is_ok() }
                else { n > 0 && u32::try_from(n).is_ok() };
            all_agree(&serde_json::to_vec(&value)?, valid)?;
        }
    }

    #[test]
    fn timeout_order_and_range_are_enforced(
        connect in prop_oneof![0..=u64::from(u32::MAX), any::<u64>()],
        request in prop_oneof![0..=u64::from(u32::MAX), any::<u64>()],
    ) {
        for prefix in ["inference", "command"] {
            let mut value = fixture();
            set(&mut value, &format!("/{prefix}/connect_timeout_ms"), json!(connect))?;
            set(&mut value, &format!("/{prefix}/request_timeout_ms"), json!(request))?;
            all_agree(&serde_json::to_vec(&value)?, connect > 0 && connect <= request && u32::try_from(request).is_ok())?;
        }
    }

    #[test]
    fn arbitrary_bounded_input_has_consistent_acceptance(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
        let valid = Manifest::from_slice(&bytes).is_ok();
        all_agree(&bytes, valid)?;
    }

    #[test]
    fn reader_chunking_does_not_change_the_manifest(chunk_size in 1usize..128) {
        struct Chunked<'a> { remaining: &'a [u8], chunk_size: usize, interrupt: bool }
        impl Read for Chunked<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.interrupt {
                    self.interrupt = false;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let mut limited = self.remaining.take(u64::try_from(self.chunk_size).map_err(std::io::Error::other)?);
                let count = limited.read(buffer)?;
                self.remaining = self.remaining.get(count..).ok_or_else(|| std::io::Error::other("invalid read count"))?;
                self.interrupt = true;
                Ok(count)
            }
        }
        let bytes = serde_json::to_vec(&fixture())?;
        let reader = Chunked { remaining: &bytes, chunk_size, interrupt: true };
        prop_assert_eq!(Manifest::from_reader(reader)?, Manifest::from_slice(&bytes)?);
    }
}

#[test]
fn every_object_rejects_missing_extra_duplicate_fields_arrays_and_wrong_types() -> TestResult {
    let value = fixture();
    for path in ["", "/inference", "/command"] {
        let object = value
            .pointer(path)
            .and_then(Value::as_object)
            .ok_or_else(|| TestCaseError::fail("missing object"))?;
        let encoded = serde_json::to_string(object)?;
        let mut extra = object.clone();
        extra.insert("extra".into(), json!(true));
        let replacements = [
            json!(extra),
            json!(object.values().collect::<Vec<_>>()),
            Value::Null,
        ];
        for replacement in replacements {
            let mut mutated = value.clone();
            set(&mut mutated, path, replacement)?;
            all_agree(&serde_json::to_vec(&mutated)?, false)?;
        }
        for (key, original) in object {
            let mut missing = object.clone();
            missing.remove(key);
            let mut mutated = value.clone();
            set(&mut mutated, path, json!(missing))?;
            all_agree(&serde_json::to_vec(&mutated)?, false)?;
            let duplicate = format!(
                "{{{}:{},{}",
                serde_json::to_string(key)?,
                original,
                encoded
                    .strip_prefix('{')
                    .ok_or_else(|| TestCaseError::fail("invalid object"))?
            );
            let bytes = serde_json::to_string(&value)?.replacen(&encoded, &duplicate, 1);
            all_agree(bytes.as_bytes(), false)?;
            for wrong in [Value::Null, json!(true), json!([])] {
                let mut mutated = value.clone();
                set(&mut mutated, &format!("{path}/{key}"), wrong)?;
                all_agree(&serde_json::to_vec(&mutated)?, false)?;
            }
        }
    }
    let trailing = format!("{} {{}}", serde_json::to_string(&value)?);
    all_agree(trailing.as_bytes(), false)?;
    all_agree(br#"{"version":1,"run_id":"smoke"}"#, false)?;
    Ok(())
}

#[test]
fn endpoint_constraints_match_the_client_roles() -> TestResult {
    for (field, path) in [
        ("/inference/completion_url", "/v1/chat/completions"),
        ("/command/command_url", "/v1/command"),
    ] {
        for url in [
            "garbage".into(),
            path.into(),
            format!("file://host{path}"),
            format!("http://user@host{path}"),
            format!("http://user:secret@host{path}"),
            format!("http://host{path}?"),
            format!("http://host{path}#"),
            format!("http://host{path}/"),
            "http://host/wrong".into(),
        ] {
            let mut value = fixture();
            set(&mut value, field, json!(url))?;
            all_agree(&serde_json::to_vec(&value)?, false)?;
        }
    }
    Ok(())
}

#[test]
fn numeric_json_types_and_domain_boundaries_are_strict() -> TestResult {
    for path in [
        "/version",
        "/max_model_turns",
        "/inference/max_tokens",
        "/inference/connect_timeout_ms",
        "/inference/request_timeout_ms",
        "/command/connect_timeout_ms",
        "/command/request_timeout_ms",
        "/inference/max_request_bytes",
        "/inference/max_response_bytes",
        "/command/max_request_bytes",
        "/command/max_response_bytes",
    ] {
        for invalid in [json!(-1), json!(1.0), json!("1"), json!(0), json!(u64::MAX)] {
            let mut value = fixture();
            set(&mut value, path, invalid)?;
            all_agree(&serde_json::to_vec(&value)?, false)?;
        }
    }
    for prefix in ["inference", "command"] {
        let mut value = fixture();
        for field in ["connect_timeout_ms", "request_timeout_ms"] {
            set(&mut value, &format!("/{prefix}/{field}"), json!(u32::MAX))?;
        }
        for field in ["max_request_bytes", "max_response_bytes"] {
            set(&mut value, &format!("/{prefix}/{field}"), json!(isize::MAX))?;
        }
        all_agree(&serde_json::to_vec(&value)?, true)?;
    }
    Ok(())
}

#[test]
fn manifest_size_limit_is_exact_and_reader_stops_on_overflow() -> TestResult {
    let mut bytes = serde_json::to_vec(&fixture())?;
    bytes.resize(MAX_MANIFEST_BYTES, b' ');
    all_agree(&bytes, true)?;
    bytes.push(b' ');
    assert!(matches!(
        Manifest::from_slice(&bytes),
        Err(ManifestError::TooLarge)
    ));
    assert!(matches!(
        Manifest::from_reader(bytes.as_slice()),
        Err(ManifestError::TooLarge)
    ));
    // Direct Serde has no byte-stream boundary; its schema remains valid here.
    assert!(serde_json::from_slice::<Manifest>(&bytes).is_ok());
    let mut infinite = std::io::repeat(b' ');
    assert!(matches!(
        Manifest::from_reader(&mut infinite),
        Err(ManifestError::TooLarge)
    ));
    Ok(())
}

#[test]
fn semantic_failures_retain_typed_diagnostics() -> TestResult {
    let mut value = fixture();
    set(&mut value, "/version", json!(2))?;
    assert!(matches!(
        Manifest::from_slice(&serde_json::to_vec(&value)?),
        Err(ManifestError::UnsupportedVersion(2))
    ));
    set(&mut value, "/version", json!(1))?;
    set(&mut value, "/run_id", json!(""))?;
    assert!(matches!(
        Manifest::from_slice(&serde_json::to_vec(&value)?),
        Err(ManifestError::EmptyRunId)
    ));
    set(&mut value, "/run_id", json!("smoke"))?;
    set(&mut value, "/task", json!(" \n"))?;
    assert!(matches!(
        Manifest::from_slice(&serde_json::to_vec(&value)?),
        Err(ManifestError::InvalidField { field: "task", .. })
    ));
    Ok(())
}

#[test]
fn reader_failures_remain_explicit() -> TestResult {
    struct FailedReader;
    impl Read for FailedReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected read failure"))
        }
    }
    struct UnwindingReader;
    impl Read for UnwindingReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            // Fault injection only, never a production error-handling path.
            std::panic::resume_unwind(Box::new("injected reader unwind"))
        }
    }
    struct InvalidReader;
    impl Read for InvalidReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Ok(usize::MAX)
        }
    }
    assert!(matches!(
        Manifest::from_reader(FailedReader),
        Err(ManifestError::Read(_))
    ));
    assert!(matches!(
        Manifest::from_reader(UnwindingReader),
        Err(ManifestError::DependencyPanicked)
    ));
    assert!(matches!(
        Manifest::from_reader(InvalidReader),
        Err(ManifestError::Read(_))
    ));
    let bytes = serde_json::to_vec(&fixture())?;
    assert_eq!(
        Manifest::from_reader(Cursor::new(&bytes))?,
        Manifest::from_slice(&bytes)?
    );
    Ok(())
}
