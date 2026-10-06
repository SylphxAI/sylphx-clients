//! The build lease, shared by `sylphx build run` and the Build service.
//!
//! - [`guest`]: the lease's guest daemon over the E2B envd protocol (a process
//!   stream, files).
//! - [`machine`]: the lease request of a build, waiting for the machine to be
//!   ready, a guest token, releasing the lease, and how a fault is judged.

pub mod guest;
pub mod machine;
pub mod scripts;

pub use machine::{
    allowed_domains, guest, judge, lease_request, mint_token, provision_failure, release,
    release_request, state, wait_ready, why, wire, LeaseParams, Ready, Verdict, BUILD_CACHE_HOST,
    BUILD_PACKAGES, IDLE_TIMEOUT, TEMPLATE, TOKEN_TTL, TTL_SLACK, WS,
};
