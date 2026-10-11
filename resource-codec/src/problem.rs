//! The one error body (spec §3.8): RFC 9457 problem details served as
//! `application/problem+json`, with the HTTP status fixed by the registry.

use serde_json::{json, Value};

use crate::ir::Index;

/// What a failed call committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    Applied,
    Unknown,
}

impl Effect {
    fn wire(self) -> &'static str {
        match self {
            Effect::None => "none",
            Effect::Applied => "applied",
            Effect::Unknown => "unknown",
        }
    }
}

/// A failed call. `code` is a registry `ErrorCode` (without prefix).
#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: &'static str,
    pub detail: String,
    pub effect: Effect,
    pub details: Vec<Value>,
    pub retry_after_ms: Option<u64>,
}

impl ApiError {
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            effect: Effect::None,
            details: vec![],
            retry_after_ms: None,
        }
    }
    pub fn internal() -> Self {
        Self::new("INTERNAL", "An internal error occurred.").with_effect(Effect::Unknown)
    }
    pub fn with_effect(mut self, e: Effect) -> Self {
        self.effect = e;
        self
    }
    pub fn with_detail(mut self, d: Value) -> Self {
        self.details.push(d);
        self
    }
    pub fn not_found(name: &str) -> Self {
        Self::new("RESOURCE_NOT_FOUND", format!("{name} does not exist."))
    }
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new("INVALID_FIELD", detail)
    }
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self::new("UNAVAILABLE", detail)
    }

    /// `(grpc_status, http status, retryable)` of this code.
    pub fn class(&self, ix: &Index) -> (String, u16, bool) {
        if let Some(c) = ix.error_code(self.code) {
            let retryable = matches!(
                self.code,
                "IDEMPOTENCY_IN_PROGRESS"
                    | "ETAG_MISMATCH"
                    | "RATE_LIMITED"
                    | "UNAVAILABLE"
                    | "NOT_YET_PROPAGATED"
                    | "DEADLINE_EXCEEDED"
                    | "CONTRACT_SKEW"
            );
            // IDEMPOTENCY_KEY_REUSED is the one INVALID_ARGUMENT answered 422.
            let status = if self.code == "IDEMPOTENCY_KEY_REUSED" {
                422
            } else {
                c.http_status
            };
            return (c.grpc_status.clone(), status, retryable);
        }
        ("INTERNAL".into(), 500, false)
    }

    /// The problem body for request `request_id`.
    pub fn body(&self, ix: &Index, request_id: &str) -> (u16, Value) {
        let (grpc, status, retryable) = self.class(ix);
        let mut v = json!({
            "type": format!("https://sylphx.com/docs/errors/{}", self.code.to_ascii_lowercase().replace('_', "-")),
            "title": title(self.code),
            "status": status,
            "detail": self.detail,
            "instance": request_id,
            "code": self.code,
            "grpc_status": grpc,
            "retryable": retryable,
            "effect": self.effect.wire(),
        });
        if let Some(ms) = self.retry_after_ms {
            v["retry_after_ms"] = json!(ms.to_string());
        }
        if !self.details.is_empty() {
            v["details"] = Value::Array(self.details.clone());
        }
        (status, v)
    }
}

fn title(code: &str) -> &'static str {
    match code {
        "UNKNOWN_FIELD" => "The request has a field the schema does not know.",
        "INVALID_FIELD" => "A field failed validation.",
        "PAGE_TOKEN_MISMATCH" => "The page token belongs to another query.",
        "IDEMPOTENCY_KEY_REUSED" => "The Idempotency-Key was used with another request.",
        "IDEMPOTENCY_IN_PROGRESS" => {
            "The first request with this Idempotency-Key is still running."
        }
        "ETAG_MISMATCH" => "The resource changed since you read it.",
        "ETAG_CONFLICT" => "The If-Match header and the etag in the request differ.",
        "RESOURCE_NOT_FOUND" | "NOT_FOUND" => "The resource does not exist.",
        "RESOURCE_ALREADY_EXISTS" => "The resource already exists.",
        "UNAUTHENTICATED" => "No valid key or token was presented.",
        "PERMISSION_DENIED" => "The key lacks the permission this call needs.",
        "DELETION_PROTECTED" => "Deletion protection is on.",
        "RECONCILE_FAILED" => "The resource could not be reconciled.",
        "PLAN_LIMIT_REACHED" => "The plan's limit is reached.",
        "SPEND_LIMIT_REACHED" => "The spend limit is reached.",
        "RATE_LIMITED" => "Too many requests.",
        "INVALID_STATE" => "The resource's state forbids this call.",
        "UNAVAILABLE" => "The service is unavailable.",
        "UNIMPLEMENTED" => "This method is not served yet.",
        "NOT_YET_PROPAGATED" => "The key is not active in this region yet.",
        "DEADLINE_EXCEEDED" => "The deadline passed.",
        _ => "An internal error occurred.",
    }
}
