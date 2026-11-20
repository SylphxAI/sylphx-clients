//! Build machine lease acquisition, source-auth/cache wiring, and fault classification.

use super::*;

impl Run<'_> {
    /// The accepted Cell, otherwise the explicit region or automatic placement.
    pub(super) fn region(&self) -> &str {
        match self.accepted_region.as_deref() {
            // An empty CreateLease region would select automatically again.
            Some("") => "fsn1",
            Some(region) => region,
            None => self.o.region.as_deref().unwrap_or(""),
        }
    }

    /// " in region `<r>`" when a region was asked for.
    pub(super) fn where_(&self) -> String {
        match &self.o.region {
            Some(r) => format!(" in region {r}"),
            None => String::new(),
        }
    }

    pub(super) fn add(&self, f: impl FnOnce(&mut Stats)) {
        f(&mut self.stats.lock().expect("not poisoned"));
    }

    pub(super) async fn attempt(&mut self) -> Attempt {
        let warned_before =
            self.cache_warned.load(Ordering::Relaxed) || self.source_warned.load(Ordering::Relaxed);
        let mut m = match self.acquire().await {
            Ok(m) => m,
            Err(a) => return a,
        };
        let name = m.lease.name.clone();
        let mut guest = match m.guest.take() {
            Some(g) => g,
            None => match self.guest(&m.lease).await {
                Ok(g) => g,
                Err(e) => return self.lost_or(&name, e).await,
            },
        };
        let mut minted = Instant::now();
        // Keep request phases ordered on every attempt: a failed workspace
        // upload must be classified before dependency uploads start, and the
        // next accepted lease must mint its own source grants.
        let mut sources = match self.source(&m.lease).await {
            Ok(source) => source,
            Err(e) => return Attempt::Done(Outcome::platform(e, false)),
        };
        match self.sync(&guest, m.store.as_ref()).await {
            Ok(()) => {}
            Err(e) if workspace_full(e.text()) => return self.out_of_space(&m).await,
            Err(e) => return self.lost_or(&name, e).await,
        }
        let (mut git_env, mut setup) = match self.send_git_deps(&guest, &sources.repositories).await
        {
            Ok(e) => e,
            Err(e) if out_of_space(e.text()) => return self.out_of_space(&m).await,
            Err(e) => return self.lost_or(&name, e).await,
        };
        build_source::merge_env(&mut git_env, std::mem::take(&mut sources.env));
        match self.send_source_history(&guest).await {
            Ok(script) => setup.push_str(&script),
            Err(e) => return self.lost_or(&name, e).await,
        }
        let shape = format!("build-{}", self.o.size);
        let warned = !warned_before
            && (self.cache_warned.load(Ordering::Relaxed)
                || self.source_warned.load(Ordering::Relaxed));
        // A warning takes the place of this progress line.
        if !warned {
            self.out.progress(&format!(
                "running on {shape}, cold lease-owned workspace{}",
                self.where_()
            ));
        }
        if let Err(e) = self.send_auth(&guest).await {
            return self.lost_or(&name, e).await;
        }
        if self
            .held
            .with(|h| h.publish.as_ref().is_some_and(|p| p.auth))
        {
            setup.push_str(&shell_call(&["chmod", "600", &image::auth_path()]));
        }
        if !sources.repositories.is_empty() {
            let bytes = match build_source::take_headers(&mut git_env, SOURCE_AUTH_PATH) {
                Ok(bytes) => bytes,
                Err(_) => {
                    return Attempt::Done(Outcome::platform(
                        "source session returned invalid Git settings",
                        false,
                    ))
                }
            };
            if let Err(error) = self.write_source_headers(&guest, &bytes).await {
                return self.lost_or(&name, error).await;
            }
        }
        sources.env = git_env;
        let has_sources = !sources.repositories.is_empty();
        let ran = self
            .exec(&mut guest, &mut minted, &name, m.cache, sources, &setup)
            .await;
        // Read the registry with this build's credential before removing it.
        // Keep the result until cleanup finishes, including on a refusal.
        let image_result = if self.o.image.is_some() && matches!(&ran, Ok(Some(0))) {
            self.collect_image(&guest, &name).await
        } else {
            Ok(())
        };
        self.drop_auth(&guest).await;
        if has_sources {
            let none = BTreeMap::new();
            let _ = guest
                .run(
                    USER,
                    &argv(&[
                        "/bin/sh",
                        "-c",
                        "rm -rf -- \"$1\"",
                        "source-auth-cleanup",
                        SOURCE_AUTH_DIR,
                    ]),
                    &none,
                )
                .await;
        }
        let code = match ran {
            Ok(Some(c)) => c,
            Ok(None) => return Attempt::Done(Outcome::TimedOut),
            Err(ExecFault::Before(fault)) => return self.lost_or(&name, fault).await,
            Err(ExecFault::Setup(fault)) => {
                return Attempt::Done(Outcome::platform(fault.text().to_string(), true))
            }
            Err(ExecFault::Launched(fault)) => return self.lost_after_start(&name, fault).await,
        };
        if let Err(o) = image_result {
            return Attempt::Done(o);
        }
        if !self.o.artifacts.is_empty() {
            if let Err(e) = self.collect(&guest, &name).await {
                return Attempt::Done(Outcome::platform(
                    format!("copying artifacts back: {e}"),
                    true,
                ));
            }
        }
        Attempt::Done(Outcome::Ran(code))
    }

    /// Mint through the public gateway using this caller only. Missing public
    /// grants retain the caller's existing synced dependency mirrors.
    pub(super) async fn source(&self, lease: &sbx::Lease) -> Result<Sources, String> {
        let mut sources = Sources {
            env: BTreeMap::new(),
            repositories: BTreeSet::new(),
            network: build_cache::Network::Public,
            expires_at: u64::MAX,
        };
        if self.o.no_cache || build_source::overridden(&self.o.env) {
            return Ok(sources);
        }
        let (Some(key), Some(project)) = (self.key, build_cache::project_id(&self.parent)) else {
            return Ok(sources);
        };
        let network =
            build_cache::run_request(project.clone(), self.o.timeout, Duration::ZERO, lease)
                .network;
        sources.network = network;
        let mut repositories: BTreeSet<String> = self
            .git_deps
            .iter()
            .filter_map(|(dep, _)| build_source::repository(&dep.url))
            .collect();
        if let Some(origin) = build_source::origin(&self.o.root) {
            repositories.insert(origin);
        }
        let minted = pipeline(repositories, |repository| {
            let project = &project;
            async move {
                let minted =
                    build_source::mint(&self.cache_url, key, project, &repository, network, None)
                        .await;
                if let Ok(minted) = &minted {
                    self.remember_source(minted);
                }
                (repository, minted)
            }
        })
        .await;
        for (repository, minted) in minted {
            match minted {
                Ok(minted) => {
                    sources.expires_at = sources.expires_at.min(minted.expires_at.unwrap_or(0));
                    build_source::merge_env(&mut sources.env, minted.env);
                    sources.repositories.insert(repository);
                }
                // A missing grant applies only to this repository, not to
                // sessions already granted for the build's other sources.
                Err(build_cache::MintError::Status {
                    status: 403,
                    code: Some(code),
                    ..
                }) if code == "SOURCE_GRANT_MISSING" => {}
                Err(error)
                    if matches!(
                        &error,
                        build_cache::MintError::TimedOut
                            | build_cache::MintError::Unreachable
                            | build_cache::MintError::Status {
                                status: 401 | 403 | 404 | 500..=599,
                                ..
                            }
                    ) =>
                {
                    if !self.source_warned.swap(true, Ordering::Relaxed) {
                        eprintln!("sylphx: warning: source cache unavailable ({}); using the caller’s existing source inputs", error.diagnostic());
                    }
                    sources.env.clear();
                    sources.repositories.clear();
                    sources.expires_at = u64::MAX;
                    return Ok(sources);
                }
                Err(e) => return Err(format!("source session unavailable ({})", e.diagnostic())),
            }
        }
        Ok(sources)
    }

    pub(super) fn remember_source(&self, minted: &build_cache::Minted) {
        self.held.with(|h| {
            h.secrets.extend(minted.secrets());
            h.output_secrets.extend(minted.token.iter().cloned());
            h.output_secrets.extend(
                minted
                    .env
                    .values()
                    .find(|v| v.starts_with("Authorization:"))
                    .cloned(),
            );
        });
    }

    pub(super) async fn write_source_headers(
        &self,
        guest: &Guest,
        bytes: &[u8],
    ) -> Result<(), Fault> {
        // Carry credentials only in the process environment, never argv or
        // output. One action creates a private temporary file and publishes it
        // atomically, including on renewal.
        let env = BTreeMap::from([(
            "SYLPHX_SOURCE_HEADERS".to_string(),
            String::from_utf8(bytes.to_vec())
                .map_err(|_| Fault::Other("invalid source credentials".into()))?,
        )]);
        let publish = argv(&[
            "/bin/sh",
            "-c",
            SOURCE_AUTH_WRITE,
            "source-auth",
            SOURCE_AUTH_DIR,
            SOURCE_AUTH_PATH,
        ]);
        let (code, _, _) = guest.run(USER, &publish, &env).await?;
        if code != 0 {
            return Err(Fault::Other(
                "publishing private source credentials failed".into(),
            ));
        }
        Ok(())
    }

    pub(super) async fn renew_sources(
        &self,
        guest: &Guest,
        sources: &mut Sources,
    ) -> Result<(), Fault> {
        let key = self
            .key
            .ok_or_else(|| Fault::Other("source renewal has no caller identity".into()))?;
        let project = build_cache::project_id(&self.parent)
            .ok_or_else(|| Fault::Other("source renewal has no project".into()))?;
        let mut env = BTreeMap::new();
        let mut expires_at = u64::MAX;
        for repository in &sources.repositories {
            let minted = build_source::mint(
                &self.cache_url,
                key,
                &project,
                repository,
                sources.network,
                None,
            )
            .await
            .map_err(|error| {
                Fault::Other(format!(
                    "source session renewal failed ({})",
                    error.diagnostic()
                ))
            })?;
            self.remember_source(&minted);
            expires_at = expires_at.min(
                minted
                    .expires_at
                    .ok_or_else(|| Fault::Other("source renewal has no expiry".into()))?,
            );
            build_source::merge_env(&mut env, minted.env);
        }
        let bytes = build_source::take_headers(&mut env, SOURCE_AUTH_PATH)
            .map_err(|_| Fault::Other("source renewal returned invalid Git settings".into()))?;
        self.write_source_headers(guest, &bytes).await?;
        sources.expires_at = expires_at;
        Ok(())
    }

    /// The cache environment for the command, minted before the sync (its
    /// lifetime is the run's timeout plus slack, which covers the sync). Empty
    /// with `--no-cache`, and when no token
    /// can be had (a cache that is missing only slows the build). The build
    /// store, when the token reaches it, takes the sync's files.
    pub(super) async fn cache(
        &self,
        lease: &sbx::Lease,
        queue_timeout: Duration,
    ) -> (BTreeMap<String, String>, Option<store::Store>) {
        if self.o.no_cache {
            return (BTreeMap::new(), None);
        }
        let minted = match (self.key, build_cache::project_id(&self.parent)) {
            (Some(key), Some(project)) => {
                let req = build_cache::run_request(project, self.o.timeout, queue_timeout, lease);
                build_cache::mint(&self.cache_url, key, &req).await
            }
            (None, _) => Err(build_cache::MintError::NoProject("not signed in".into())),
            (_, None) => Err(build_cache::MintError::NoProject(
                "the environment names no project".into(),
            )),
        };
        match minted {
            Ok(m) => {
                self.held.with(|h| {
                    h.secrets.extend(m.secrets());
                    h.output_secrets.extend(m.token.iter().cloned());
                });
                // The gateway as the build machine reaches it.
                let guest_base = m.env.get("TURBO_API").cloned().unwrap_or_default();
                let st = m.token.as_deref().and_then(|t| {
                    store::Store::new(
                        &self.cache_url,
                        &guest_base,
                        m.cache.as_deref().unwrap_or(""),
                        t,
                    )
                });
                (m.env, st)
            }
            Err(e) => {
                let first = !self.cache_warned.swap(true, Ordering::Relaxed);
                if first {
                    eprintln!(
                        "sylphx: warning: build cache unavailable ({}); building without it",
                        e.short()
                    );
                }
                (BTreeMap::new(), None)
            }
        }
    }

    /// The tree directory the command runs in, relative to the tree root.
    pub(super) fn rel(&self) -> &str {
        if self.o.rel.is_empty() {
            "."
        } else {
            &self.o.rel
        }
    }

    /// A guest fault means the machine may be gone: if the lease ended
    /// preempted or lost, or the process stream broke, the run is worth one
    /// retry; otherwise it is a platform failure.
    pub(super) async fn lost_or(&self, name: &str, f: Fault) -> Attempt {
        let mut get = sbx::GetLeaseRequest::default();
        get.name = name.to_string();
        let lease = self.client.sandboxes().leases().get(get).await.ok();
        let reason = lease
            .as_ref()
            .and_then(|l| l.status.as_ref())
            .and_then(|s| s.end_reason.as_ref())
            .map(|r| r.as_str().to_string());
        let ended = lease
            .as_ref()
            .map(|l| matches!(state(l), sbx::LeaseState::Ending | sbx::LeaseState::Ended))
            .unwrap_or(false);
        let verdict = judge(reason.as_deref(), &f);
        // A retry takes a new machine: end this one first, or it idles on.
        let release_now = !ended && matches!(verdict, Verdict::Retry(_));
        self.held.with(|h| {
            h.proc = None;
            if ended || release_now {
                h.lease = None;
            }
        });
        if release_now {
            release(self.client, name).await;
        }
        match verdict {
            Verdict::Retry(why) => Attempt::Lost(why),
            Verdict::Fail { reason, retryable } => {
                Attempt::Done(Outcome::platform(reason, retryable))
            }
        }
    }

    /// Retry once before the command starts on another fresh lease-owned
    /// workspace. A second failure is 125, as any loss before the start.
    pub(super) async fn out_of_space(&mut self, m: &Machine) -> Attempt {
        let name = &m.lease.name;
        self.held.with(|h| {
            h.proc = None;
            h.lease = None;
        });
        release(self.client, name).await;
        Attempt::Lost("the workspace ran out of space".into())
    }

    /// The command started and the run lost it: the machine ended, or the
    /// command cannot be followed. It is never run again (it may have done
    /// part of its work); the run ends as a killed process would.
    pub(super) async fn lost_after_start(&self, name: &str, f: Fault) -> Attempt {
        let mut get = sbx::GetLeaseRequest::default();
        get.name = name.to_string();
        let lease = self.client.sandboxes().leases().get(get).await.ok();
        let reason = lease
            .as_ref()
            .and_then(|l| l.status.as_ref())
            .and_then(|s| s.end_reason.as_ref())
            .map(|r| format!("the build machine ended: {}", r.as_str()))
            .unwrap_or_else(|| f.text().to_string());
        self.held.with(|h| h.proc = None);
        Attempt::Done(Outcome::Lost(reason))
    }

    /// Lease first; the workload creates its cold workspace only after
    /// admission. Explicit regions pin placement, never a workspace Volume.
    ///
    /// The queue budget starts here, before the lease is created, and is the
    /// SDK client's shared deadline: the SDK's own retries of a retryable
    /// answer (a stale access snapshot answering 503, say) are spent inside
    /// it, and when they end — at its retry limit or at the deadline — the
    /// server's own answer is returned, message intact. No new logical call
    /// is made here: a fresh create would carry a fresh idempotency key and
    /// could duplicate the machine.
    pub(super) async fn acquire(&mut self) -> Result<Machine, Attempt> {
        let deadline = Instant::now() + self.o.queue_timeout;
        let bounded = self.client.with_deadline(deadline);
        let lease = self.lease(&bounded).await.map_err(Attempt::Done)?;
        // The lease's own labels, as the platform holds them: the run's
        // `worktree`/`source` and the caller's `--label` beside `purpose`.
        let labels = lease
            .meta
            .as_ref()
            .map(|m| m.labels.clone())
            .unwrap_or_default();
        self.out.event(
            json!({"type": "queued", "lease": lease.name, "size": self.o.size, "region": lease.spec.as_ref().map(|s| s.region.as_str()).unwrap_or(self.region()), "volume": false, "labels": labels}),
        );
        // Cache credentials depend on accepted placement, not readiness.
        // Mint while the machine is admitted/booted, through the public cache API.
        let cache_started = Instant::now();
        let (ready, (mut cache, mut store), guest) = tokio::join!(
            self.ready(self.client, lease.clone(), deadline),
            self.cache(&lease, self.o.queue_timeout),
            async {
                // A queued lease cannot mint a token. A warm READY answer can
                // mint in parallel with the cache instead of serializing it.
                if state(&lease) == sbx::LeaseState::Ready {
                    self.guest(&lease).await.map(Some)
                } else {
                    Ok(None)
                }
            },
        );
        let lease = ready?;
        // Only a capped token can lose its setup/command lifetime in admission.
        // Refresh that exceptional path, never serialize normal READY setup.
        if !cache.is_empty()
            && build_cache::run_needs_refresh(
                self.o.timeout,
                self.o.queue_timeout,
                cache_started.elapsed(),
            )
        {
            (cache, store) = self.cache(&lease, Duration::ZERO).await;
        }
        let guest = match guest {
            Ok(g) => g,
            Err(e) => return Err(self.lost_or(&lease.name, e).await),
        };
        Ok(Machine {
            lease,
            cache,
            store,
            guest,
        })
    }

    /// Create a Build lease with no pre-created or attached workspace Volume.
    pub(super) async fn lease(&mut self, client: &Client) -> Result<sbx::Lease, Outcome> {
        let domains = lease_domains(self.o);
        // The run's own worktree and repository name every reservation it
        // holds, so a held lease reads back as whose it is; the caller's
        // `--label` values apply on top. A `--no-tree` run has no work tree.
        let labels = if self.o.no_tree {
            self.o.labels.clone()
        } else {
            labels::merge(labels::defaults(&self.o.root), self.o.labels.clone())
        };
        let req = lease_request(
            &self.parent,
            &LeaseParams {
                size: &self.o.size,
                region: self.region(),
                timeout: self.o.timeout,
                domains: &domains,
                labels: &labels,
            },
        );
        let lease = client
            .sandboxes()
            .leases()
            .create(req)
            .await
            .map_err(|e| match &e {
                // The SDK observes this accepted request with the same key
                // until its result is available. A pending answer escapes
                // only when another poll would exceed the queue budget.
                sylphx::Error::Api { code, status: 409, retryable: true, .. }
                    if code.as_str() == "IDEMPOTENCY_IN_PROGRESS" => Outcome::platform(
                        format!(
                            "no build machine{} within --queue-timeout {}s (the accepted request is still running)",
                            self.where_(), self.o.queue_timeout.as_secs()
                        ),
                        true,
                    ),
                _ => api("creating the build machine", &e),
            })?;
        if self.accepted_region.is_none() {
            self.accepted_region = lease.spec.as_ref().map(|s| s.region.clone());
        }
        self.held.with(|h| h.lease = Some(lease.name.clone()));
        self.add(|stats| stats.lease = Some(lease.name.clone()));
        self.out
            .progress(&with_lease("accepted build machine", Some(&lease.name)));
        Ok(lease)
    }

    /// Wait for admission and READY within the caller's queue budget.
    pub(super) async fn ready(
        &self,
        client: &Client,
        lease: sbx::Lease,
        deadline: Instant,
    ) -> Result<sbx::Lease, Attempt> {
        // The create already returned this resource. A failure before that
        // has no name; say so instead of printing an empty one.
        let label = lease_label(&lease.name);
        match sylphx_build_lease::machine::wait_ready_with_progress(
            client,
            lease.clone(),
            None,
            deadline,
            |lease, elapsed| self.out.progress(&queue_progress(lease, elapsed)),
        )
        .await
        {
            Ready::Ready(l) => Ok(*l),
            // No workspace node pin is requested by this caller.
            Ready::PinExpired => unreachable!("volume-free Build leases have no node pin"),
            Ready::QueueTimeout => {
                // The GET budget expired; the original cleanup client released
                // the lease. Only forget it here.
                self.held.with(|h| h.lease = None);
                Err(Attempt::Done(Outcome::platform(
                    format!(
                        "no build machine{} within --queue-timeout {}s ({label})",
                        self.where_(),
                        self.o.queue_timeout.as_secs()
                    ),
                    true,
                )))
            }
            Ready::Refused { why, retryable } => {
                self.held.with(|h| h.lease = None);
                Err(Attempt::Done(Outcome::platform(
                    format!("the build machine was refused: {why} ({label})"),
                    retryable,
                )))
            }
            Ready::Ended { reason, retryable } => {
                self.held.with(|h| h.lease = None);
                Err(Attempt::Done(Outcome::platform(
                    format!("the build machine ended before it was ready: {reason} ({label})"),
                    retryable,
                )))
            }
            Ready::Lost(reason) => {
                self.held.with(|h| h.lease = None);
                Err(Attempt::Lost(reason))
            }
            Ready::Api(e) => Err(Attempt::Done(api("reading the build machine", &e))),
        }
    }

    pub(super) async fn token(&self, lease: &str) -> Result<String, Fault> {
        sylphx_build_lease::mint_token(self.client, lease).await
    }

    pub(super) async fn guest(&self, lease: &sbx::Lease) -> Result<Guest, Fault> {
        sylphx_build_lease::guest(self.client, lease).await
    }
}
