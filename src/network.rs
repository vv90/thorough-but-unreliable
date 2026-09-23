//! Pure validation of the isolated, two-interface boot network contract.
use serde::{Deserialize, Serialize};
use std::{
    net::Ipv4Addr,
    panic::{AssertUnwindSafe, catch_unwind},
};

pub const MAX_BYTES: usize = 4096;
pub const DEFAULT: &str = r#"{"version":1,"inference":{"mac":"52:54:00:99:01:02","address":"10.99.1.2/24"},"command":{"mac":"52:54:00:99:02:01","address":"10.99.2.1/30"}}"#;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetwork {
    version: u32,
    inference: RawInterface,
    command: RawInterface,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInterface {
    mac: String,
    address: String,
}

/// Only constructible through validation; serialization produces canonical values.
#[derive(Debug, Serialize)]
pub struct Network {
    version: u32,
    inference: Interface,
    command: Interface,
}
#[derive(Debug, Serialize)]
struct Interface {
    mac: String,
    address: String,
    #[serde(skip)]
    subnet: (u32, u32),
}

impl Network {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        match catch_unwind(AssertUnwindSafe(|| parse(bytes))) {
            Ok(result) => result,
            Err(payload) => {
                std::mem::forget(payload);
                Err("network configuration dependency panicked".into())
            }
        }
    }
}

fn parse(bytes: &[u8]) -> Result<Network, String> {
    if bytes.len() > MAX_BYTES {
        return Err("network.json exceeds 4096 bytes".into());
    }
    // Reject positional struct arrays as well as unknown and duplicate fields.
    let shape: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if !shape.is_object()
        || !shape.get("inference").is_some_and(|v| v.is_object())
        || !shape.get("command").is_some_and(|v| v.is_object())
    {
        return Err("network configuration and interfaces must be objects".into());
    }
    let raw: RawNetwork = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if raw.version != 1 {
        return Err("unsupported network version".into());
    }
    let inference = interface(raw.inference)?;
    let command = interface(raw.command)?;
    if inference.mac == command.mac {
        return Err("interface MAC addresses must differ".into());
    }
    if inference.subnet.0 <= command.subnet.1 && command.subnet.0 <= inference.subnet.1 {
        return Err("interface subnets must not overlap".into());
    }
    Ok(Network {
        version: 1,
        inference,
        command,
    })
}

fn interface(raw: RawInterface) -> Result<Interface, String> {
    let mut octets = Vec::new();
    for part in raw.mac.split(':') {
        if part.len() != 2 || !part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("MAC must contain six hexadecimal octets".into());
        }
        octets.push(u8::from_str_radix(part, 16).map_err(|_| "invalid MAC octet")?);
    }
    if octets.len() != 6
        || octets.first().is_none_or(|v| v & 1 != 0)
        || octets.iter().all(|v| *v == 0)
    {
        return Err("MAC must be a nonzero unicast address".into());
    }
    let (ip, prefix) = raw
        .address
        .split_once('/')
        .ok_or("address requires IPv4/prefix")?;
    let ip: Ipv4Addr = ip.parse().map_err(|_| "invalid IPv4 address")?;
    let prefix: u32 = prefix.parse().map_err(|_| "invalid IPv4 prefix")?;
    if !(1..=30).contains(&prefix) {
        return Err("IPv4 prefix must be between 1 and 30".into());
    }
    let mask = u32::MAX
        .checked_shl(32 - prefix)
        .ok_or("invalid prefix shift")?;
    let value = u32::from(ip);
    let start = value & mask;
    let end = start | !mask;
    // Keep this increment to ordinary static unicast networks.
    if value == start
        || value == end
        || ip.is_loopback()
        || ip.is_link_local()
        || value < 0x01000000
        || value >= 0xe0000000
    {
        return Err("address must be a usable static unicast host address".into());
    }
    Ok(Interface {
        mac: raw.mac.to_ascii_lowercase(),
        address: format!("{ip}/{prefix}"),
        subnet: (start, end),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn distinct_private_subnets_round_trip(a in 1u8..=254, b in 1u8..=254) {
            let input = DEFAULT.replace("10.99.1.2/24", &format!("10.1.0.{a}/24"))
                .replace("10.99.2.1/30", &format!("10.2.0.{b}/24"));
            let network = Network::parse(input.as_bytes()).map_err(TestCaseError::fail)?;
            let encoded = serde_json::to_vec(&network).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let again = Network::parse(&encoded).map_err(TestCaseError::fail)?;
            prop_assert_eq!(serde_json::to_vec(&again).map_err(|e| TestCaseError::fail(e.to_string()))?, encoded);
        }
        #[test]
        fn overlapping_subnets_rejected(host in 1u8..=254) {
            let input = DEFAULT.replace("10.99.2.1/30", &format!("10.99.1.{host}/24"));
            prop_assert!(Network::parse(input.as_bytes()).is_err());
        }
        #[test]
        fn network_and_broadcast_rejected(third in any::<u8>(), host in prop_oneof![Just(0), Just(255)]) {
            let input = DEFAULT.replace("10.99.1.2/24", &format!("10.1.{third}.{host}/24"));
            prop_assert!(Network::parse(input.as_bytes()).is_err());
        }
    }
    #[test]
    fn invalid_contracts_fail_closed() {
        for input in [
            DEFAULT.replace("\"version\":1", "\"version\":1,\"version\":1"),
            DEFAULT.replace("\"version\":1", "\"version\":2"),
            DEFAULT.replace("\"version\":1", "\"version\":1,\"gateway\":\"10.99.1.1\""),
            DEFAULT.replace("52:54:00:99:02:01", "52:54:00:99:01:02"),
            DEFAULT.replace("52:54", "53:54"),
            DEFAULT.replace("10.99.1.2/24", "10.99.1.2/24\nInjected=yes"),
            " ".repeat(MAX_BYTES + 1),
        ] {
            assert!(Network::parse(input.as_bytes()).is_err());
        }
        assert!(Network::parse(DEFAULT.as_bytes()).is_ok());
    }
}
