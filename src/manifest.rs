use std::{
    fmt,
    fs::File,
    io::{BufReader, Read},
    path::Path,
};

use serde::Deserialize;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u64,
    pub run_id: String,
}

impl Manifest {
    pub fn from_path(path: &Path) -> Result<Self, ManifestError> {
        let file = File::open(path).map_err(ManifestError::Read)?;
        Self::from_reader(BufReader::new(file))
    }

    pub fn from_reader(reader: impl Read) -> Result<Self, ManifestError> {
        let manifest: Self = serde_json::from_reader(reader).map_err(ManifestError::Parse)?;

        if manifest.version != 1 {
            return Err(ManifestError::UnsupportedVersion(manifest.version));
        }
        if manifest.run_id.is_empty() {
            return Err(ManifestError::EmptyRunId);
        }

        Ok(manifest)
    }
}

#[derive(Debug)]
pub enum ManifestError {
    Read(std::io::Error),
    Parse(serde_json::Error),
    UnsupportedVersion(u64),
    EmptyRunId,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "could not read manifest: {error}"),
            Self::Parse(error) => write!(formatter, "invalid manifest JSON: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "unsupported manifest version {version}; expected 1"
                )
            }
            Self::EmptyRunId => write!(formatter, "run_id must not be empty"),
        }
    }
}

impl std::error::Error for ManifestError {}

#[cfg(test)]
mod tests {
    use super::{Manifest, ManifestError};

    fn parse(contents: &str) -> Result<Manifest, ManifestError> {
        Manifest::from_reader(contents.as_bytes())
    }

    #[test]
    fn accepts_the_current_manifest() {
        let manifest = parse(r#"{"version":1,"run_id":"smoke"}"#).unwrap();

        assert_eq!(
            manifest,
            Manifest {
                version: 1,
                run_id: "smoke".into(),
            }
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = parse(r#"{"version":1,"run_id":"smoke","extra":true}"#).unwrap_err();

        assert!(matches!(error, ManifestError::Parse(_)));
    }

    #[test]
    fn rejects_missing_fields() {
        let error = parse(r#"{"version":1}"#).unwrap_err();

        assert!(matches!(error, ManifestError::Parse(_)));
    }

    #[test]
    fn rejects_wrong_field_types() {
        let error = parse(r#"{"version":1,"run_id":42}"#).unwrap_err();

        assert!(matches!(error, ManifestError::Parse(_)));
    }

    #[test]
    fn rejects_unsupported_versions() {
        let error = parse(r#"{"version":2,"run_id":"smoke"}"#).unwrap_err();

        assert!(matches!(error, ManifestError::UnsupportedVersion(2)));
    }

    #[test]
    fn rejects_an_empty_run_id() {
        let error = parse(r#"{"version":1,"run_id":""}"#).unwrap_err();

        assert!(matches!(error, ManifestError::EmptyRunId));
    }
}
