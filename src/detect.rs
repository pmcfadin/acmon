//! Recognising agent CLIs from declarative data.

use serde::Deserialize;

const EMBEDDED_DETECTORS: &str = include_str!("../detectors.toml");

/// One CLI's recognition rule.
#[derive(Debug, Clone, Deserialize)]
pub struct Detector {
    /// Stable identifier, also used as the displayed CLI name.
    pub id: String,
    /// Substrings of the resolved executable path, any of which identifies this CLI.
    ///
    /// Use this only where the identifying part of the path is in the middle — as it is
    /// for Claude Code, whose filename is a version string. A substring rule is the wrong
    /// tool for a name like "codex", which also appears in an application bundle's path.
    #[serde(default)]
    pub exe_contains: Vec<String>,
    /// Suffixes of the resolved executable path, any of which identifies this CLI.
    ///
    /// Anchoring to the end of the path is what separates a command-line tool from an
    /// application that merely mentions it: a CLI sits in a conventional binary
    /// directory, and a helper alongside it has a longer name.
    #[serde(default)]
    pub exe_ends_with: Vec<String>,
}

impl Detector {
    pub fn matches(&self, exe_path: &str) -> bool {
        self.exe_contains
            .iter()
            .any(|needle| exe_path.contains(needle))
            || self
                .exe_ends_with
                .iter()
                .any(|suffix| exe_path.ends_with(suffix))
    }

    /// Whether this detector can ever match anything.
    fn has_a_rule(&self) -> bool {
        !self.exe_contains.is_empty() || !self.exe_ends_with.is_empty()
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
    let detectors = toml::from_str::<DetectorFile>(EMBEDDED_DETECTORS)
        .expect("embedded detectors.toml must parse")
        .detector;

    // A detector with no rules matches nothing, for every process, silently — the exact
    // shape of the defect this project exists to remove. Refuse to start instead.
    if let Some(toothless) = detectors.iter().find(|d| !d.has_a_rule()) {
        panic!(
            "detector {:?} has no matching rules, so it would silently recognise nothing",
            toothless.id
        );
    }
    detectors
}
