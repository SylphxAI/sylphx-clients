//! The native Resource API codec shared by Sylphx clients and the front.
//!
//! The registry and OpenAPI assets ship inside this package and are generated
//! from the same contracts as the server. No private Cloud source is needed.
//!
//! Use [`ir::Index::embedded`] to index that registry, [`route::Router`] to
//! match methods, [`schema::field_at`] and [`schema::validate`] for native
//! request schemas, and [`problem::ApiError`] for RFC 9457 problem details.

pub mod ir;
pub mod problem;
pub mod route;
pub mod schema;
