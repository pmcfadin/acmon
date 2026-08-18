//! Recognising agent CLIs from declarative data.

use serde::Deserialize;

const EMBEDDED_DETECTORS: &str = include_str!("../detectors.toml");

/// One CLI's recognition rule.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

/// Parse user-supplied detector configuration from TOML.
///
/// Returns `Ok(detectors)` when the text parses and every detector has at least one rule.
/// Returns `Err` when the text is malformed or a detector has no rules — the latter being
/// a runtime hazard exactly like the embedded case, but one that arrives from user data
/// rather than from the build.
pub fn parse_user_detectors(text: &str) -> Result<Vec<Detector>, String> {
    let file: DetectorFile =
        toml::from_str(text).map_err(|e| format!("could not parse detector configuration: {e}"))?;

    // A user-supplied detector with no rules is the same hazard as an embedded one: it
    // matches nothing, for every process, silently. The embedded version panics because
    // it is a build-time authoring error; this one returns an error because it is runtime
    // user data, and panicking on user input is never the answer.
    if let Some(toothless) = file.detector.iter().find(|d| !d.has_a_rule()) {
        return Err(format!(
            "detector {:?} has no matching rules — it would silently recognise nothing",
            toothless.id
        ));
    }

    Ok(file.detector)
}

/// Layer user-supplied detectors over the embedded defaults.
///
/// A user detector whose `id` matches an embedded one replaces it entirely — not a
/// field-wise merge, which would make it impossible to *remove* a rule that is matching
/// something it should not. A user detector with a new `id` adds to the set.
///
/// This is what makes "adding support for an additional agent CLI requires no code change"
/// true: a user can copy the embedded `detectors.toml`'s shape and either extend it or
/// override an entry that is over-matching.
pub fn merge_detectors(embedded: Vec<Detector>, user: Vec<Detector>) -> Vec<Detector> {
    let mut result = embedded;

    for user_detector in user {
        // Replace if an embedded detector has the same id, otherwise add.
        if let Some(pos) = result.iter().position(|d| d.id == user_detector.id) {
            result[pos] = user_detector;
        } else {
            result.push(user_detector);
        }
    }

    result
}
