//! Exercise bounded file IO and the boot helper's fail-closed process contract.
use std::{fs, process::Command};

#[test]
fn defaults_are_valid_and_missing_input_does_not_fall_back()
-> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_harness-network-config")).output()?;
    assert!(output.status.success());
    thorough_but_unreliable::network::Network::parse(&output.stdout)?;
    let output = Command::new(env!("CARGO_BIN_EXE_harness-network-config"))
        .arg("/no-such-harness-network-config/network.json")
        .output()?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn supplied_file_is_applied_and_invalid_file_emits_no_configuration()
-> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join(format!(
        "harness-network-config-{}.json",
        std::process::id()
    ));
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let input =
            thorough_but_unreliable::network::DEFAULT.replace("10.99.1.2/24", "192.168.40.2/24");
        fs::write(&path, input)?;
        let output = Command::new(env!("CARGO_BIN_EXE_harness-network-config"))
            .arg(&path)
            .output()?;
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout)?.contains("192.168.40.2/24"));
        for input in [b"{}".to_vec(), vec![b' '; 4097]] {
            fs::write(&path, input)?;
            let output = Command::new(env!("CARGO_BIN_EXE_harness-network-config"))
                .arg(&path)
                .output()?;
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
        }
        Ok(())
    })();
    fs::remove_file(path)?;
    result
}
