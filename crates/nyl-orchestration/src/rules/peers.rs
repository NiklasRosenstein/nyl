//! Whether a problem with another environment is an error or a warning.
//!
//! Contract: [Other environments](../../../../design/orchestration-core.md#other-environments).
//!
//! A command reads another environment for one of two reasons, and the reason
//! decides what a problem there means, whatever kind of problem it is:
//!
//! - [`Need::Required`]: the command cannot do its job without it, such as the
//!   producer environment a unit's reference resolves from. An invalid
//!   declaration or unreadable state there is an error, with the cause's own
//!   code and fix.
//! - [`Need::Advisory`]: the command reads it only to give advice, such as the
//!   scan of `nyl observe` for consumers elsewhere whose input changed. Any
//!   problem there is a `NYL-STATE-UNAVAILABLE` warning that names the
//!   environment, never a failure of the command.
//!
//! An environment a command does not read at all is not judged: its problems
//! belong to `nyl validate` and to the commands that need it.

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::EnvironmentName;

/// Why a command reads another environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// The command must read it to do its job.
    Required,
    /// The command reads it only to give advice.
    Advisory,
}

/// What a problem with another environment means to the command that read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The command cannot do its job.
    Error(Finding),
    /// The command goes on without what it could not read.
    Warning(Finding),
}

impl Problem {
    /// The finding, whichever it is.
    pub fn into_finding(self) -> Finding {
        match self {
            Self::Error(finding) | Self::Warning(finding) => finding,
        }
    }
}

/// Judge `cause`, a problem with `peer`, by why the command read `peer`.
///
/// A required problem keeps the cause's code, message, and fix. An advisory
/// one is a `NYL-STATE-UNAVAILABLE` warning that names `peer`, says its
/// consumers were not checked, and adds `nyl validate` to the cause's own fix
/// (access to the repository for an unreadable one, the declaration for an
/// invalid one), because validate reports an invalid declaration as an error.
/// `peer` may also be the synthetic name of the instances of a template that
/// could not be listed; the cause's message says which.
pub fn judge(need: Need, peer: &EnvironmentName, cause: Finding) -> Problem {
    match need {
        Need::Required => Problem::Error(cause),
        Need::Advisory => Problem::Warning(Finding::new(
            Code::StateUnavailable,
            format!("the consumers in {peer} were not checked: {}", cause.message),
            format!(
                "{}; if a declaration is invalid, `nyl validate` reports it as an error",
                cause.fix
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(name: &str) -> EnvironmentName {
        EnvironmentName::new(name).unwrap()
    }

    fn cause() -> Finding {
        Finding::new(
            Code::EnvironmentUnknown,
            "cannot resolve the state location of prod: no such repository",
            "fix spec.state of the Environment",
        )
    }

    /// Catches a required peer's problem losing its code or fix, which the
    /// operator needs to act, or an advisory peer's problem failing the command.
    #[test]
    fn test_a_required_problem_is_the_causes_error_and_an_advisory_one_a_warning() {
        let required = judge(Need::Required, &name("prod"), cause());
        assert_eq!(required, Problem::Error(cause()));

        let Problem::Warning(warning) = judge(Need::Advisory, &name("prod"), cause()) else {
            panic!("an advisory problem is a warning");
        };
        assert_eq!(warning.code, Code::StateUnavailable);
        assert!(warning.message.contains("prod") && warning.message.contains("no such repository"));
        assert!(warning.fix.contains("fix spec.state") && warning.fix.contains("nyl validate"));
    }
}
