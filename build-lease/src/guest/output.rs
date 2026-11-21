//! Exact-length guest output transfer into private, caller-owned staging.

use reqwest::{Method, StatusCode};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::{q, Duration, Guest};

/// A failed transfer never authorizes use of the staging contents.
/// Errors contain no request URL, headers, or response body.
#[derive(Debug, PartialEq, Eq)]
pub enum OutputTransferError {
    LimitExceeded,
    LengthMismatch,
    Refused(StatusCode),
    Transport,
    Staging,
}

impl Guest {
    /// Copy exactly `expected_bytes` from this lease, subject to the caller's byte
    /// budget `max_bytes`. Neither a missing file nor a partial response succeeds.
    ///
    /// The caller supplies a private staging sink, discards it after any failure
    /// or cancellation, and separately validates content integrity before use.
    /// Count success does not prove the content's digest or publish the output.
    pub async fn download_exact_into(
        &self,
        path: &str,
        user: &str,
        expected_bytes: u64,
        max_bytes: u64,
        staging: &mut (impl AsyncWrite + Unpin + Send + ?Sized),
    ) -> Result<(), OutputTransferError> {
        use OutputTransferError::{LengthMismatch, LimitExceeded, Refused, Staging, Transport};

        if expected_bytes > max_bytes {
            return Err(LimitExceeded);
        }
        let mut resp = self
            .req(
                Method::GET,
                &format!("files?path={}&username={}", q(path), q(user)),
                user,
            )
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .map_err(|_| Transport)?;
        if resp.status() != StatusCode::OK {
            return Err(Refused(resp.status()));
        }
        if let Some(len) = resp.content_length() {
            if len > max_bytes {
                return Err(LimitExceeded);
            }
            if len != expected_bytes {
                return Err(LengthMismatch);
            }
        }

        let mut remaining = expected_bytes;
        while let Some(chunk) = resp.chunk().await.map_err(|_| Transport)? {
            let len = chunk.len() as u64;
            if len > remaining {
                return Err(if len - remaining > max_bytes - expected_bytes {
                    LimitExceeded
                } else {
                    LengthMismatch
                });
            }
            staging.write_all(&chunk).await.map_err(|_| Staging)?;
            remaining -= len;
        }
        if remaining != 0 {
            return Err(LengthMismatch);
        }
        staging.flush().await.map_err(|_| Staging)
    }
}

#[cfg(test)]
mod tests;
