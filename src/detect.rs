//! Recognising agent CLIs from declarative data.

use serde::Deserialize;

const EMBEDDED_DETECTORS: &str = include_str!("../detectors.toml");

/// One CLI's recognition rule.
#[derive(Debug, Clone, Deserialize)]
pub struct Detector {
    /// Stable identifier, also used as the displayed CLI name.
    pub id: String,
    /// Substrings of the resolved executable path, any of which identifies this CLI.
    pub exe_contains: Vec<String>,
}

impl Detector {
    pub fn matches(&self, exe_path: &str) -> bool {
        self.exe_contains
            .iter()
            .any(|needle| exe_path.contains(needle))
    }
}

#[derive(Debug, Deserialize)]
struct DetectorFile {
    detector: Vec<Detector>,
}

/// The detectors compiled into the binary.
///
/// Panics if the embedded data is malformed, which is a build-time authoring error
/// rather than a runtime condition: the file ships inside the binary.
pub fn embedded_detectors() -> Vec<Detector> {
    toml::from_str::<DetectorFile>(EMBEDDED_DETECTORS)
        .expect("embedded detectors.toml must parse")
        .detector
}
