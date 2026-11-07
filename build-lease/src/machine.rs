//! A build machine: the lease request, the wait for READY, a guest token and
//! the judgement of a fault. No I/O besides the Sandboxes calls it is given a
//! client for.

use std::time::{Duration, Instant};

use sylphx::sandboxes as sbx;
use sylphx::Client;

use crate::guest::{Fault, Guest};

/// The lease-owned workspace's mount point; the tree is `WS/tree`.
pub const WS: &str = "/workspace";
pub const TEMPLATE: &str = "template:build";
/// Sync, toolchain install and copy-back on top of the run's timeout.
pub const TTL_SLACK: Duration = Duration::from_secs(30 * 60);
pub const IDLE_TIMEOUT: &str = "600s";
const POLL: Duration = Duration::from_secs(2);
/// Lease tokens live an hour at most; renew well before.
pub const TOKEN_TTL: Duration = Duration::from_secs(3600);

/// The `build-packages` egress preset: no internet package host. Crates and
/// npm tarballs come through the build cache's mirrors (the cache token's
/// `SYLPHX_CRATES_MIRROR` and `SYLPHX_NPM_MIRROR`), toolchains, PyPI and Go
/// modules through its upstream doors, all of which a build lease reaches in
/// the cell; the pinned Rust toolchain is in the lease image. The one name
/// here is the build-cache gateway's public name (a lease on the public
/// network reaches the cache there with its run token; in-cluster leases use
/// the Service). Any other host is the caller's `--allow-host`.
pub const BUILD_PACKAGES: [&str; 1] = [BUILD_CACHE_HOST];
/// The build-cache gateway's public name.
pub const BUILD_CACHE_HOST: &str = "build-cache.sylphx.net";

/// The lease's egress allow-list: the preset, then each extra host.
pub fn allowed_domains(extra: &[String]) -> Vec<String> {
    BUILD_PACKAGES
        .iter()
        .map(|h| h.to_string())
        .chain(extra.iter().cloned())
        .collect()
}

/// The wire form of a lease duration: `spec.ttl` is `"1800s"`.
pub fn wire(d: Duration) -> String {
    format!("{}s", d.as_secs())
}

/// What a build asks of its machine.
pub struct LeaseParams<'a> {
    /// `standard`, `large` or `xlarge`: the lease shape is `build-<size>`.
    pub size: &'a str,
    /// The explicit region, or "" for automatic placement. Neither reserves
    /// a persistent workspace; the admitted Pod owns its workspace.
    pub region: &'a str,
    /// The command's time limit; the lease lives this plus [`TTL_SLACK`].
    pub timeout: Duration,
    /// The egress allow-list, whole: [`allowed_domains`] for a command
    /// build, the caller's own list for another kind of build.
    pub domains: &'a [String],
    /// The caller's own attribution labels (worktree, run id, source), which
    /// land beside `purpose` on the lease's metadata so one held reservation
    /// reads back as whose it is. Sorted keys, validated by the caller.
    pub labels: &'a [(String, String)],
}

/// The request that leases a build machine below `parent` (an environment).
pub fn lease_request(parent: &str, p: &LeaseParams<'_>) -> sbx::CreateLeaseRequest {
    let mut spec = sbx::LeaseSpec::default();
    spec.shape = format!("build-{}", p.size);
    spec.image = TEMPLATE.into();
    spec.region = p.region.to_string();
    spec.kind = Some(sbx::LeaseKind::General);
    spec.ttl = wire((p.timeout + TTL_SLACK).min(Duration::from_secs(24 * 3600)));
    spec.idle_timeout = IDLE_TIMEOUT.into();
    let mut net = sbx::LeaseNetwork::default();
    net.egress = Some(sbx::EgressPolicy::Allowlist);
    net.allowed_domains = p.domains.to_vec();
    spec.network = Some(net);
    let mut meta = sylphx::common::ResourceMeta::default();
    meta.labels.insert("purpose".into(), "build-run".into());
    for (k, v) in p.labels {
        meta.labels.insert(k.clone(), v.clone());
    }
    let mut lease = sbx::Lease::default();
    lease.meta = Some(meta);
    lease.spec = Some(spec);
    let mut req = sbx::CreateLeaseRequest::default();
    req.parent = parent.to_string();
    req.lease = Some(lease);
    req
}

pub fn release_request(name: &str) -> sbx::ReleaseLeaseRequest {
    let mut req = sbx::ReleaseLeaseRequest::default();
    req.name = name.to_string();
    req
}

pub fn state(lease: &sbx::Lease) -> sbx::LeaseState {
    lease
        .status
        .as_ref()
        .and_then(|s| s.state.clone())
        .unwrap_or(sbx::LeaseState::Unknown(String::new()))
}

pub fn why(e: &sylphx::Error) -> String {
    match e {
        sylphx::Error::Api {
            code,
            status,
            detail,
            ..
        } => format!("{} ({status}) {detail}", code.as_str()),
        other => other.to_string(),
    }
}

/// How waiting for a machine ended.
#[derive(Debug)]
pub enum Ready {
    Ready(Box<sbx::Lease>),
    /// `pin` passed with time left: the caller may take another workspace.
    /// The lease was released.
    PinExpired,
    /// The deadline passed. The lease was released.
    QueueTimeout,
    /// The machine was refused (`no_capacity` is retryable).
    Refused {
        why: String,
        retryable: bool,
    },
    /// The machine was preempted or lost before it started: worth one retry.
    Lost(String),
    /// The machine ended some other way before it was ready.
    Ended {
        reason: String,
        retryable: bool,
    },
    /// The lease could not be read.
    Api(sylphx::Error),
}

/// Waits for READY. With `pin`, gives up after that long so the caller can
/// take a fresh workspace.
pub async fn wait_ready(
    client: &Client,
    lease: sbx::Lease,
    pin: Option<Duration>,
    deadline: Instant,
) -> Ready {
    wait_ready_with_progress(client, lease, pin, deadline, |_, _| {}).await
}

/// Waits with a periodic observation of the last public lease snapshot, even
/// while a status request is awaiting its response. Cleanup keeps the original
/// client: an expired GET budget must not prevent releasing the accepted lease.
pub async fn wait_ready_with_progress(
    client: &Client,
    lease: sbx::Lease,
    pin: Option<Duration>,
    deadline: Instant,
    progress: impl FnMut(&sbx::Lease, Duration),
) -> Ready {
    wait_ready_observed(
        client,
        lease,
        pin,
        deadline,
        progress,
        POLL,
        Duration::from_secs(30),
    )
    .await
}

async fn wait_ready_observed(
    client: &Client,
    mut lease: sbx::Lease,
    pin: Option<Duration>,
    deadline: Instant,
    mut progress: impl FnMut(&sbx::Lease, Duration),
    poll: Duration,
    heartbeat: Duration,
) -> Ready {
    let t = Instant::now();
    let bounded = client.with_deadline(deadline);
    let leases = bounded.sandboxes().leases();
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + heartbeat, heartbeat);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        match state(&lease) {
            sbx::LeaseState::Ready => return Ready::Ready(Box::new(lease)),
            sbx::LeaseState::Refused => {
                let why = lease
                    .status
                    .as_ref()
                    .and_then(|s| s.refusal.as_ref())
                    .map(|r| r.as_str().to_string())
                    .unwrap_or_default();
                let retryable = why == "no_capacity";
                return Ready::Refused { why, retryable };
            }
            sbx::LeaseState::Ending | sbx::LeaseState::Ended => {
                let r = lease
                    .status
                    .as_ref()
                    .and_then(|s| s.end_reason.as_ref())
                    .map(|r| r.as_str().to_string())
                    .unwrap_or_default();
                if r == "preempted" || r == "machine_lost" {
                    return Ready::Lost(format!(
                        "the build machine was {} before it started",
                        r.replace('_', " ")
                    ));
                }
                return Ready::Ended {
                    retryable: r == "boot_failed",
                    reason: r,
                };
            }
            _ => {}
        }
        if pin.is_some_and(|p| t.elapsed() >= p) || Instant::now() >= deadline {
            release(client, &lease.name).await;
            if pin.is_some() && Instant::now() < deadline {
                return Ready::PinExpired;
            }
            return Ready::QueueTimeout;
        }
        let mut get = sbx::GetLeaseRequest::default();
        get.name = lease.name.clone();
        let read = async {
            tokio::time::sleep(poll.min(deadline.saturating_duration_since(Instant::now()))).await;
            leases.get(get).await
        };
        tokio::pin!(read);
        let result = loop {
            tokio::select! {
                result = &mut read => break result,
                _ = ticks.tick() => progress(&lease, t.elapsed()),
            }
        };
        lease = match result {
            Ok(l) => l,
            Err(_) if Instant::now() >= deadline => {
                release(client, &lease.name).await;
                return Ready::QueueTimeout;
            }
            Err(e) => return Ready::Api(e),
        };
    }
}

/// Releases a lease; `false` when it was not released (its idle timeout ends it).
pub async fn release(client: &Client, name: &str) -> bool {
    let r = tokio::time::timeout(
        Duration::from_secs(30),
        client.sandboxes().leases().release(release_request(name)),
    )
    .await;
    matches!(r, Ok(Ok(_)))
}

/// A guest-scoped lease token.
pub async fn mint_token(client: &Client, lease: &str) -> Result<String, Fault> {
    let mut req = sbx::MintLeaseTokenRequest::default();
    req.name = lease.to_string();
    req.scopes = vec![sbx::TokenScope::Guest];
    req.ttl = wire(TOKEN_TTL);
    client
        .sandboxes()
        .leases()
        .mint_token(req)
        .await
        .map(|r| r.token)
        .map_err(|e| Fault::Other(format!("minting a guest token: {}", why(&e))))
}

/// The guest of a ready lease.
pub async fn guest(client: &Client, lease: &sbx::Lease) -> Result<Guest, Fault> {
    let ep = lease
        .status
        .as_ref()
        .and_then(|s| s.endpoints.clone())
        .unwrap_or_default();
    if ep.guest_uri.is_empty() || ep.e2b_sandbox_id.is_empty() {
        return Err(Fault::Other(
            "the build machine has no guest endpoint".into(),
        ));
    }
    let token = mint_token(client, &lease.name).await?;
    Guest::new(&ep.guest_uri, &ep.e2b_sandbox_id, &token).map_err(Fault::Other)
}

/// What a guest fault means for the run.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Worth one more try on a new machine.
    Retry(String),
    Fail {
        reason: String,
        retryable: bool,
    },
}

/// `end_reason` is why the lease ended, when it has.
pub fn judge(end_reason: Option<&str>, f: &Fault) -> Verdict {
    match end_reason {
        Some(r @ ("preempted" | "machine_lost")) => {
            Verdict::Retry(format!("the build machine was {}", r.replace('_', " ")))
        }
        Some(r) => Verdict::Fail {
            reason: format!("the build machine ended: {r}"),
            retryable: false,
        },
        None => match f {
            Fault::Stream(t) => Verdict::Retry(t.clone()),
            other => Verdict::Fail {
                reason: other.text().to_string(),
                retryable: true,
            },
        },
    }
}

/// The reason for a non-zero provisioning status, with the last lines it
/// wrote to stderr.
pub fn provision_failure(code: i32, stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let tail: String = text
        .trim()
        .chars()
        .rev()
        .take(400)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() {
        format!("preparing the build machine failed (status {code})")
    } else {
        format!("preparing the build machine failed (status {code}): {tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_egress_is_the_cache_gateway_then_the_extra_hosts() {
        assert_eq!(allowed_domains(&[]), ["build-cache.sylphx.net"]);
        let d = allowed_domains(&["example.org".into()]);
        assert_eq!(d, ["build-cache.sylphx.net", "example.org"]);
    }

    #[test]
    fn the_lease_is_a_build_shape_without_a_workspace_reservation() {
        let hosts = allowed_domains(&[]);
        let p = LeaseParams {
            size: "large",
            region: "",
            timeout: Duration::from_secs(60),
            domains: &hosts,
            labels: &[],
        };
        let r = lease_request("orgs/o/projects/p/envs/e", &p);
        let spec = r.lease.unwrap().spec.unwrap();
        assert_eq!(spec.shape, "build-large");
        assert!(spec.volumes.is_empty());
        assert_eq!(spec.ttl, format!("{}s", 60 + TTL_SLACK.as_secs()));
        let p = LeaseParams { region: "hk", ..p };
        let spec = lease_request("x", &p).lease.unwrap().spec.unwrap();
        assert_eq!(spec.region, "hk");
        assert!(spec.volumes.is_empty());
    }

    #[test]
    fn the_callers_labels_land_beside_the_purpose_label() {
        let hosts = allowed_domains(&[]);
        let labels = [
            ("worktree".to_string(), "w-22292".to_string()),
            ("source".to_string(), "sylphxai-cloud".to_string()),
        ];
        let p = LeaseParams {
            size: "standard",
            region: "",
            timeout: Duration::from_secs(60),
            domains: &hosts,
            labels: &labels,
        };
        let meta = lease_request("x", &p).lease.unwrap().meta.unwrap();
        assert_eq!(meta.labels["purpose"], "build-run");
        assert_eq!(meta.labels["worktree"], "w-22292");
        assert_eq!(meta.labels["source"], "sylphxai-cloud");
    }

    #[tokio::test]
    async fn silent_readiness_poll_obeys_budget_and_releases_with_original_client() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        async fn headers(socket: &mut tokio::net::TcpStream) -> String {
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
                bytes.push(byte[0]);
            }
            String::from_utf8(bytes).unwrap()
        }
        let server = tokio::spawn(async move {
            let (mut silent, _) = listener.accept().await.unwrap();
            let get = headers(&mut silent).await;
            assert!(
                get.starts_with("GET /v1/orgs/o/projects/p/envs/e/leases/sbx-budget "),
                "{get}"
            );
            // Keep the GET unanswered; cleanup must open a distinct request
            // after its deadline, rather than using the expired GET client.
            let (mut cleanup, _) = listener.accept().await.unwrap();
            let release = headers(&mut cleanup).await;
            assert!(
                release.starts_with("POST /v1/orgs/o/projects/p/envs/e/leases/sbx-budget:release "),
                "{release}"
            );
            let body = r#"{"name":"orgs/o/projects/p/envs/e/leases/sbx-budget"}"#;
            cleanup.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            (get, release)
        });
        let client = Client::new(
            sylphx::HttpTransport::builder()
                .base_url(url)
                .api_key("sylphx_sk_test")
                .max_retries(0)
                .build()
                .unwrap(),
        );
        let mut lease = sbx::Lease::default();
        lease.name = "orgs/o/projects/p/envs/e/leases/sbx-budget".into();
        let mut status = sbx::LeaseStatus::default();
        status.state = Some(sbx::LeaseState::Granted);
        lease.status = Some(status);
        let mut observed = Vec::new();
        let start = Instant::now();
        let ready = tokio::time::timeout(
            Duration::from_secs(3),
            wait_ready_observed(
                &client,
                lease,
                None,
                start + Duration::from_millis(250),
                |lease, elapsed| observed.push((lease.name.clone(), elapsed)),
                Duration::from_millis(1),
                Duration::from_millis(40),
            ),
        )
        .await
        .expect("silent poll exceeded its queue budget");
        assert!(matches!(ready, Ready::QueueTimeout), "{ready:?}");
        assert!(
            !observed.is_empty(),
            "heartbeat must run while GET is silent"
        );
        assert!(observed
            .iter()
            .all(|(name, _)| name.ends_with("sbx-budget")));
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn faults_are_judged_retry_or_fail() {
        let stream = Fault::Stream("the process stream broke".into());
        assert_eq!(
            judge(None, &stream),
            Verdict::Retry("the process stream broke".into())
        );
        for r in ["preempted", "machine_lost"] {
            assert!(matches!(
                judge(Some(r), &Fault::Gone("x".into())),
                Verdict::Retry(_)
            ));
        }
        // The machine ended for another reason: no retry, even on a broken stream.
        assert!(matches!(
            judge(Some("idle"), &stream),
            Verdict::Fail {
                retryable: false,
                ..
            }
        ));
        for f in [
            Fault::Gone("guest unreachable".into()),
            Fault::Other("denied".into()),
        ] {
            assert!(matches!(
                judge(None, &f),
                Verdict::Fail {
                    retryable: true,
                    ..
                }
            ));
        }
    }
}
