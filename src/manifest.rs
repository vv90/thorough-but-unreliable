use serde::{Deserialize, Deserializer, de};
use std::{
    fmt,
    fs::File,
    io::{BufReader, Read},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
};

/// A validated version-1 manifest. Every construction/deserialization path checks
/// the schema; its fields cannot be changed after validation.
///
/// ```compile_fail
/// use thorough_but_unreliable::manifest::Manifest;
/// let invalid = Manifest { run_id: String::new() };
/// ```
/// Direct Serde deserialization performs the same validation:
///
/// ```
/// use thorough_but_unreliable::manifest::Manifest;
/// assert!(serde_json::from_str::<Manifest>(r#"{"version":2,"run_id":""}"#).is_err());
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct Manifest {
    run_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    version: u64,
    run_id: String,
}

/// Require an actual object, preserving duplicate-field checks in Serde.
fn deserialize_raw<'de, D: Deserializer<'de>>(deserializer: D) -> Result<RawManifest, D::Error> {
    struct ObjectVisitor;
    impl<'de> de::Visitor<'de> for ObjectVisitor {
        type Value = RawManifest;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a manifest object")
        }
        fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            RawManifest::deserialize(de::value::MapAccessDeserializer::new(map))
        }
    }
    deserializer.deserialize_map(ObjectVisitor)
}

impl TryFrom<RawManifest> for Manifest {
    type Error = ManifestError;
    fn try_from(raw: RawManifest) -> Result<Self, Self::Error> {
        if raw.version != 1 {
            return Err(ManifestError::UnsupportedVersion(raw.version));
        }
        if raw.run_id.is_empty() {
            return Err(ManifestError::EmptyRunId);
        }
        Ok(Self { run_id: raw.run_id })
    }
}

impl<'de> Deserialize<'de> for Manifest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(deserialize_raw(deserializer)?).map_err(de::Error::custom)
    }
}

impl Manifest {
    pub const fn version(&self) -> u64 {
        1
    }
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Pure parsing and semantic validation, retaining typed validation failures.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, ManifestError> {
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        let raw = deserialize_raw(&mut deserializer).map_err(ManifestError::Parse)?;
        deserializer.end().map_err(ManifestError::Parse)?;
        Self::try_from(raw)
    }

    pub fn from_path(path: &Path) -> Result<Self, ManifestError> {
        let file = File::open(path).map_err(ManifestError::Read)?;
        Self::from_reader(BufReader::new(file))
    }

    /// Thin IO layer; custom readers may unwind, so contain that boundary.
    pub fn from_reader(mut reader: impl Read) -> Result<Self, ManifestError> {
        let result = catch_unwind(AssertUnwindSafe(move || {
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .map_err(ManifestError::Read)?;
            Self::from_slice(&bytes)
        }));
        match result {
            Ok(result) => result,
            Err(payload) => {
                std::mem::forget(payload);
                Err(ManifestError::DependencyPanicked)
            }
        }
    }
}

#[derive(Debug)]
pub enum ManifestError {
    Read(std::io::Error),
    Parse(serde_json::Error),
    UnsupportedVersion(u64),
    EmptyRunId,
    DependencyPanicked,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "could not read manifest: {error}"),
            Self::Parse(error) => write!(formatter, "invalid manifest JSON: {error}"),
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "unsupported manifest version {version}; expected 1"
            ),
            Self::EmptyRunId => write!(formatter, "run_id must not be empty"),
            Self::DependencyPanicked => write!(formatter, "manifest reader dependency panicked"),
        }
    }
}
impl std::error::Error for ManifestError {}
#[cfg(test)]
mod tests {
    use super::{Manifest, ManifestError};
    use proptest::prelude::*;

    fn parse(contents: &str) -> Result<Manifest, ManifestError> {
        Manifest::from_slice(contents.as_bytes())
    }

    #[test]
    fn accepts_the_current_manifest() -> Result<(), ManifestError> {
        let manifest = parse(r#"{"version":1,"run_id":"smoke"}"#)?;

        assert_eq!(
            manifest,
            Manifest {
                run_id: "smoke".into(),
            }
        );
        Ok(())
    }

    #[test]
    fn rejects_unknown_fields() {
        let result = parse(r#"{"version":1,"run_id":"smoke","extra":true}"#);

        assert!(matches!(result, Err(ManifestError::Parse(_))));
    }

    #[test]
    fn rejects_missing_fields() {
        let result = parse(r#"{"version":1}"#);

        assert!(matches!(result, Err(ManifestError::Parse(_))));
    }

    #[test]
    fn rejects_wrong_field_types() {
        let result = parse(r#"{"version":1,"run_id":42}"#);

        assert!(matches!(result, Err(ManifestError::Parse(_))));
    }

    #[test]
    fn rejects_unsupported_versions() {
        let result = parse(r#"{"version":2,"run_id":"smoke"}"#);

        assert!(matches!(result, Err(ManifestError::UnsupportedVersion(2))));
    }

    #[test]
    fn rejects_an_empty_run_id() {
        let result = parse(r#"{"version":1,"run_id":""}"#);

        assert!(matches!(result, Err(ManifestError::EmptyRunId)));
    }

    proptest! {
        #[test]
        fn every_entry_point_preserves_valid_run_ids(run_id in ".+") {
            let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"run_id":run_id}))?;
            let parsed = Manifest::from_slice(&bytes)?;
            prop_assert_eq!(parsed.run_id(), &run_id);
            prop_assert_eq!(parsed.version(), 1);
            prop_assert_eq!(Manifest::from_reader(bytes.as_slice())?, serde_json::from_slice::<Manifest>(&bytes)?);
        }

        #[test]
        fn no_deserialization_entry_point_accepts_invalid_semantics(version in any::<u64>(), run_id in any::<String>()) {
            let bytes = serde_json::to_vec(&serde_json::json!({"version":version,"run_id":run_id}))?;
            let valid = version == 1 && !run_id.is_empty();
            prop_assert_eq!(Manifest::from_slice(&bytes).is_ok(), valid);
            prop_assert_eq!(Manifest::from_reader(bytes.as_slice()).is_ok(), valid);
            prop_assert_eq!(serde_json::from_slice::<Manifest>(&bytes).is_ok(), valid);
            let empty = br#"{"version":1,"run_id":""}"#;
            prop_assert!(Manifest::from_slice(empty).is_err());
            prop_assert!(serde_json::from_slice::<Manifest>(empty).is_err());
        }
    }

    #[test]
    fn every_entry_point_rejects_nonobjects_duplicate_fields_and_trailing_data() {
        for bytes in [
            br#"[1,"run"]"#.as_slice(),
            br#"{"version":1,"version":1,"run_id":"run"}"#,
            br#"{"version":1,"run_id":"run","run_id":"other"}"#,
            br#"{"version":1,"run_id":"run"} {}"#,
        ] {
            assert!(Manifest::from_slice(bytes).is_err());
            assert!(Manifest::from_reader(bytes).is_err());
            assert!(serde_json::from_slice::<Manifest>(bytes).is_err());
        }
    }

    #[test]
    fn reader_failures_remain_explicit() {
        struct FailedReader;
        impl std::io::Read for FailedReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected read failure"))
            }
        }
        struct UnwindingReader;
        impl std::io::Read for UnwindingReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                // Fault injection only, not a production error-handling path.
                std::panic::resume_unwind(Box::new("injected reader unwind"))
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
    }
}
