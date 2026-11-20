//! Workspace sync, command execution, registry auth renewal, and artifact/image collection.

use super::*;

impl Run<'_> {
    /// Materialize a fresh workspace from in-cell packs. Direct tarballs
    /// remain the cache-failure and --no-cache path.
    pub(super) async fn sync(&self, g: &Guest, store: Option<&store::Store>) -> Result<(), Fault> {
        let t = Instant::now();
        let none = BTreeMap::new();
        // The admitted workload owns a newly created workspace: no previous
        // lease's manifest or build state is available, even with --region.
        let plan = sync::plan(&self.tree, None);
        let bytes = plan.upload_bytes(&self.tree);
        let mut up = 0u64;
        let filled = match store {
            Some(st) if !plan.upload.is_empty() => {
                let mut known = store::Known::load(
                    Some(sync::source_cache(&self.o.root).join("tree-packs")),
                    &st.instance_key(),
                );
                let f = store::fill(
                    g,
                    st,
                    &mut known,
                    store::Local {
                        root: &self.o.root,
                        tree: &self.tree,
                    },
                    &plan,
                    WS,
                    USER,
                )
                .await?;
                known.save();
                up += f.sent;
                if let Some(w) = &f.warning {
                    if !self.cache_warned.swap(true, Ordering::Relaxed) {
                        eprintln!(
                            "sylphx: warning: build store: {w}; sending those files directly"
                        );
                    }
                }
                f.paths
            }
            _ => Default::default(),
        };
        let direct: Vec<String> = plan
            .upload
            .iter()
            .filter(|p| !filled.contains(*p))
            .cloned()
            .collect();
        let tars = sync::tarballs(&self.o.root, &direct, &self.tree).map_err(Fault::Other)?;
        let lists = sync::lists(&self.tree, &plan, &filled);
        let s = format!("{WS}/.sylphx");
        let mut uploads: Vec<(String, &[u8])> = tars
            .iter()
            .enumerate()
            .map(|(i, t)| (format!("{s}/in-{i:04}.tar.gz"), t.as_slice()))
            .collect();
        for (name, body) in [
            ("drop", lists.drop.as_bytes()),
            ("add", lists.add.as_bytes()),
            ("remove", &lists.remove[..]),
        ] {
            if !body.is_empty() {
                uploads.push((format!("{s}/{name}"), body));
            }
        }
        // Stop at the first fault: further requests could race the lease-loss
        // check and must not outlive the failed workspace setup phase.
        for (path, body) in uploads {
            g.upload(&path, USER, body).await?;
            up += body.len() as u64;
        }
        self.add(|st| {
            st.bytes_up += up;
            st.from_store = filled.len();
        });
        let full = match (self.o.fresh, plan.full) {
            (true, _) => "2",
            (false, true) => "1",
            (false, false) => "0",
        };
        // Credentials are uploaded only into a private directory. Prepare it
        // in the existing sync-apply action, never as another round trip.
        let apply = if self.o.image.is_some() {
            format!(
                "set -eu\numask 077\nmkdir -m 700 {}\n{}",
                shell_word(&image::private_dir()),
                sync::APPLY
            )
        } else {
            sync::APPLY.to_string()
        };
        let (code, _, err) = g
            .run(
                USER,
                &argv(&[
                    "/bin/sh",
                    "-c",
                    &apply,
                    "apply",
                    WS,
                    full,
                    if filled.is_empty() { "0" } else { "1" },
                ]),
                &none,
            )
            .await?;
        if code != 0 {
            return Err(Fault::Other(format!(
                "{APPLY_FAILED}: {}",
                String::from_utf8_lossy(&err).trim()
            )));
        }
        let secs = t.elapsed().as_secs_f64();
        self.out.progress(&format!(
            "synced {} files, {}, in {secs:.1} s{}{}",
            plan.upload.len(),
            human(up),
            if filled.is_empty() {
                String::new()
            } else {
                format!("; {} from the build store", filled.len())
            },
            if plan.remove.is_empty() {
                String::new()
            } else {
                format!("; {} removed", plan.remove.len())
            }
        ));
        self.out.event(json!({
            "type": "sync",
            "full": plan.full,
            "files": plan.upload.len(),
            "removed": plan.remove.len(),
            "bytes": bytes,
            "bytes_up": up,
            "from_store": filled.len(),
            "duration_ms": t.elapsed().as_millis() as u64,
            "workspace": "cold",
            "volume": false,
        }));
        Ok(())
    }

    /// Upload only ungranted locked dependencies, then install them in the
    /// existing guest start action. Cold leases need no remote HAVE query.
    pub(super) async fn send_git_deps(
        &self,
        g: &Guest,
        covered: &BTreeSet<String>,
    ) -> Result<(BTreeMap<String, String>, String), Fault> {
        let deps: Vec<_> = self
            .git_deps
            .iter()
            .filter(|(d, _)| {
                !build_source::repository(&d.url).is_some_and(|repo| covered.contains(&repo))
            })
            .collect();
        let mut up = 0u64;
        let mut args = vec!["/bin/sh", "-c", git_deps::APPLY, "git-deps-apply", WS];
        let names: Vec<String> = deps.iter().map(|(d, _)| d.name()).collect();
        for result in pipeline(deps.iter().zip(&names), |((d, tgz), n)| async move {
            let d = d.clone();
            let tgz = tgz.clone();
            let cache = sync::source_cache(&self.o.root).join("git-deps");
            // Git/tar and filesystem reads are blocking; keep them off the
            // async executor so independent grants/uploads keep progressing.
            let bytes = tokio::task::spawn_blocking(move || {
                let tgz = match tgz {
                    Some(path) => path,
                    None => {
                        std::fs::create_dir_all(&cache).map_err(|e| {
                            Fault::Other(format!("preparing git dependencies: {e}"))
                        })?;
                        git_deps::pack(&cache, &d).map_err(Fault::Other)?
                    }
                };
                std::fs::read(&tgz)
                    .map_err(|e| Fault::Other(format!("reading {}: {e}", tgz.display())))
            })
            .await
            .map_err(|_| Fault::Other("packing git dependencies failed".into()))??;
            g.upload(&format!("{WS}/{}/in/{n}.tgz", git_deps::DIR), USER, &bytes)
                .await?;
            Ok::<_, Fault>(bytes.len() as u64)
        })
        .await
        {
            up += result?;
        }
        args.extend(names.iter().map(String::as_str));
        self.add(|s| s.bytes_up += up);
        if !names.is_empty() {
            self.out.event(json!({"type": "git_deps", "deps": names.len(), "sent": names.len(), "bytes_up": up}));
        }
        let deps: Vec<git_deps::Dep> = deps.iter().map(|(d, _)| d.clone()).collect();
        Ok((
            git_deps::env(&deps, WS),
            if names.is_empty() {
                String::new()
            } else {
                shell_call(&args)
            },
        ))
    }

    pub(super) async fn send_source_history(&self, g: &Guest) -> Result<String, Fault> {
        let Some((reference, bundle)) = &self.source_history else {
            return Ok(String::new());
        };
        for (i, part) in bundle.chunks(history::CHUNK_SIZE).enumerate() {
            if let Err(e) = g
                .upload(&format!("{WS}/.sylphx/source.bundle.part.{i}"), USER, part)
                .await
            {
                // Preserve the upload fault. An unreachable guest is reclaimed
                // with its cold workspace when the lease is released.
                let _ = g
                    .run(
                        USER,
                        &[
                            "/bin/sh".into(),
                            "-c".into(),
                            history::CLEAN_PARTS.into(),
                            "source-history".into(),
                            WS.into(),
                        ],
                        &BTreeMap::new(),
                    )
                    .await;
                return Err(e);
            }
        }
        let (code, _, stderr) = g
            .run(
                USER,
                &[
                    "/bin/sh".into(),
                    "-c".into(),
                    history::REASSEMBLE.into(),
                    "source-history".into(),
                    WS.into(),
                    bundle.len().div_ceil(history::CHUNK_SIZE).to_string(),
                    bundle.len().to_string(),
                    history::digest(bundle),
                ],
                &BTreeMap::new(),
            )
            .await?;
        if code != 0 {
            return Err(Fault::Other(format!(
                "source history reassembly failed ({code}): {}",
                String::from_utf8_lossy(&stderr)
            )));
        }
        let script = shell_call(&[
            "/bin/sh",
            "-c",
            history::APPLY,
            "source-history",
            WS,
            reference,
        ]);
        self.add(|s| s.bytes_up += bundle.len() as u64);
        self.out.event(
            json!({"type": "source_history", "reference": reference, "bytes_up": bundle.len()}),
        );
        Ok(script)
    }

    /// Runs the command, streaming its output; `None` when `--timeout` hit.
    /// A broken stream is reattached to the running command; the error says
    /// whether the command had started when the run lost it.
    pub(super) async fn exec(
        &self,
        g: &mut Guest,
        minted: &mut Instant,
        lease: &str,
        cache: BTreeMap<String, String>,
        mut sources: Sources,
        setup: &str,
    ) -> Result<Option<i32>, ExecFault> {
        let mut args = vec![
            "/bin/sh".to_string(),
            "-c".into(),
            start_script(setup, self.o.image.is_none()),
            "sylphx-run".into(),
            WS.into(),
            self.rel().to_string(),
            "1".into(),
        ];
        // An image build's command carries the publication resolved at
        // send_auth (the managed reference is only known then); everything
        // else runs the command the caller gave.
        let command = self.held.with(|h| match (&self.o.image, &h.publish) {
            (Some(img), Some(publish)) => img.argv(self.rel(), publish),
            _ => self.o.command.clone(),
        });
        args.extend(command);
        // The command's own `--env` wins over the cache's and ours.
        let cached = !cache.is_empty();
        let mut env = cache;
        if let Some(img) = &self.o.image {
            // An image build gets the cache's mirror addresses, never its
            // write tokens: no step of an image build needs sccache or Turbo.
            env.retain(|k, _| image::forwarded(k));
            env.entry("SYLPHX_OCI_MIRROR".into())
                .or_insert_with(image::default_oci_mirror);
            env.extend(img.env());
        }
        if let Some(sha) = sync::head(&self.o.root) {
            env.insert("SYLPHX_BUILD_GIT_HEAD".into(), sha);
        }
        env.extend(std::mem::take(&mut sources.env));
        env.extend(self.o.env.clone());
        if self.o.source_history.is_some() {
            env.insert("SYLPHX_BUILD_SOURCE_HISTORY".into(), "1".into());
        } else {
            env.remove("SYLPHX_BUILD_SOURCE_HISTORY");
        }
        let action_state = format!(
            "/tmp/sylphx-action-{:016x}",
            sylphx_build_lease::guest::rand_u64()
        );
        env.insert("SYLPHX_BUILD_ACTION_STATE".into(), action_state.clone());
        let mut p = g
            .start(USER, &args, "", &env)
            .await
            .map_err(ExecFault::before)?;
        let mut startup = Startup::default();
        let mut deadline = tokio::time::Instant::now() + Duration::from_secs(900);
        let mut tick = tokio::time::interval(KEEPALIVE);
        tick.tick().await;
        let mut registry_minted = Instant::now();
        let mut reattached = false;
        let mut output = redact::Output::new(self.out, self.held);
        let result = loop {
            tokio::select! {
                ev = p.next() => match ev {
                    Ok(Some(Event::Start(pid))) => {
                        let g2 = g.clone();
                        self.held.with(|h| h.proc = Some((g2, pid)));
                    }
                    Ok(Some(Event::Stdout(b))) => output.stdout(&b),
                    Ok(Some(Event::Stderr(b))) => {
                        let before = startup.started();
                        let bytes = startup.push(&b);
                        if !before && startup.started() {
                            deadline = tokio::time::Instant::now() + self.o.timeout;
                            self.out.event(json!({"type": "running", "lease": lease, "size": self.o.size, "region": self.region(), "workspace": "cold", "volume": false, "cache": cached}));
                        }
                        if !bytes.is_empty() { output.stderr(&bytes); }
                    }
                    Ok(Some(Event::End(c))) => {
                        let pending = startup.finish();
                        if !pending.is_empty() { output.stderr(&pending); }
                        if !startup.started() {
                            return Err(ExecFault::Setup(Fault::Other(format!("preparing the build machine failed (status {c}); see its stderr"))));
                        }
                        // An exit buffered while detached cannot outrun the
                        // original command deadline recovered from the guest.
                        break if tokio::time::Instant::now() >= deadline { None } else { Some(c) };
                    }
                    Ok(Some(Event::Error(m))) => {
                        let f = Fault::Other(format!("the command could not run: {m}"));
                        return Err(ExecFault::after(p.pid.is_some(), f));
                    }
                    Ok(None) | Err(Fault::Stream(_)) => {
                        // A proxy on the way restarted: follow the same
                        // process again rather than running it twice.
                        let broke = Instant::now();
                        if let Ok(t) = self.token(lease).await {
                            g.set_token(&t);
                            *minted = Instant::now();
                        }
                        // `--timeout` still holds while the stream is down.
                        let reattached_by = tokio::time::timeout_at(
                            deadline,
                            g.reattach_within(&mut p, REATTACH_BUDGET),
                        )
                        .await;
                        let Ok(reattach) = reattached_by else {
                            if let Some(pid) = p.pid {
                                let _ = g.signal(pid, "SIGKILL").await;
                            }
                            break None;
                        };
                        match reattach {
                            Ok(Reattach::Attached) => {
                                if !startup.started() {
                                    let elapsed = tokio::time::timeout_at(deadline, g.command_elapsed(&action_state))
                                        .await
                                        .map_err(|_| ExecFault::Setup(Fault::Other("recovering command-start state timed out".into())))?
                                        .map_err(|f| ExecFault::after(true, f))?;
                                    if let Some(elapsed) = elapsed {
                                        startup.recover();
                                        deadline = tokio::time::Instant::now() + self.o.timeout.saturating_sub(elapsed);
                                        self.out.event(json!({"type": "running", "lease": lease, "size": self.o.size, "region": self.region(), "workspace": "cold", "volume": false, "cache": cached}));
                                    }
                                }
                                let gap = broke.elapsed().as_millis() as u64;
                                self.out.event(json!({"type": "reattached", "gap_ms": gap}));
                                if !reattached {
                                    reattached = true;
                                    self.out.progress(&format!(
                                        "warning: the output stream broke; reattached to the running command after {:.1} s (output written meanwhile is not shown)",
                                        gap as f64 / 1000.0
                                    ));
                                }
                            }
                            Ok(Reattach::NotRunning) => {
                                let f = Fault::Stream("the output stream broke and the command is no longer on the machine".into());
                                return Err(ExecFault::after(p.pid.is_some(), f));
                            }
                            Err(f) => return Err(ExecFault::after(p.pid.is_some(), f)),
                        }
                    }
                    Err(f) => return Err(ExecFault::after(p.pid.is_some(), f)),
                },
                _ = tick.tick() => {
                    if minted.elapsed() >= TOKEN_RENEW {
                        if let Ok(t) = self.token(lease).await {
                            g.set_token(&t);
                            *minted = Instant::now();
                        }
                    }
                    if sources.expiring() {
                        if let Err(fault) = self.renew_sources(g, &mut sources).await {
                            if let Some(pid) = p.pid { let _ = g.signal(pid, "SIGKILL").await; }
                            return Err(ExecFault::after(p.pid.is_some(), fault));
                        }
                    }
                    if self.o.image.as_ref().is_some_and(|i| i.managed)
                        && registry_minted.elapsed() >= REGISTRY_TOKEN_RENEW
                    {
                        if let Err(fault) = self.renew_registry_auth(g).await {
                            if let Some(pid) = p.pid {
                                let _ = g.signal(pid, "SIGKILL").await;
                            }
                            return Err(ExecFault::after(p.pid.is_some(), fault));
                        }
                        registry_minted = Instant::now();
                    }
                    let _ = g.touch(WS).await;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    if let Some(pid) = p.pid {
                        let _ = g.signal(pid, "SIGKILL").await;
                    }
                    if !startup.started() {
                        return Err(ExecFault::Setup(Fault::Other("preparing the build machine took longer than 900s".into())));
                    }
                    break None;
                }
            }
        };
        self.add(|s| s.bytes_down += p.bytes);
        self.held.with(|h| h.proc = None);
        Ok(result)
    }

    /// `build image`: resolves the publication (minting the managed one),
    /// puts its credentials on the machine as a private file (0600 in a 0700
    /// directory), and adds the file's values to the secrets scrubbed from
    /// any text that leaves this process. The file never exists on this
    /// machine, and the credentials never enter an argument, an environment
    /// variable, or a synced file.
    pub(super) async fn send_auth(&self, g: &Guest) -> Result<(), Fault> {
        let Some(img) = self.o.image.as_ref() else {
            return Ok(());
        };
        let (publish, bytes) = if img.managed {
            let minted = self.mint_upload().await?;
            (
                image::Publish {
                    reference: Some(minted.reference),
                    auth: true,
                    managed: true,
                },
                minted.auth,
            )
        } else {
            let Some(file) = img.registry_auth.as_ref().filter(|f| f.is_file()) else {
                return Err(Fault::Other(
                    "--push needs a readable --registry-auth file".into(),
                ));
            };
            let bytes = std::fs::read(file).map_err(|e| {
                Fault::Other(format!("reading --registry-auth {}: {e}", file.display()))
            })?;
            (img.explicit_publish(), bytes)
        };
        self.held.with(|h| {
            let secrets = image::auth_secrets(&String::from_utf8_lossy(&bytes));
            h.output_secrets.extend(secrets.iter().cloned());
            h.secrets.extend(secrets);
        });
        let path = image::auth_path();
        g.upload(&path, USER, &bytes).await?;
        self.held.with(|h| h.publish = Some(publish));
        Ok(())
    }

    /// Mints this build's managed publication: the upload reference and a
    /// short-lived registry credential, both scoped by the verified grant and
    /// the run's canonical parent — never a repository a caller names. The
    /// registry host is the one the lease's egress was fixed to. An issuer
    /// that is down answers 503: a platform failure the caller may retry.
    pub(super) async fn mint_upload(&self) -> Result<Minted, Fault> {
        let parent = image::project_parent(&self.parent).ok_or_else(|| {
            Fault::Other(format!(
                "{}: no project to publish the image in",
                self.parent
            ))
        })?;
        let answer = self
            .client
            .invoke(
                "artifacts.artifacts.prepare_upload",
                json!({"parent": parent}),
            )
            .await
            .map_err(|e| Fault::Other(format!("minting the image upload: {}", why(&e))))?;
        let reference = answer
            .get("image_reference")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let auth = answer
            .get("registry_auth")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if reference.is_empty() || auth.is_empty() {
            return Err(Fault::Other(
                "the image upload mint answered no reference".into(),
            ));
        }
        // The lease's egress list was fixed before this call: a reference to
        // another host would be pushed into a blocked route.
        if image::registry_host(reference).as_deref() != Some(image::MANAGED_REGISTRY_HOST) {
            return Err(Fault::Other(format!(
                "the image upload mint answered {reference}: not on {}",
                image::MANAGED_REGISTRY_HOST
            )));
        }
        Ok(Minted {
            reference: reference.to_string(),
            auth: auth.as_bytes().to_vec(),
        })
    }

    /// Replaces the private config atomically. The guest credential helper
    /// reads it on each BuildKit credential lookup, including publication.
    pub(super) async fn renew_registry_auth(&self, g: &Guest) -> Result<(), Fault> {
        let minted = self.mint_upload().await?;
        let same_repository = self.held.with(|h| {
            h.publish
                .as_ref()
                .and_then(|p| p.reference.as_deref())
                .is_some_and(|reference| {
                    reference.rsplit_once(':').map(|(repository, _)| repository)
                        == minted
                            .reference
                            .rsplit_once(':')
                            .map(|(repository, _)| repository)
                })
        });
        if !same_repository {
            return Err(Fault::Other("the renewed upload changed repository".into()));
        }
        self.held.with(|h| {
            let secrets = image::auth_secrets(&String::from_utf8_lossy(&minted.auth));
            h.output_secrets.extend(secrets.iter().cloned());
            h.secrets.extend(secrets);
        });
        let path = image::auth_path();
        let next = format!("{path}.next");
        g.upload(&next, USER, &minted.auth).await?;
        let (code, _, _) = g
            .run(
                USER,
                &argv(&[
                    "/bin/sh",
                    "-c",
                    "chmod 600 \"$1\" && mv -f \"$1\" \"$2\"",
                    "registry-auth",
                    &next,
                    &path,
                ]),
                &BTreeMap::new(),
            )
            .await?;
        if code != 0 {
            return Err(Fault::Other(
                "renewing the private registry credentials failed".into(),
            ));
        }
        Ok(())
    }

    /// Removes what [`Self::send_auth`] put on the machine; best effort (the
    /// machine may be gone, and the next image build replaces the directory).
    pub(super) async fn drop_auth(&self, g: &Guest) {
        if self
            .held
            .with(|h| h.publish.as_ref().is_some_and(|p| p.auth))
        {
            let _ = g
                .run(
                    USER,
                    &argv(&["rm", "-rf", &image::private_dir()]),
                    &BTreeMap::new(),
                )
                .await;
        }
    }

    /// `build image`: copies the script's metadata (and the `--oci-out`
    /// archive) back and prints the digest, frontend and process sandbox.
    pub(super) async fn collect_image(&self, g: &Guest, lease: &str) -> Result<(), Outcome> {
        let fail = |what: &str, f: Fault| Outcome::platform(format!("{what}: {}", f.text()), true);
        let Some(bytes) = g
            .download(&image::metadata_path(), USER)
            .await
            .map_err(|f| fail("copying the image metadata back", f))?
        else {
            return Err(Outcome::platform(
                "the build ended without writing its metadata",
                false,
            ));
        };
        let meta: Value = serde_json::from_slice(&bytes).map_err(|e| {
            Outcome::platform(format!("the image metadata is not JSON: {e}"), false)
        })?;
        self.add(|s| s.bytes_down += bytes.len() as u64);
        let dir = PathBuf::from(".sylphx/out").join(lease.rsplit('/').next().unwrap_or("run"));
        let local = dir.join("image.json");
        std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(&local, &bytes))
            .map_err(|e| Outcome::platform(format!("{}: {e}", local.display()), false))?;
        if let Some(dest) = self.o.image.as_ref().and_then(|i| i.oci_out.as_ref()) {
            let tar = g
                .download(&image::oci_path(), USER)
                .await
                .map_err(|f| fail("copying the image archive back", f))?
                .ok_or_else(|| Outcome::platform("the image archive was not written", false))?;
            if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(dest, &tar)
                .map_err(|e| Outcome::platform(format!("{}: {e}", dest.display()), false))?;
            self.add(|s| s.bytes_down += tar.len() as u64);
        }
        for line in image::summary(&meta) {
            if self.o.json {
                continue;
            }
            println!("{line}");
        }
        // A managed publication is registered in Sylphx Artifacts, so a
        // Release can name it; the deployable reference is what a caller
        // deploys with.
        if let Some(publish) = self.held.with(|h| h.publish.clone()).filter(|p| p.managed) {
            let (artifact, source_ref) = self.register_image(g, &meta, &publish).await?;
            if !self.o.json {
                println!("source_ref: {source_ref}");
            }
            self.add(|s| {
                s.artifact = Some(artifact);
                s.source_ref = Some(source_ref);
            });
        }
        self.add(|s| s.image = Some(meta));
        Ok(())
    }

    /// Registers the pushed image as an immutable Artifact and answers its
    /// name and the deployable `repo@digest` reference. A digest already
    /// registered is the same image (a digest addresses immutable bytes):
    /// the existing Artifact's name is reported, never a second one and
    /// never a success without it.
    pub(super) async fn register_image(
        &self,
        g: &Guest,
        meta: &Value,
        publish: &image::Publish,
    ) -> Result<(String, String), Outcome> {
        let digest = image::image_digest(meta)
            .ok_or_else(|| Outcome::platform("the build metadata names no image digest", false))?;
        let reference = publish.reference.as_deref().unwrap_or_default();
        let source_ref = image::source_ref(reference, digest).ok_or_else(|| {
            Outcome::platform(format!("{reference}: not a published digest"), false)
        })?;
        let auth = g
            .download(&image::auth_path(), USER)
            .await
            .map_err(|f| {
                Outcome::platform(format!("reading registry credentials: {}", f.text()), true)
            })?
            .ok_or_else(|| {
                Outcome::platform("the build's registry credentials are missing", false)
            })?;
        let descriptor = image::registry_descriptor(&source_ref, &auth)
            .await
            .map_err(|e| Outcome::platform(e, false))?;
        let parent = image::project_parent(&self.parent).ok_or_else(|| {
            Outcome::platform(
                format!("{}: no project to register the image in", self.parent),
                false,
            )
        })?;
        let name = self
            .register(&parent, digest, descriptor, &source_ref)
            .await?;
        Ok((name, source_ref))
    }

    /// Registers one immutable Artifact for the pushed image and answers its
    /// name. A digest already registered is the same bytes: the Artifact
    /// there is read back and checked, never duplicated and never assumed.
    pub(super) async fn register(
        &self,
        parent: &str,
        digest: &str,
        descriptor: image::ImageDescriptor,
        expected_reference: &str,
    ) -> Result<String, Outcome> {
        let mut artifact = sylphx::artifacts::Artifact::default();
        artifact.digest = digest.to_string();
        artifact.kind = Some(sylphx::artifacts::ArtifactKind::Image);
        artifact.media_type = descriptor.media_type;
        artifact.size_bytes = descriptor.size_bytes;
        let mut request = sylphx::artifacts::CreateArtifactRequest::default();
        request.parent = parent.to_string();
        request.artifact = Some(artifact);
        request.managed_upload = true;
        let registered = match self.client.artifacts().artifacts().create(request).await {
            Ok(a) => a,
            Err(sylphx::Error::Api {
                code: sylphx::common::ErrorCode::ResourceAlreadyExists,
                ..
            }) => self.existing_image(parent, digest).await?,
            Err(e) => return Err(api("registering the image artifact", &e)),
        };
        let expected_name = image::artifact_name(parent, digest)
            .ok_or_else(|| Outcome::platform("the image digest is not canonical", false))?;
        if registered.name != expected_name
            || registered.digest != digest
            || registered.kind != Some(sylphx::artifacts::ArtifactKind::Image)
            || registered.reference != expected_reference
        {
            return Err(Outcome::platform(
                "the registered Artifact does not name this managed image",
                false,
            ));
        }
        Ok(registered.name)
    }

    /// The Artifact already registered for `digest`, checked to be that same
    /// image.
    pub(super) async fn existing_image(
        &self,
        parent: &str,
        digest: &str,
    ) -> Result<sylphx::artifacts::Artifact, Outcome> {
        let name = image::artifact_name(parent, digest).ok_or_else(|| {
            Outcome::platform(format!("{parent}: no artifact name for {digest}"), false)
        })?;
        let mut request = sylphx::artifacts::GetArtifactRequest::default();
        request.name = name.clone();
        let artifact = self
            .client
            .artifacts()
            .artifacts()
            .get(request)
            .await
            .map_err(|e| api(&format!("reading the registered image artifact {name}"), &e))?;
        if artifact.digest != digest {
            return Err(Outcome::platform(
                format!("{name} holds {} and not {digest}", artifact.digest.trim()),
                false,
            ));
        }
        Ok(artifact)
    }

    /// Copies the `--artifact` matches back, checking each digest.
    pub(super) async fn collect(&self, g: &Guest, lease: &str) -> Result<(), String> {
        let rel = if self.o.rel.is_empty() {
            "."
        } else {
            &self.o.rel
        };
        let mut args = argv(&[
            "/bin/bash",
            "-O",
            "globstar",
            "-O",
            "nullglob",
            "-c",
            ARTIFACTS,
            "artifacts",
            WS,
            rel,
        ]);
        args.extend(self.o.artifacts.iter().cloned());
        let (_, listing, _) = g
            .run(USER, &args, &BTreeMap::new())
            .await
            .map_err(|f| f.text().to_string())?;
        let dir = self.o.out.clone().unwrap_or_else(|| {
            PathBuf::from(".sylphx/out").join(lease.rsplit('/').next().unwrap_or("run"))
        });
        let mut n = 0usize;
        for line in String::from_utf8_lossy(&listing).lines() {
            let Some((digest, path)) = parse_sum(line) else {
                continue;
            };
            let Some(local) = artifact_path(&dir, path) else {
                continue;
            };
            let remote = format!("{WS}/tree/{rel}/{path}");
            let bytes = g
                .download(&remote, USER)
                .await
                .map_err(|f| f.text().to_string())?
                .ok_or_else(|| format!("{path} disappeared before it was copied"))?;
            if sync::sha256_hex(&bytes) != digest {
                return Err(format!("{path} changed while it was copied"));
            }
            if let Some(parent) = local.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            std::fs::write(&local, &bytes).map_err(|e| format!("{}: {e}", local.display()))?;
            self.add(|s| {
                s.bytes_down += bytes.len() as u64;
                s.artifacts += 1;
            });
            self.out.event(json!({
                "type": "artifact",
                "path": local.to_string_lossy(),
                "digest": format!("sha256:{digest}"),
                "bytes": bytes.len(),
            }));
            n += 1;
        }
        if n == 0 {
            self.out.progress("no file matched --artifact");
        }
        Ok(())
    }
}
