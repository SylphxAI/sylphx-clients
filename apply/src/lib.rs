//! `sylphx apply`: Resources declared in `sylphx.toml`, checked offline,
//! planned from Get and Update `validate_only`, and written with the
//! ordinary standard methods (docs/specs/one-platform/sylphx-apply.md).
//!
//! The crate is a library on the generated Rust SDK (`sylphx`): requests are
//! the SDK's `HttpRequest`s, answers and errors are decoded by its runtime.
//! The CLI's `apply` command and the Release step's Job are front ends of
//! [`Applier`].
//!
//! ```no_run
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! use sylphx_apply::{Applier, Declarable, Declarations, Options, Registry};
//!
//! let text = std::fs::read_to_string("sylphx.toml")?;
//! let decls = Declarations::from_toml(&text)?;
//! let transport = sylphx::HttpTransport::from_env()?;
//! let declarable = Declarable::embedded();
//! let applier = Applier::new(Registry::embedded(), &declarable, &transport, Options::default());
//! let plan = applier.plan(&decls, &[]).await?;
//! println!("{}", plan.report().to_json());
//! # Ok(()) }
//! ```

pub mod apply;
pub mod check;
pub mod declare;
pub mod diff;
pub mod eligibility;
pub mod error;
pub mod hash;
pub mod registry;
pub mod wire;

#[cfg(test)]
mod apply_tests;
#[cfg(test)]
mod fake;

pub use apply::{
    Action, Applied, Applier, Options, Outcome, Plan, PlanItem, Report, ANN_APPLIED, ANN_RELEASE,
    ANN_SOURCE, MANAGED_BY, MANAGER,
};
pub use check::{check, is_slug};
pub use declare::{Declaration, Declarations, Removed};
pub use diff::PatchOp;
pub use eligibility::Declarable;
pub use error::{ApplyError, Issue};
pub use registry::Registry;
pub use wire::Wire;
