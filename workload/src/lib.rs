//! Public SPIFFE workload identity helpers and the native Workload API client.
//!
//! [`customer_workload`] constructs and parses the canonical customer workload
//! identity from its actual trust domain, Kubernetes namespace and service
//! account. Naming is not proof of identity or tenant authorization: parse an
//! SVID's identity only after cryptographic validation.
//!
//! The default `workload` feature enables `workload`, the JWT-SVID client for
//! the SPIRE agent's Unix socket. Disable default features for dependency-free
//! customer identity construction and parsing.
//!
//! ```no_run
//! # #[cfg(feature = "workload")]
//! async fn caller_proof() -> Result<String, String> {
//!     use std::time::Duration;
//!     use sylphx_workload::{customer_workload, workload};
//!     let identity = customer_workload::spiffe_id(
//!         "spiffe.sylphx.net",
//!         "env-0123456789abcdef0123",
//!         "api-123456",
//!     )?;
//!     workload::fetch_jwt_svid("relying-party", &identity, Duration::from_secs(2)).await
//! }
//! ```

pub mod customer_workload;

#[cfg(feature = "workload")]
pub mod workload;
