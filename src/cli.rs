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
    Planned {
        tracked_as: &'static str,
    },
    /// Some of the verb is built and the rest is not, so an invocation does real work and
    /// still cannot do the job. `watch` is here: it takes and releases the lock (#26) around a
    /// collection loop that does not exist yet (#27). A caller must treat it exactly as it
    /// treats [`VerbState::Planned`] — the verb does not work — and the exit code says so.
    Partial {
        built: &'static str,
        tracked_as: &'static str,
    },
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
            AmonVerb::Watch => VerbState::Partial {
                built: "the single-writer lock, #26",
                tracked_as: "#27",
            },
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

/// The flag that runs the monitor in this terminal.
///
/// It exists for debugging and it is **still subject to the lock** (F19). Two writers is two
/// writers regardless of intent, so this changes where the monitor's noise goes and nothing
/// else.
pub const FOREGROUND_FLAG: &str = "--foreground";

/// A parsed `amon` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmonRequest {
    /// Help was asked for. That is a job done, and exits zero.
    Help,
    Verb {
        verb: AmonVerb,
        foreground: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// Nothing was asked for. Deliberately not "print help and succeed": a process that did
    /// nothing must not report success.
    NoVerb,
    UnknownVerb(String),
    /// A flag this verb does not take. Refused rather than ignored: someone who passed
    /// `--foreground` to a verb that has no foreground would otherwise be told nothing and
    /// conclude it had been honoured.
    FlagNotValidFor {
        flag: String,
        verb: AmonVerb,
    },
    UnexpectedArgument(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::NoVerb => write!(formatter, "no verb given"),
            CliError::UnknownVerb(name) => write!(formatter, "unknown verb `{name}`"),
            CliError::FlagNotValidFor { flag, verb } => {
                write!(formatter, "`{flag}` is not a flag `{}` takes", verb.name())
            }
            CliError::UnexpectedArgument(argument) => {
                write!(formatter, "unexpected argument `{argument}`")
            }
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

    let Some(verb) = AmonVerb::from_name(&first) else {
        return Err(CliError::UnknownVerb(first));
    };

    let mut foreground = false;
    for argument in arguments {
        match argument.as_str() {
            FOREGROUND_FLAG if verb == AmonVerb::Watch => foreground = true,
            FOREGROUND_FLAG => {
                return Err(CliError::FlagNotValidFor {
                    flag: argument,
                    verb,
                })
            }
            _ => return Err(CliError::UnexpectedArgument(argument)),
        }
    }

    Ok(AmonRequest::Verb { verb, foreground })
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
            VerbState::Partial { built, tracked_as } => {
                format!("  (has {built}; still not usable — {tracked_as})")
            }
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
         FLAGS\n\
         \x20   --foreground  Run `watch` in this terminal rather than under launchd, for\n\
         \x20                 debugging. Still subject to the single-writer lock: two writers\n\
         \x20                 is two writers regardless of intent, so a second `amon watch`\n\
         \x20                 is refused here too. `amon status` and the log are how you\n\
         \x20                 watch the resident one work.\n\
         \n\
         The display is a separate binary, `agtop`. If it measures, it is amon; if it draws,\n\
         it is agtop.\n",
    );

    text
}

/// The flag that prints one pass as plain lines instead of taking the screen.
///
/// Not a fallback. It is what keeps the renderer testable against a fixed buffer instead of a
/// live terminal, and what keeps the output pipeable (F34).
pub const ONCE_FLAG: &str = "--once";

/// A parsed `agtop` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgtopRequest {
    Help,
    /// Full-screen, refreshing while open. The default, because it is what the tool is for.
    Live,
    /// One pass, as plain lines, then exit.
    Once,
}

/// Parse `agtop`'s arguments, excluding argv[0].
///
/// Deliberately strict about verbs. `agtop watch` reading as a successful monitor start would
/// undo the whole point of there being two names.
pub fn parse_agtop<I>(arguments: I) -> Result<AgtopRequest, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut request = AgtopRequest::Live;

    for argument in arguments {
        match argument.as_str() {
            "--help" | "-h" => return Ok(AgtopRequest::Help),
            ONCE_FLAG => request = AgtopRequest::Once,
            _ => return Err(CliError::UnexpectedArgument(argument)),
        }
    }

    Ok(request)
}

/// `agtop`'s help text.
pub fn agtop_usage() -> String {
    String::from(
        "agtop — the display. Draws; it never measures, records or notifies.\n\
         \n\
         USAGE\n\
         \x20   agtop           Full screen, refreshing while it is open.\n\
         \x20   agtop --once    One pass as plain lines, then exit. Pipeable.\n\
         \x20   agtop --help\n\
         \n\
         Polls the state file `amon` writes and renders it. It is read-only: it writes no\n\
         state and sends no notification, because a notification from a foreground UI is\n\
         redundant with looking at it, and a second writer would undo the single-writer\n\
         guarantee the split rests on.\n\
         \n\
         With nothing published it collects once for itself and says, on screen, that\n\
         nothing is being recorded or alerted.\n\
         \n\
         There are no keybindings but the ones that leave — q, Esc, Ctrl-C. Sorting is\n\
         fixed: an interactive display invites the keystroke that kills a process, and this\n\
         tool never signals an agent.\n\
         \n\
         The monitor is a separate binary, `amon`. If it measures, it is amon; if it draws,\n\
         it is agtop.\n",
    )
}
