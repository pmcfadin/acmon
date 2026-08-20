//! The command-line surfaces of the two binaries.
//!
//! Kept in the library rather than in the binaries so that the verb list, the help text and
//! the parser are one thing tested together. The failure that arrangement prevents is a verb
//! advertised in help that the parser has never heard of, or the reverse.
//!
//! The rule the split is built on: **if it measures, it is `amon`; if it draws, it is
//! `agtop`.**

use std::fmt;

/// What `amon` can be asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmonVerb {
    Watch,
    Install,
    Uninstall,
    Status,
    Probe,
    Report,
}

/// Whether a verb does its job today, and if not, where the work that delivers it lives.
///
/// A recognised-but-unbuilt verb has to be distinguishable from an abandoned one. "not
/// implemented" on its own leaves a reader unable to tell which they are looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerbState {
    Available,
    Planned { tracked_as: &'static str },
}

impl AmonVerb {
    /// Every verb `amon` advertises. Help is generated from this, and so is the parser, so
    /// the two cannot drift apart.
    pub fn all() -> &'static [AmonVerb] {
        &[
            AmonVerb::Watch,
            AmonVerb::Install,
            AmonVerb::Uninstall,
            AmonVerb::Status,
            AmonVerb::Probe,
            AmonVerb::Report,
        ]
    }

    pub fn name(&self) -> &'static str {
        match self {
            AmonVerb::Watch => "watch",
            AmonVerb::Install => "install",
            AmonVerb::Uninstall => "uninstall",
            AmonVerb::Status => "status",
            AmonVerb::Probe => "probe",
            AmonVerb::Report => "report",
        }
    }

    pub fn summary(&self) -> &'static str {
        match self {
            AmonVerb::Watch => {
                "Collect on every tier, write state, notify. Resident; the sole writer."
            }
            AmonVerb::Install => {
                "Write and load the LaunchAgent, so alerts fire with no terminal open."
            }
            AmonVerb::Uninstall => "Unload and remove the LaunchAgent.",
            AmonVerb::Status => {
                "Report whether the job is loaded, a process is running, and the last write's age."
            }
            AmonVerb::Probe => "Measure this machine's own tax. One shot, holds no state.",
            AmonVerb::Report => "Render a measured profile as findings. One shot.",
        }
    }

    pub fn state(&self) -> VerbState {
        match self {
            AmonVerb::Watch => VerbState::Planned { tracked_as: "#27" },
            AmonVerb::Install | AmonVerb::Uninstall | AmonVerb::Status => {
                VerbState::Planned { tracked_as: "#11" }
            }
            AmonVerb::Probe | AmonVerb::Report => VerbState::Planned { tracked_as: "v2" },
        }
    }

    pub fn from_name(name: &str) -> Option<AmonVerb> {
        AmonVerb::all()
            .iter()
            .find(|verb| verb.name() == name)
            .copied()
    }
}

/// A parsed `amon` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmonRequest {
    /// Help was asked for. That is a job done, and exits zero.
    Help,
    Verb(AmonVerb),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// Nothing was asked for. Deliberately not "print help and succeed": a process that did
    /// nothing must not report success.
    NoVerb,
    UnknownVerb(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::NoVerb => write!(formatter, "no verb given"),
            CliError::UnknownVerb(name) => write!(formatter, "unknown verb `{name}`"),
        }
    }
}

impl std::error::Error for CliError {}

/// Parse `amon`'s arguments, excluding argv[0].
pub fn parse_amon<I>(arguments: I) -> Result<AmonRequest, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = arguments.into_iter();

    let Some(first) = arguments.next() else {
        return Err(CliError::NoVerb);
    };

    if first == "--help" || first == "-h" {
        return Ok(AmonRequest::Help);
    }

    match AmonVerb::from_name(&first) {
        Some(verb) => Ok(AmonRequest::Verb(verb)),
        None => Err(CliError::UnknownVerb(first)),
    }
}

/// `amon`'s help text, generated from [`AmonVerb::all`].
pub fn amon_usage() -> String {
    let width = AmonVerb::all()
        .iter()
        .map(|verb| verb.name().len())
        .max()
        .unwrap_or(0);

    let mut text = String::from(
        "amon — the monitor. Measures, records and notifies; it never draws.\n\
         \n\
         USAGE\n\
         \x20   amon <verb>\n\
         \x20   amon --help\n\
         \n\
         VERBS\n",
    );

    for verb in AmonVerb::all() {
        let marker = match verb.state() {
            VerbState::Available => String::new(),
            VerbState::Planned { tracked_as } => format!("  (not built yet — {tracked_as})"),
        };
        text.push_str(&format!(
            "    {:width$}  {}{}\n",
            verb.name(),
            verb.summary(),
            marker,
            width = width
        ));
    }

    text.push_str(
        "\n\
         The display is a separate binary, `agtop`. If it measures, it is amon; if it draws,\n\
         it is agtop.\n",
    );

    text
}

/// `agtop`'s help text.
pub fn agtop_usage() -> String {
    String::from(
        "agtop — the display. Draws; it never measures, records or notifies.\n\
         \n\
         USAGE\n\
         \x20   agtop\n\
         \x20   agtop --help\n\
         \n\
         Renders what `amon` recorded. With no monitor running it collects once for itself\n\
         and says that nothing is being recorded.\n\
         \n\
         The monitor is a separate binary, `amon`. If it measures, it is amon; if it draws,\n\
         it is agtop.\n",
    )
}
