//! Publication metadata comes from the registry, not the independent OCI
//! exporter: their provenance can give the two indexes different digests.
use std::time::Duration;

use reqwest::{header::HeaderMap, StatusCode};
use serde_json::Value;

use super::{is_digest, ImageDescriptor, MANAGED_REGISTRY_HOST};

const MEDIA_TYPES: &[&str] = &[
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

/// Confirm the exact pushed manifest with the build's existing credential.
/// HEAD has no JSON body. Credentials and registry bodies never enter errors.
pub async fn registry_descriptor(source_ref: &str, auth: &[u8]) -> Result<ImageDescriptor, String> {
    let (repository, digest) = source_ref
        .split_once('@')
        .ok_or("the publication has no digest")?;
    let repository = repository
        .strip_prefix(&format!("{MANAGED_REGISTRY_HOST}/"))
        .filter(|r| !r.is_empty())
        .ok_or("the publication is outside the managed registry")?;
    if !is_digest(digest) {
        return Err("the publication digest is not canonical".into());
    }
    let auth: Value = serde_json::from_slice(auth)
        .map_err(|_| "the build's registry credentials are not JSON")?;
    let token = auth["auths"][MANAGED_REGISTRY_HOST]["identitytoken"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("the build's managed registry credential is missing")?;
    let url = format!("https://{MANAGED_REGISTRY_HOST}/v2/{repository}/manifests/{digest}");
    head_descriptor(&url, digest, token).await
}

async fn head_descriptor(url: &str, digest: &str, token: &str) -> Result<ImageDescriptor, String> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| "creating the registry client failed")?;
    for attempt in 0..3 {
        let response = http
            .head(url)
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, MEDIA_TYPES.join(", "))
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    format!("reading registry descriptor for {digest}: request timed out")
                } else {
                    format!(
                        "reading registry descriptor for {digest}: {}",
                        e.without_url()
                    )
                }
            })?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND && attempt < 2 {
            tokio::time::sleep(Duration::from_millis(200 * (attempt + 1))).await;
            continue;
        }
        if status != StatusCode::OK {
            return Err(format!(
                "reading registry descriptor for {digest}: HTTP {status}"
            ));
        }
        return descriptor(response.headers(), digest);
    }
    unreachable!("the final attempt returns its result")
}

fn descriptor(headers: &HeaderMap, digest: &str) -> Result<ImageDescriptor, String> {
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    if header("docker-content-digest") != Some(digest) {
        return Err(format!(
            "the registry did not confirm {digest}: Docker-Content-Digest is missing or differs"
        ));
    }
    let media_type = header("content-type")
        .and_then(|v| v.split(';').next())
        .map(str::trim)
        .filter(|v| MEDIA_TYPES.contains(v))
        .ok_or_else(|| {
            format!("the registry reported no supported image media type for {digest}")
        })?;
    let size_bytes = header("content-length")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| format!("the registry reported no positive manifest size for {digest}"))?;
    Ok(ImageDescriptor {
        digest: digest.into(),
        media_type: media_type.into(),
        size_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const DIGEST: &str = "sha256:5ae53b82b2e240ed452023afbec6b3b9bf70ff547016c8aac06f11ffe95cfd30";

    // A real HEAD peer sends headers and no body, even with Content-Length.
    // This also exercises the 404 window without retrying any other refusal.
    async fn serve(statuses: &[u16], headers: &str) -> Result<ImageDescriptor, String> {
        serve_digest(statuses, headers, DIGEST).await
    }

    async fn serve_digest(
        statuses: &[u16],
        headers: &str,
        digest: &str,
    ) -> Result<ImageDescriptor, String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v2/org/project-image/manifests/{digest}",
            listener.local_addr().unwrap()
        );
        let statuses = statuses.to_vec();
        let headers = headers.to_string();
        let expected_digest = digest.to_string();
        let server = tokio::spawn(async move {
            for status in statuses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with(&format!(
                    "HEAD /v2/org/project-image/manifests/{expected_digest} "
                )));
                assert!(request
                    .to_lowercase()
                    .contains("authorization: bearer private-test-token\r\n"));
                assert!(request.contains(MEDIA_TYPES[0]));
                socket
                    .write_all(
                        format!("HTTP/1.1 {status} X\r\n{headers}connection: close\r\n\r\n")
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let result = head_descriptor(&url, digest, "private-test-token").await;
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        result
    }

    fn headers() -> String {
        format!("Docker-Content-Digest: {DIGEST}\r\nContent-Type: {}; charset=utf-8\r\nContent-Length: 855\r\n", MEDIA_TYPES[0])
    }

    #[tokio::test]
    async fn published_descriptor_uses_head_headers_without_an_archive_or_json_body() {
        let got = serve(&[200], &headers()).await.unwrap();
        assert_eq!(got.digest, DIGEST);
        assert_eq!(got.media_type, MEDIA_TYPES[0]);
        assert_eq!(got.size_bytes, 855);
    }

    #[tokio::test]
    async fn an_independent_exporter_index_need_not_contain_the_published_digest() {
        // Captured from lease sbx-1jy953xz: both exporters succeeded, but
        // the archive has only the OCI exporter's independently attested index.
        const OCI: &str = "sha256:eb0ebd350cc8045eecebdcf2af78ea3e10dd4fb5b775bedf2b75ba735656a64e";
        const PUSHED: &str =
            "sha256:c2c3c61093d21ebc66999a8839173b5cd4f901fc42b9c5b966ea07541c97f416";
        let archive_index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{"digest": OCI, "mediaType": MEDIA_TYPES[0], "size": 856}],
        });
        assert!(!archive_index["manifests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|manifest| manifest["digest"] == PUSHED));

        // No archive or JSON body is supplied to the production reader.
        // Its authoritative descriptor is the exact pushed digest's HEAD.
        let published_headers = headers().replace(DIGEST, PUSHED);
        let got = serve_digest(&[200], &published_headers, PUSHED)
            .await
            .unwrap();
        assert_eq!(got.digest, PUSHED);
        assert_eq!(got.media_type, MEDIA_TYPES[0]);
        assert_eq!(got.size_bytes, 855);

        // Substituting the archive's descriptor must still fail closed.
        let wrong_headers = published_headers.replace(PUSHED, OCI);
        let err = serve_digest(&[200], &wrong_headers, PUSHED)
            .await
            .unwrap_err();
        assert!(err.contains("Docker-Content-Digest is missing or differs"));
        assert!(err.contains(PUSHED));
    }

    #[tokio::test]
    async fn a_late_manifest_is_retried_but_not_found_is_bounded() {
        assert!(serve(&[404, 404, 200], &headers()).await.is_ok());
        let err = serve(&[404, 404, 404], "").await.unwrap_err();
        assert!(err.contains("404") && err.contains(DIGEST), "{err}");
    }

    #[tokio::test]
    async fn refusals_and_redirects_fail_once_without_leaking_credentials() {
        for status in [401, 403, 429, 500, 302] {
            let err = serve(&[status], "").await.unwrap_err();
            assert!(
                err.contains(&status.to_string()) && err.contains(DIGEST),
                "{err}"
            );
            assert!(!err.contains("private-test-token"));
        }
    }

    #[tokio::test]
    async fn mismatched_and_incomplete_descriptors_fail_without_retry() {
        for invalid in [
            String::new(),
            headers().replace(
                DIGEST,
                "sha256:6b8745635e8c0e0ac81cdd43ac467d885391c952c541dbd941705024f0b8475b",
            ),
            headers().replace("Content-Length: 855\r\n", ""),
            headers().replace("855", "0"),
            headers().replace("855", "-1"),
            headers().replace("855", "9223372036854775808"),
            headers().replace(MEDIA_TYPES[0], "text/html"),
        ] {
            let err = serve(&[200], &invalid).await.unwrap_err();
            assert!(err.contains(DIGEST), "{err}");
        }
    }
}
