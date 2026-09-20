use super::*;
use proptest::prelude::*;

fn fixture() -> std::result::Result<serde_json::Value, serde_json::Error> {
    serde_json::from_str(include_str!("../../../tests/fixtures/broker-config.json"))
}

proptest! {
    #[test]
    fn command_deadline_must_fit_strictly_inside_watchdog(command in 0u32..10000, watchdog in 0u32..10000) {
        let mut raw = fixture()?;
        let object = raw.as_object_mut().ok_or_else(|| TestCaseError::fail("fixture is not object"))?;
        object.insert("command_timeout_ms".into(), command.into());
        object.insert("adapter_timeout_ms".into(), watchdog.into());
        prop_assert_eq!(Config::from_slice(&serde_json::to_vec(&raw)?).is_ok(), command > 0 && command < watchdog);
    }

    #[test]
    fn environment_preserves_values_and_rejects_repeated_keys(value in "[^\\x00]{0,100}") {
        let mut raw = fixture()?;
        let object = raw.as_object_mut().ok_or_else(|| TestCaseError::fail("fixture is not object"))?;
        object.insert("environment".into(), serde_json::json!([["EXPERIMENT", value]]));
        prop_assert!(Config::from_slice(&serde_json::to_vec(&raw)?).is_ok());
        let object = raw.as_object_mut().ok_or_else(|| TestCaseError::fail("fixture is not object"))?;
        object.insert("environment".into(), serde_json::json!([["EXPERIMENT", value], ["EXPERIMENT", "other"]]));
        prop_assert!(Config::from_slice(&serde_json::to_vec(&raw)?).is_err());
    }
}

#[test]
fn rejects_ambiguous_or_unbounded_configuration()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let original = include_str!("../../../tests/fixtures/broker-config.json");
    assert!(Config::from_slice(original.as_bytes()).is_ok());
    for bad in [
        original.replacen("{", "{\"version\":1,", 1),
        original.replacen("{", "{\"unexpected\":1,", 1),
        original.replace("127.0.0.1:18080", "0.0.0.0:18080"),
        original.replace("127.0.0.1:18080", "127.0.0.1:0"),
        format!("{original} {{}}"),
        "[]".into(),
        " ".repeat(CONFIG_BYTES + 1),
    ] {
        assert!(Config::from_slice(bad.as_bytes()).is_err());
    }
    Ok(())
}
