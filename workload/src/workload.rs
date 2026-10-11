//! The SPIFFE Workload API client: JWT-SVIDs over the agent's Unix socket.
//!
//! Callers fetch an SVID for a relying party, which validates it through its
//! own agent before authorizing the returned SPIFFE identity. This client is
//! shared by the platform and public SDK consumers. Hand-rolled gRPC-over-HTTP/2
//! because the package-less SPIFFE
//! Workload API full method is `/SpiffeWorkloadAPI/<Method>` with
//! `workload.spiffe.io: true`, and the two messages this needs are tiny:
//! pulling in a protobuf runtime and a code generator for them would be more
//! machinery than protocol.
//!
//! The socket is mounted by the SPIFFE CSI driver (`csi.spiffe.io`) at
//! [`SPIFFE_WORKLOAD_SOCKET`]. A pod without the volume has no socket and
//! cannot fetch an SVID at all — [`socket_available`] says which case a
//! process is in, and [`is_socket_absent`] tells an absent socket apart from
//! an agent that is present and refusing.

use std::time::Duration;

/// Path where the SPIFFE Workload API socket is mounted in a pod.
pub const SPIFFE_WORKLOAD_SOCKET: &str = "/spiffe-workload-api/spire-agent.sock";

/// gRPC full methods of the package-less SPIFFE Workload API
/// (verified against SPIRE 1.14.5 through server reflection).
const FETCH_JWT_SVID_PATH: &str = "/SpiffeWorkloadAPI/FetchJWTSVID";
const VALIDATE_JWT_SVID_PATH: &str = "/SpiffeWorkloadAPI/ValidateJWTSVID";

/// A fetch or validation that names no timeout waits this long.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// The connect failure detail for a pod that mounts no socket, so a caller
/// can tell an unmounted volume (expected in some deployments) apart from an
/// identity outage (agent present, refusing).
pub const SOCKET_ABSENT: &str = "workload_api_socket_absent";

/// Does this process have a Workload API socket to ask?
#[must_use]
pub fn socket_available() -> bool {
    std::path::Path::new(SPIFFE_WORKLOAD_SOCKET).exists()
}

/// Is this failure detail the absent-socket shape rather than an outage?
#[must_use]
pub fn is_socket_absent(error: &str) -> bool {
    error.contains(SOCKET_ABSENT)
}

/// Name a connect failure. Only a missing socket is an expected deployment
/// shape; every other errno stays a distinct, loud transport detail.
fn connect_detail(error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        SOCKET_ABSENT.to_string()
    } else {
        format!("uds_connect: {error}")
    }
}

/// Fetch one JWT-SVID for `audience` from the Workload API.
///
/// `spiffe_id` selects the entry to return when the pod holds more than one
/// registration (an empty `spiffe_id` takes the first entry that carries a
/// token). An empty `audience` is refused before the socket: it asks SPIRE
/// for a token any audience would accept, which no relying party can pin.
pub async fn fetch_jwt_svid(
    audience: &str,
    spiffe_id: &str,
    timeout: Duration,
) -> Result<String, String> {
    if audience.is_empty() {
        return Err("jwt-svid audience must be named".to_string());
    }
    let budget = if timeout.is_zero() {
        DEFAULT_TIMEOUT
    } else {
        timeout
    };
    tokio::time::timeout(budget, async {
        let message = unary(
            FETCH_JWT_SVID_PATH,
            &encode_jwt_svid_request(audience, spiffe_id),
        )
        .await?;
        decode_jwt_svid_response(&message, spiffe_id)
    })
    .await
    .map_err(|_| "workload_api_timeout".to_string())?
}

/// Validate a JWT-SVID for `audience` through the local agent: signature
/// against the trust domain's JWT bundle, expiry, and audience. Returns the
/// token's SPIFFE id; the caller decides which ids it admits.
pub async fn validate_jwt_svid(
    token: &str,
    audience: &str,
    timeout: Duration,
) -> Result<String, String> {
    if audience.is_empty() || token.is_empty() {
        return Err("jwt-svid audience and token must be named".to_string());
    }
    let budget = if timeout.is_zero() {
        DEFAULT_TIMEOUT
    } else {
        timeout
    };
    tokio::time::timeout(budget, async {
        let message = unary(
            VALIDATE_JWT_SVID_PATH,
            &encode_validate_jwt_svid_request(audience, token),
        )
        .await?;
        decode_validate_jwt_svid_response(&message)
    })
    .await
    .map_err(|_| "workload_api_timeout".to_string())?
}

/// One unary call: connect, handshake, send one framed message, read one.
/// A connection per call is enough here — the agent is on the same node and
/// these calls carry an SVID, not bulk data.
async fn unary(path: &str, body: &[u8]) -> Result<Vec<u8>, String> {
    use bytes::BytesMut;
    let stream = tokio::net::UnixStream::connect(SPIFFE_WORKLOAD_SOCKET)
        .await
        .map_err(|error| connect_detail(&error))?;
    let (mut sender, conn) = h2::client::handshake(stream)
        .await
        .map_err(|e| format!("h2_handshake: {e}"))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let request = http::Request::builder()
        .method(http::Method::POST)
        .uri(path)
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .header("workload.spiffe.io", "true")
        .body(())
        .map_err(|e| format!("request_build: {e}"))?;
    let (response, mut send_stream) = sender
        .send_request(request, false)
        .map_err(|e| format!("h2_send_request: {e}"))?;
    send_stream
        .send_data(grpc_frame(body), true)
        .map_err(|e| format!("h2_send_data: {e}"))?;
    let mut response = response.await.map_err(|e| format!("h2_response: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("http_status_{status}"));
    }
    let mut buf = BytesMut::new();
    let body = response.body_mut();
    let mut message = None;
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|e| format!("h2_body: {e}"))?;
        buf.extend_from_slice(&chunk);
        body.flow_control()
            .release_capacity(chunk.len())
            .map_err(|e| format!("h2_flow: {e}"))?;
        if let Some(msg) = try_pop_grpc_message(&mut buf)? {
            message = Some(msg);
            break;
        }
    }
    if message.is_none() {
        // An empty stream is how the agent refuses (expired SVID, unknown
        // audience): the reason is in the trailers, not in a message.
        if let Some(trailers) = body
            .trailers()
            .await
            .map_err(|e| format!("h2_trailers: {e}"))?
        {
            if let Some(gs) = trailers.get("grpc-status").and_then(|v| v.to_str().ok()) {
                if gs != "0" {
                    let gm = trailers
                        .get("grpc-message")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    return Err(format!("grpc_status_{gs}:{gm}"));
                }
            }
        }
        message = try_pop_grpc_message(&mut buf)?;
    }
    message.ok_or_else(|| "empty_jwt_svid_stream".to_string())
}

/// `iat` and `exp` of a JWT, in milliseconds since the epoch.
///
/// The payload is decoded without any signature check: every caller uses this
/// only to time a cache refresh, never to admit a caller. `None` when the
/// token has no readable payload or no numeric `iat`/`exp`.
#[must_use]
pub fn jwt_iat_exp_ms(token: &str) -> Option<(u64, u64)> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let claim = |name: &str| -> Option<u64> {
        claims
            .get(name)
            .and_then(serde_json::Value::as_f64)
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(|n| (n * 1000.0) as u64)
    };
    Some((claim("iat")?, claim("exp")?))
}

/// `JWTSVIDRequest { repeated string audience = 1; string spiffe_id = 2; }`.
///
/// The SPIFFE ID is named when there is one, not left to SPIRE: a pod that
/// matches two registrations would otherwise get SVIDs back in no promised
/// order, and the relying party pins one `sub`.
fn encode_jwt_svid_request(audience: &str, spiffe_id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(audience.len() + spiffe_id.len() + 8);
    push_length_delimited(1, audience.as_bytes(), &mut out);
    if !spiffe_id.is_empty() {
        push_length_delimited(2, spiffe_id.as_bytes(), &mut out);
    }
    out
}

/// `ValidateJWTSVIDRequest { string audience = 1; string svid = 2; }`.
fn encode_validate_jwt_svid_request(audience: &str, token: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(audience.len() + token.len() + 8);
    push_length_delimited(1, audience.as_bytes(), &mut out);
    push_length_delimited(2, token.as_bytes(), &mut out);
    out
}

/// `JWTSVIDResponse { repeated JWTSVID svids = 1; }` where
/// `JWTSVID { string spiffe_id = 1; string svid = 2; ... }`.
///
/// Returns the token of the entry whose `spiffe_id` is exactly `want`, or the
/// first entry that carries a token when `want` is empty. Never the first
/// entry by position for a named `want`: another identity's SVID is refused
/// by the relying party. An entry with no token is skipped rather than
/// returned as an empty string, which would be presented as a bearer token.
fn decode_jwt_svid_response(message: &[u8], want: &str) -> Result<String, String> {
    let mut i = 0usize;
    while i < message.len() {
        let (key, ni) = read_varint(message, i)?;
        i = ni;
        let field = (key >> 3) as u32;
        let wire = (key & 0x7) as u8;
        if field == 1 && wire == 2 {
            let (len, ni) = read_varint(message, i)?;
            let end = delimited_end(message.len(), ni, len, "truncated_jwt_svid_entry")?;
            let (id, svid) = decode_jwt_svid_entry(&message[ni..end])?;
            i = end;
            if let (Some(id), Some(svid)) = (id, svid) {
                if want.is_empty() || id == want {
                    return Ok(svid);
                }
            }
        } else {
            skip_field(message, &mut i, wire)?;
        }
    }
    if want.is_empty() {
        Err("no_jwt_svid_in_response".into())
    } else {
        Err(format!("no_jwt_svid_for {want}"))
    }
}

/// The `spiffe_id` (field 1) and `svid` (field 2) of one `JWTSVID`.
fn decode_jwt_svid_entry(bytes: &[u8]) -> Result<(Option<String>, Option<String>), String> {
    let mut i = 0usize;
    let mut id: Option<String> = None;
    let mut svid: Option<String> = None;
    while i < bytes.len() {
        let (key, ni) = read_varint(bytes, i)?;
        i = ni;
        let field = (key >> 3) as u32;
        let wire = (key & 0x7) as u8;
        if (field == 1 || field == 2) && wire == 2 {
            let (len, ni) = read_varint(bytes, i)?;
            let end = delimited_end(bytes.len(), ni, len, "truncated_jwt_svid_svid")?;
            let text = String::from_utf8(bytes[ni..end].to_vec())
                .map_err(|_| "jwt_svid_not_utf8".to_string())?;
            if !text.is_empty() {
                if field == 1 {
                    id = Some(text);
                } else {
                    svid = Some(text);
                }
            }
            i = end;
        } else {
            skip_field(bytes, &mut i, wire)?;
        }
    }
    Ok((id, svid))
}

/// `ValidateJWTSVIDResponse { string spiffe_id = 1; Struct claims = 2; }`.
/// The claims are ignored: the agent already admitted the token, and the
/// caller authorizes on the SPIFFE id.
fn decode_validate_jwt_svid_response(message: &[u8]) -> Result<String, String> {
    let mut i = 0usize;
    while i < message.len() {
        let (key, ni) = read_varint(message, i)?;
        i = ni;
        let field = (key >> 3) as u32;
        let wire = (key & 0x7) as u8;
        if field == 1 && wire == 2 {
            let (len, ni) = read_varint(message, i)?;
            let end = delimited_end(message.len(), ni, len, "truncated_spiffe_id")?;
            let id = String::from_utf8(message[ni..end].to_vec())
                .map_err(|_| "spiffe_id_not_utf8".to_string())?;
            i = end;
            if !id.is_empty() {
                return Ok(id);
            }
        } else {
            skip_field(message, &mut i, wire)?;
        }
    }
    Err("jwt_svid_validation_returned_no_spiffe_id".into())
}

fn push_length_delimited(field: u32, value: &[u8], out: &mut Vec<u8>) {
    push_varint(out, ((field as u64) << 3) | 2);
    push_varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn grpc_frame(message: &[u8]) -> bytes::Bytes {
    use bytes::BytesMut;
    let mut out = BytesMut::with_capacity(5 + message.len());
    out.extend_from_slice(&[0]); // compressed flag
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out.freeze()
}

/// The next complete message in `buf`, `None` while it is still partial.
/// Compressed messages are refused rather than mis-decoded.
fn try_pop_grpc_message(buf: &mut bytes::BytesMut) -> Result<Option<Vec<u8>>, String> {
    if buf.len() < 5 {
        return Ok(None);
    }
    if buf[0] != 0 {
        return Err("grpc_compressed_not_supported".into());
    }
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if buf.len() < 5 + len {
        return Ok(None);
    }
    let _ = buf.split_to(5);
    Ok(Some(buf.split_to(len).to_vec()))
}

/// The end of a length-delimited field starting at `at`: `at + len`, or
/// `truncated` when that runs past `buf`. The addition is checked because
/// `len` is a wire varint: a corrupt or hostile agent stream must read as a
/// refusal, never as arithmetic that overflows.
fn delimited_end(buf_len: usize, at: usize, len: u64, truncated: &str) -> Result<usize, String> {
    let len = usize::try_from(len).map_err(|_| "length_overflow".to_string())?;
    match at.checked_add(len) {
        Some(end) if end <= buf_len => Ok(end),
        _ => Err(truncated.to_string()),
    }
}

fn read_varint(buf: &[u8], mut i: usize) -> Result<(u64, usize), String> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        if i >= buf.len() {
            return Err("truncated_varint".into());
        }
        let b = buf[i];
        i += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok((result, i));
        }
        shift += 7;
        if shift > 63 {
            return Err("varint_overflow".into());
        }
    }
}

/// Step over one field this parser does not read (a future `JWTSVID` field,
/// a claim struct, …), so an added field never breaks the client.
fn skip_field(buf: &[u8], i: &mut usize, wire: u8) -> Result<(), String> {
    match wire {
        0 => {
            let (_, ni) = read_varint(buf, *i)?;
            *i = ni;
            Ok(())
        }
        1 => {
            *i = i.checked_add(8).ok_or("skip_overflow")?;
            if *i > buf.len() {
                return Err("truncated_fixed64".into());
            }
            Ok(())
        }
        2 => {
            let (len, ni) = read_varint(buf, *i)?;
            *i = delimited_end(buf.len(), ni, len, "truncated_len_field")?;
            Ok(())
        }
        5 => {
            *i = i.checked_add(4).ok_or("skip_overflow")?;
            if *i > buf.len() {
                return Err("truncated_fixed32".into());
            }
            Ok(())
        }
        other => Err(format!("unsupported_wire_type_{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    const ANY: &str = "";

    /// `JWTSVIDResponse` carrying one entry, hand-built on the wire.
    fn fetch_response(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut message = Vec::new();
        for (id, token) in entries {
            let mut entry = Vec::new();
            push_length_delimited(1, id.as_bytes(), &mut entry);
            push_length_delimited(2, token.as_bytes(), &mut entry);
            push_length_delimited(1, &entry, &mut message);
        }
        message
    }

    #[test]
    fn the_fetch_request_names_the_audience_and_the_spiffe_id() {
        // JWTSVIDRequest { repeated string audience = 1; string spiffe_id = 2; }
        let id = "spiffe://sylphx.local/role/api";
        let body = encode_jwt_svid_request("openbao-sign", id);
        assert_eq!(body[0], 0x0a);
        assert_eq!(body[1] as usize, "openbao-sign".len());
        assert_eq!(&body[2..14], b"openbao-sign");
        assert_eq!(body[14], 0x12);
        assert_eq!(body[15] as usize, id.len());
        assert_eq!(&body[16..], id.as_bytes());
    }

    #[test]
    fn an_audience_only_fetch_request_carries_no_second_field() {
        // The snapshot client asks for its own workload's SVID: no id pin.
        let body = encode_jwt_svid_request("sylphx-access-snapshot", ANY);
        assert_eq!(body[0], 0x0a);
        assert_eq!(body[1] as usize, "sylphx-access-snapshot".len());
        assert_eq!(&body[2..], b"sylphx-access-snapshot");
        assert_eq!(body.len(), 2 + 22);
    }

    #[test]
    fn a_request_field_over_127_bytes_uses_a_two_byte_length() {
        let long = "a".repeat(200);
        let body = encode_jwt_svid_request(&long, ANY);
        assert_eq!(&body[..3], &[0x0a, 0xc8, 0x01]);
        assert_eq!(body.len(), 3 + 200);
    }

    #[test]
    fn the_validate_request_carries_the_audience_then_the_svid() {
        // ValidateJWTSVIDRequest { string audience = 1; string svid = 2; }
        let body = encode_validate_jwt_svid_request("sylphx-access-snapshot", "a.b.c");
        assert_eq!(body[0], 0x0a);
        assert_eq!(body[1] as usize, "sylphx-access-snapshot".len());
        assert_eq!(&body[2..24], b"sylphx-access-snapshot");
        assert_eq!(body[24], 0x12);
        assert_eq!(body[25], 5);
        assert_eq!(&body[26..], b"a.b.c");
    }

    #[test]
    fn the_fetch_response_yields_the_token_of_the_named_spiffe_id() {
        let id = "spiffe://sylphx.local/role/api";
        let msg = fetch_response(&[(id, "eyJhbGciOiJub25lIn0.body.sig")]);
        assert_eq!(
            decode_jwt_svid_response(&msg, id).unwrap(),
            "eyJhbGciOiJub25lIn0.body.sig"
        );
    }

    #[test]
    fn the_first_token_is_taken_when_no_spiffe_id_is_named() {
        let msg = fetch_response(&[
            (
                "spiffe://sylphx.local/role/platform-registry-consumer",
                "first.tok",
            ),
            ("spiffe://sylphx.local/role/api", "second.tok"),
        ]);
        assert_eq!(decode_jwt_svid_response(&msg, ANY).unwrap(), "first.tok");
        assert_eq!(
            decode_jwt_svid_response(&msg, "spiffe://sylphx.local/role/api").unwrap(),
            "second.tok"
        );
    }

    #[test]
    fn an_entry_without_a_token_does_not_become_an_empty_token() {
        // Only spiffe_id present: no svid. Returning "" here would present an
        // empty bearer token.
        let id = "spiffe://sylphx.local/role/api";
        let mut entry = vec![0x0a, id.len() as u8];
        entry.extend_from_slice(id.as_bytes());
        let mut msg = vec![0x0a, entry.len() as u8];
        msg.extend_from_slice(&entry);
        assert!(decode_jwt_svid_response(&msg, id).is_err());
        assert!(decode_jwt_svid_response(&msg, ANY).is_err());
    }

    #[test]
    fn a_truncated_jwt_svid_response_is_refused() {
        assert!(
            decode_jwt_svid_response(&[0x0a, 0x40, 0x01], "spiffe://sylphx.local/role/api")
                .is_err()
        );
    }

    /// Ten varint bytes that decode to `u64::MAX`, so the addition they drive
    /// would overflow a `usize` before any bound check could refuse it.
    const HUGE_LEN: [u8; 11] = [
        0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
    ];

    #[test]
    fn a_length_that_would_overflow_is_a_refusal_not_a_panic() {
        assert!(decode_jwt_svid_response(&HUGE_LEN, ANY).is_err());
        assert!(decode_jwt_svid_response(&HUGE_LEN, "spiffe://sylphx.local/role/api").is_err());
        assert!(decode_validate_jwt_svid_response(&HUGE_LEN).is_err());
        assert!(decode_jwt_svid_entry(&HUGE_LEN).is_err());
        // The same field length where the parser only steps over it.
        let mut msg = HUGE_LEN.to_vec();
        msg[0] = 0x12; // field 2, wire 2: an unread field of the response
        assert!(decode_jwt_svid_response(&msg, ANY).is_err());
    }

    #[test]
    fn delimited_end_refuses_only_what_runs_past_the_buffer() {
        assert_eq!(delimited_end(10, 2, 8, "trunc").unwrap(), 10);
        assert!(delimited_end(10, 2, 9, "trunc")
            .unwrap_err()
            .contains("trunc"));
        assert!(delimited_end(10, 10, 0, "trunc").is_ok());
        assert!(delimited_end(10, 2, u64::MAX, "trunc").is_err());
        assert!(delimited_end(usize::MAX, usize::MAX, 1, "trunc").is_err());
    }

    #[test]
    fn an_unknown_entry_field_is_skipped_not_fatal() {
        // A future JWTSVID field (4, varint) ahead of the token.
        let id = "spiffe://sylphx.local/role/api";
        let mut entry = vec![0x0a, id.len() as u8];
        entry.extend_from_slice(id.as_bytes());
        entry.extend_from_slice(&[0x20, 0x07]); // field 4, varint, 7
        push_length_delimited(2, b"tok", &mut entry);
        let mut msg = Vec::new();
        push_length_delimited(1, &entry, &mut msg);
        assert_eq!(decode_jwt_svid_response(&msg, id).unwrap(), "tok");
    }

    #[test]
    fn the_validate_response_yields_the_spiffe_id_and_ignores_claims() {
        // ValidateJWTSVIDResponse { string spiffe_id = 1; Struct claims = 2; }
        let id = "spiffe://sylphx.local/role/api";
        let mut msg = Vec::new();
        push_length_delimited(1, id.as_bytes(), &mut msg);
        // claims: a Struct with one string field, plus an unknown field 5.
        let mut claims = Vec::new();
        push_length_delimited(1, b"aud", &mut claims);
        push_length_delimited(2, &claims, &mut msg);
        msg.extend_from_slice(&[0x28, 0x00]); // field 5, varint
        assert_eq!(decode_validate_jwt_svid_response(&msg).unwrap(), id);
    }

    #[test]
    fn a_validate_response_without_a_spiffe_id_is_refused() {
        assert!(decode_validate_jwt_svid_response(&[]).is_err());
        let mut msg = Vec::new();
        let claims = Vec::new();
        push_length_delimited(2, &claims, &mut msg);
        assert!(decode_validate_jwt_svid_response(&msg).is_err());
    }

    #[test]
    fn a_message_is_framed_with_the_compression_flag_and_a_big_endian_length() {
        let framed = grpc_frame(b"abcdef");
        assert_eq!(&framed[..5], &[0, 0, 0, 0, 6]);
        assert_eq!(&framed[5..], b"abcdef");
    }

    #[test]
    fn a_partial_frame_is_waited_for_and_a_whole_one_popped() {
        let framed = grpc_frame(b"abc");
        let mut buf = BytesMut::from(&framed[..4]);
        assert_eq!(try_pop_grpc_message(&mut buf).unwrap(), None);
        buf.extend_from_slice(&framed[4..]);
        assert_eq!(
            try_pop_grpc_message(&mut buf).unwrap(),
            Some(b"abc".to_vec())
        );
        // Two messages in one buffer pop in order, then nothing is left.
        let mut buf = BytesMut::from(&grpc_frame(b"one")[..]);
        buf.extend_from_slice(&grpc_frame(b"two"));
        assert_eq!(
            try_pop_grpc_message(&mut buf).unwrap(),
            Some(b"one".to_vec())
        );
        assert_eq!(
            try_pop_grpc_message(&mut buf).unwrap(),
            Some(b"two".to_vec())
        );
        assert_eq!(try_pop_grpc_message(&mut buf).unwrap(), None);
    }

    #[test]
    fn a_compressed_message_is_refused() {
        let mut framed = grpc_frame(b"abc").to_vec();
        framed[0] = 1;
        let mut buf = BytesMut::from(&framed[..]);
        assert!(try_pop_grpc_message(&mut buf).is_err());
    }

    #[test]
    fn an_absent_audience_is_refused_before_the_socket() {
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fetch_jwt_svid(
                "",
                "spiffe://sylphx.local/role/api",
                Duration::from_millis(100),
            ))
            .unwrap_err();
        assert!(err.contains("audience must be named"), "{err}");
    }

    #[test]
    fn an_absent_token_or_audience_is_refused_before_the_socket() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for (token, audience) in [("", "aud"), ("a.b.c", "")] {
            let err = rt
                .block_on(validate_jwt_svid(
                    token,
                    audience,
                    Duration::from_millis(100),
                ))
                .unwrap_err();
            assert!(err.contains("must be named"), "{err}");
        }
    }

    #[test]
    fn the_jwt_lifetime_is_read_from_the_payload_without_a_signature_check() {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"aud":"sylphx-access-snapshot","iat":1000,"exp":4600}"#);
        let token = format!("header.{payload}.signature");
        assert_eq!(jwt_iat_exp_ms(&token), Some((1_000_000, 4_600_000)));
        // Padded base64url is accepted too.
        let padded = base64::engine::general_purpose::URL_SAFE.encode(br#"{"iat":2,"exp":3}"#);
        assert_eq!(
            jwt_iat_exp_ms(&format!("h.{padded}.s")),
            Some((2_000, 3_000))
        );
        for bad in [
            "",
            "no-dots",
            "header..sig",
            "header.bm90LWpzb24.sig",
            "header.!!!not-base64!!!.sig",
        ] {
            assert_eq!(jwt_iat_exp_ms(bad), None, "{bad:?}");
        }
        let no_exp = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"iat":1}"#);
        assert_eq!(jwt_iat_exp_ms(&format!("h.{no_exp}.s")), None);
    }

    #[test]
    fn an_unmounted_socket_is_named_apart_from_an_outage() {
        // Real OS error for a socket path with no mount behind it.
        let missing =
            std::os::unix::net::UnixStream::connect("/nonexistent/mgmt-jwt/spire-agent.sock")
                .expect_err("a path with no socket must not connect");
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(connect_detail(&missing), SOCKET_ABSENT);
        assert!(is_socket_absent(&connect_detail(&missing)));

        // An agent that is present and refuses is an identity outage, not the
        // expected unmounted-volume shape.
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert!(connect_detail(&refused).starts_with("uds_connect:"));
        assert!(!is_socket_absent(&connect_detail(&refused)));
        assert_eq!(
            SPIFFE_WORKLOAD_SOCKET,
            "/spiffe-workload-api/spire-agent.sock"
        );
        assert!(!is_socket_absent("workload_api_timeout"));
    }
}
