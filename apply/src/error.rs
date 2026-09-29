//! The errors of a plan or an apply.

use serde::Serialize;

/// One problem the offline check found, at `location`
/// (`resource[0].spec.products[1].key`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Issue {
    pub location: String,
    pub message: String,
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.message)
    }
}

/// Why a plan or an apply stopped.
#[derive(Debug)]
#[non_exhaustive]
pub enum ApplyError {
    /// The file did not parse.
    Parse(String),
    /// The offline check found problems.
    Invalid(Vec<Issue>),
    /// The API refused a call, or did not answer.
    Api(sylphx::Error),
    /// The plan refuses some Resources; nothing was written.
    Refused(Vec<String>),
    /// A written Resource reports `Stalled`, or its Operation failed.
    Stalled { name: String, message: String },
    /// The Resource kept changing under the run.
    Conflict(String),
    /// An answer the planner cannot read.
    Unexpected(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Parse(e) => write!(f, "{e}"),
            ApplyError::Invalid(issues) => {
                write!(f, "the declarations are invalid:")?;
                for i in issues {
                    write!(f, "\n  {i}")?;
                }
                Ok(())
            }
            ApplyError::Api(e) => write!(f, "{e}"),
            ApplyError::Refused(reasons) => {
                write!(f, "apply refused:")?;
                for r in reasons {
                    write!(f, "\n  {r}")?;
                }
                Ok(())
            }
            ApplyError::Stalled { name, message } => write!(f, "{name} is stalled: {message}"),
            ApplyError::Conflict(e) => write!(f, "{e}"),
            ApplyError::Unexpected(e) => write!(f, "unexpected answer: {e}"),
        }
    }
}

impl std::error::Error for ApplyError {}

impl From<sylphx::Error> for ApplyError {
    fn from(e: sylphx::Error) -> Self {
        ApplyError::Api(e)
    }
}
