//! `sylphx devices run`: an app's smoke test on a device lease
//! (docs/specs/one-platform/device-farm.md §5.2). It creates an ANDROID
//! Sandboxes lease, waits for the device, uploads the APK in chunks, installs
//! it, launches it, watches it with screenshots, collects logcat, and writes
//! `result.json` and JUnit XML. The lease is always released — also when a step
//! fails or on Ctrl-C — unless `--keep`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sylphx::sandboxes as sbx;
use sylphx::Client;

use crate::{names, Failure};

mod diagnostics;
mod scenario;

/// A device lease is a `linux-l` Sandboxes lease of kind ANDROID (§3, §4.1).
const SHAPE: &str = "linux-l";
const IDLE_TIMEOUT: &str = "10m";
/// A cold emulator boots in 1–3 minutes and the Cell's own boot timeout is 10,
/// so wait 12 for a device that is still queued behind another test run.
const READY_TIMEOUT: Duration = Duration::from_secs(720);
const POLL: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(200);
/// The app is uploaded here; an ANDROID lease confines files to /home/user.
const APK: &str = "/home/user/app.apk";
/// One `:writeFile` call carries at most 16 MiB of content; 12 MiB leaves room
/// for the base64 encoding.
const CHUNK: usize = 12 * 1024 * 1024;
const DEFAULT_WAIT: Duration = Duration::from_secs(20);
const GAME_LOOP_WAIT: Duration = Duration::from_secs(300);
/// The Firebase Test Lab game loop contract: the app runs scenario N and exits.
const GAME_LOOP_ACTION: &str = "com.google.intent.action.TEST_LOOP";
const LAUNCH_CATEGORY: &str = "android.intent.category.LAUNCHER";

pub fn command() -> Command {
    Command::new("devices")
        .about("Android devices: run an app on a fresh device and report whether it stays up")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("run")
                .about("Install an APK on an Android device lease, launch it, watch it, and write screenshots, logcat, result.json, and junit.xml")
                .arg(Arg::new("app").long("app").value_name("PATH").required(true)
                    .help("The APK to install"))
                .arg(Arg::new("env").long("env").value_name("ENV")
                    .help("The environment (a slug, an id, or orgs/…/envs/…); defaults to the linked env, else the key's scope"))
                .arg(Arg::new("model").long("model").value_name("MODEL")
                    .help("Device model from the catalog, e.g. pixel_7; default the platform's"))
                .arg(Arg::new("os-version").long("os-version").value_name("VERSION")
                    .help("OS version, e.g. 14; default the newest offered"))
                .arg(Arg::new("ttl").long("ttl").value_name("DURATION").default_value("30m")
                    .help("The lease's wall-time limit (60s to 24h)"))
                .arg(Arg::new("wait").long("wait").value_name("DURATION")
                    .help("How long the app is left running before it is judged (default 20s; 5m with --game-loop)"))
                .arg(Arg::new("game-loop").long("game-loop").value_name("N")
                    .value_parser(clap::value_parser!(i64))
                    .help("Start a Test Lab game loop (com.google.intent.action.TEST_LOOP) with scenario N instead of the launcher; it ends when the app's process exits"))
                .arg(Arg::new("scenario").long("scenario").value_name("JSON")
                    .requires("size").conflicts_with("game-loop")
                    .help("Run a native capture scenario through the public lease; one fresh lease for the selected size"))
                .arg(Arg::new("size").long("size").value_name("LABEL")
                    .requires("scenario")
                    .help("One size label declared in --scenario; PNG dimensions are verified without rescaling"))
                .arg(Arg::new("screenshot-every").long("screenshot-every").value_name("DURATION").default_value("5s")
                    .help("Screenshot this often while the app runs (0s takes only the final one)"))
                .arg(Arg::new("out").long("out").value_name("DIR").default_value("device-results")
                    .help("Where screenshots, logcat, result.json, and junit.xml are written"))
                .arg(Arg::new("keep").long("keep").action(ArgAction::SetTrue)
                    .help("Keep the lease running at the end and print its name")),
        )
}

pub async fn run(client: &Client, matches: &ArgMatches) -> Result<(), Failure> {
    let Some(("run", m)) = matches.subcommand() else {
        return Err(Failure::Usage(
            "give a subcommand: `sylphx devices run --app app.apk`".into(),
        ));
    };
    let opts = Opts::parse(m).map_err(Failure::Usage)?;
    // The lease is released on the way out whichever way the run ends: on
    // Ctrl-C the future below is dropped, so the name is kept beside it.
    let held = Held::default();
    let outcome = tokio::select! {
        o = execute(client, &opts, &held) => o,
        _ = tokio::signal::ctrl_c() => Outcome::interrupted(),
    };
    let lease = held.get();
    let mut code = outcome.code();
    if let Err(e) = write_reports(&opts, &outcome) {
        eprintln!("error: {e}");
        code = 2;
    }
    if opts.json {
        println!("{}", report_text(&outcome).trim_end());
    }
    match (&lease, opts.keep) {
        (Some(name), true) => {
            eprintln!("device: keeping {name}");
            if !opts.json && code == 0 {
                println!("{name}");
            }
        }
        (Some(name), false) => {
            match client
                .sandboxes()
                .leases()
                .release(release_request(name))
                .await
            {
                Ok(_) => eprintln!("device: released {name}"),
                Err(e) => eprintln!(
                    "warning: {name} was not released ({}); its idle timeout ends it",
                    why(&e)
                ),
            }
        }
        (None, _) => {}
    }
    if code == 0 {
        Ok(())
    } else {
        Err(Failure::Exit(code))
    }
}

struct Opts {
    app: PathBuf,
    env: Option<String>,
    org: Option<String>,
    model: String,
    os_version: String,
    ttl: Duration,
    wait: Duration,
    game_loop: Option<i64>,
    scenario: Option<Capture>,
    every: Duration,
    out: PathBuf,
    keep: bool,
    json: bool,
}

struct Capture {
    scenario: scenario::Scenario,
    display: scenario::Display,
    steps: Vec<scenario::Step>,
    label: String,
    sha256: String,
}

impl Opts {
    fn parse(m: &ArgMatches) -> Result<Self, String> {
        let app = PathBuf::from(m.get_one::<String>("app").expect("required"));
        if !app.is_file() {
            return Err(format!("{} is not a file", app.display()));
        }
        let game_loop = m.get_one::<i64>("game-loop").copied();
        let scenario = m
            .get_one::<String>("scenario")
            .map(|path| {
                let bytes = std::fs::read(path).map_err(|error| format!("{path}: {error}"))?;
                let label = m.get_one::<String>("size").expect("required with scenario");
                let (scenario, display, steps) = scenario::Scenario::parse(&bytes, label)?;
                Ok::<_, String>(Capture {
                    scenario,
                    display,
                    steps,
                    label: label.clone(),
                    sha256: format!("{:x}", Sha256::digest(&bytes)),
                })
            })
            .transpose()?;
        let wait = match m.get_one::<String>("wait") {
            Some(v) => duration(v, "--wait")?,
            None if game_loop.is_some() => GAME_LOOP_WAIT,
            None => DEFAULT_WAIT,
        };
        let ttl = duration(m.get_one::<String>("ttl").expect("default"), "--ttl")?;
        if ttl < Duration::from_secs(60) {
            return Err("--ttl is 60s or more".into());
        }
        let out = PathBuf::from(m.get_one::<String>("out").expect("default"));
        if scenario.is_some() && out.exists() {
            let mut entries =
                std::fs::read_dir(&out).map_err(|error| format!("{}: {error}", out.display()))?;
            if entries.next().is_some() {
                return Err("capture output is not empty; use a new --out directory".into());
            }
        }
        Ok(Self {
            app,
            env: m.get_one::<String>("env").cloned(),
            org: m.try_get_one::<String>("org").ok().flatten().cloned(),
            model: m.get_one::<String>("model").cloned().unwrap_or_default(),
            os_version: m
                .get_one::<String>("os-version")
                .cloned()
                .unwrap_or_default(),
            ttl,
            wait,
            game_loop,
            scenario,
            every: duration(
                m.get_one::<String>("screenshot-every").expect("default"),
                "--screenshot-every",
            )?,
            out,
            keep: m.get_flag("keep"),
            json: m.get_one::<String>("output").map(String::as_str) == Some("json"),
        })
    }
}

/// The lease this run holds, so the way out can name it even when the run
/// itself was dropped (Ctrl-C).
#[derive(Default)]
struct Held(Mutex<Option<String>>);

impl Held {
    fn set(&self, name: &str) {
        *self.0.lock().expect("not poisoned") = Some(name.to_string());
    }

    fn get(&self) -> Option<String> {
        self.0.lock().expect("not poisoned").clone()
    }
}

#[derive(Default)]
struct Report {
    lease: String,
    model: String,
    os_version: String,
    package: String,
    installed: bool,
    launched: bool,
    crashed: bool,
    install_error: String,
    launch_error: String,
    /// The crash evidence, for the JUnit message.
    crash: String,
    error: String,
    screenshots: Vec<String>,
    timings: Vec<(&'static str, u128)>,
    capture: Option<Value>,
    expected_stopped: bool,
    platform_interruption: Option<diagnostics::Interruption>,
    logcat_truncated: bool,
    crash_log_truncated: bool,
}

impl Report {
    fn diagnose(&mut self, package: &str, process_missing: bool, logcat: &str, crash_log: &str) {
        // Preserve positive app-crash/ANR evidence even when a dependency
        // interruption is also present in the retained logs.
        if !package.is_empty() && !self.crashed {
            if let Some(evidence) =
                crash_evidence(package, &[("the crash log", crash_log), ("logcat", logcat)])
            {
                self.crashed = true;
                self.crash = evidence;
            }
        }
        if process_missing && !self.crashed {
            self.platform_interruption = diagnostics::dependency_interruption(package, logcat);
            if self.platform_interruption.is_none() {
                self.crashed = true;
                self.crash = format!("the {package} process was gone at the end of the run");
            }
        }
        if self.error.is_empty() {
            if self.crashed {
                self.error = self.crash.clone();
            } else if let Some(evidence) = &self.platform_interruption {
                self.error = evidence.message();
            }
        }
    }
}

struct Outcome {
    report: Report,
    /// Set when the run could not be carried out at all (exit 2).
    infra: Option<String>,
    interrupted: bool,
}

impl Outcome {
    fn interrupted() -> Self {
        let report = Report {
            error: "interrupted".into(),
            ..Default::default()
        };
        Self {
            report,
            infra: None,
            interrupted: true,
        }
    }

    fn code(&self) -> u8 {
        if self.interrupted {
            return 130;
        }
        if self.infra.is_some() || self.report.platform_interruption.is_some() {
            return 2;
        }
        let r = &self.report;
        u8::from(!(r.installed && r.launched && !r.crashed))
    }
}

async fn execute(client: &Client, o: &Opts, held: &Held) -> Outcome {
    let mut run = Run {
        client,
        o,
        held,
        r: Report::default(),
    };
    let infra = match run.go().await {
        Ok(()) => None,
        Err(e) => {
            eprintln!("error: {e}");
            run.r.error = e.clone();
            Some(e)
        }
    };
    Outcome {
        report: run.r,
        infra,
        interrupted: false,
    }
}

struct Run<'a> {
    client: &'a Client,
    o: &'a Opts,
    held: &'a Held,
    r: Report,
}

impl Run<'_> {
    async fn go(&mut self) -> Result<(), String> {
        std::fs::create_dir_all(&self.o.out)
            .map_err(|e| format!("{}: {e}", self.o.out.display()))?;
        let parent = parent_env(self.client, self.o.env.as_deref(), self.o.org.as_deref()).await?;

        let t = Instant::now();
        let lease = self.create(&parent).await?;
        let name = lease.name.clone();
        self.r.lease = name.clone();
        let (model, os) = device_of(&lease);
        self.r.model = if model.is_empty() {
            self.o.model.clone()
        } else {
            model
        };
        self.r.os_version = if os.is_empty() {
            self.o.os_version.clone()
        } else {
            os
        };
        self.timing("lease", t);
        eprintln!("device: ready in {}s", t.elapsed().as_secs());

        let t = Instant::now();
        self.upload(&name).await?;
        self.timing("upload", t);

        self.install(&name).await;
        if !self.r.installed {
            eprintln!("device: {}", self.r.install_error);
        } else {
            eprintln!("device: installed {}", self.r.package);
            // The launch onwards is what the collected log is about.
            let cleared = self.exec(&name, &["logcat", "-c"]).await?;
            if cleared.exit_code != 0 {
                return Err(format!(
                    "clearing the launch log failed: {}",
                    tail(&cleared)
                ));
            }
            if self.o.scenario.is_some() {
                // Keep diagnostics even when an input guard or scenario step
                // fails; the original error still controls the verdict.
                if let Err(error) = self.capture(&name).await {
                    let _ = self.collect(&name).await;
                    return Err(error);
                }
            } else {
                let t = Instant::now();
                self.launch(&name).await?;
                self.timing("launch", t);
                if self.r.launched {
                    eprintln!("device: launched");
                } else {
                    eprintln!("device: {}", self.r.launch_error);
                }
                self.watch(&name).await?;
            }
        }
        self.collect(&name).await?;
        Ok(())
    }

    /// Creates the lease and waits until the device is READY.
    async fn create(&self, parent: &str) -> Result<sbx::Lease, String> {
        let leases = self.client.sandboxes().leases();
        let mut spec = sbx::LeaseSpec::default();
        spec.shape = SHAPE.into();
        spec.kind = Some(sbx::LeaseKind::Android);
        spec.ttl = wire(self.o.ttl);
        spec.idle_timeout = IDLE_TIMEOUT.into();
        if !self.o.model.is_empty() || !self.o.os_version.is_empty() {
            let mut device = sbx::DeviceSpec::default();
            device.model = self.o.model.clone();
            device.os_version = self.o.os_version.clone();
            spec.device = Some(device);
        }
        let mut meta = sylphx::common::ResourceMeta::default();
        // The caller's correlation label; List filters on it.
        meta.labels.insert("purpose".into(), "device-test".into());
        let mut lease = sbx::Lease::default();
        lease.meta = Some(meta);
        lease.spec = Some(spec);
        let mut req = sbx::CreateLeaseRequest::default();
        req.parent = parent.to_string();
        req.lease = Some(lease);

        let mut lease = leases.create(req).await.map_err(|e| why(&e))?;
        self.held.set(&lease.name);
        eprintln!("device: lease {}", lease.name);
        if state(&lease) != sbx::LeaseState::Ready {
            eprintln!("device: waiting for the device…");
        }
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            match state(&lease) {
                sbx::LeaseState::Ready => return Ok(lease),
                sbx::LeaseState::Refused => return Err(refused(&lease)),
                sbx::LeaseState::Ending | sbx::LeaseState::Ended => return Err(ended(&lease)),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the lease was not ready after {} minutes",
                    READY_TIMEOUT.as_secs() / 60
                ));
            }
            tokio::time::sleep(POLL).await;
            let mut get = sbx::GetLeaseRequest::default();
            get.name = lease.name.clone();
            lease = leases.get(get).await.map_err(|e| why(&e))?;
        }
    }

    /// Uploads the APK: the first `:writeFile` replaces, the rest append.
    async fn upload(&self, lease: &str) -> Result<(), String> {
        let bytes =
            std::fs::read(&self.o.app).map_err(|e| format!("{}: {e}", self.o.app.display()))?;
        let ranges = chunks(bytes.len(), CHUNK);
        for (i, (from, to)) in ranges.iter().enumerate() {
            let mut req = sbx::WriteLeaseFileRequest::default();
            req.name = lease.to_string();
            req.path = APK.into();
            req.content = base64::engine::general_purpose::STANDARD.encode(&bytes[*from..*to]);
            req.append = i > 0;
            self.client
                .sandboxes()
                .leases()
                .write_file(req)
                .await
                .map_err(|e| format!("the upload failed: {}", why(&e)))?;
            if ranges.len() > 1 {
                eprintln!(
                    "device: uploaded {:.1} of {:.1} MiB",
                    *to as f64 / 1048576.0,
                    bytes.len() as f64 / 1048576.0
                );
            }
        }
        Ok(())
    }

    /// Installs with every permission granted. A refusal to install is a test
    /// failure, not an infrastructure error, so the run goes on to collect.
    async fn install(&mut self, lease: &str) {
        let t = Instant::now();
        let mut req = sbx::InstallLeaseAppRequest::default();
        req.name = lease.to_string();
        req.apk_path = APK.into();
        req.grant_permissions = true;
        match self.client.sandboxes().leases().install_app(req).await {
            Ok(r) if !r.package_name.is_empty() => {
                self.r.package = r.package_name;
                self.r.installed = true;
            }
            Ok(_) => self.r.install_error = "the install answered no package name".into(),
            Err(e) => self.r.install_error = format!("the install failed: {}", why(&e)),
        }
        if !self.r.installed && self.r.error.is_empty() {
            self.r.error = self.r.install_error.clone();
        }
        self.timing("install", t);
    }

    /// Launches the app: the launcher activity, or a Test Lab game loop.
    async fn launch(&mut self, lease: &str) -> Result<(), String> {
        let package = self.r.package.clone();
        let scenario = self.o.game_loop.map(|n| n.to_string());
        let argv: Vec<&str> = match &scenario {
            Some(n) => vec![
                "am",
                "start",
                "-W",
                "-S",
                "-a",
                GAME_LOOP_ACTION,
                "-t",
                "application/javascript",
                "-p",
                package.as_str(),
                "--ei",
                "scenario",
                n.as_str(),
            ],
            None => vec!["monkey", "-p", package.as_str(), "-c", LAUNCH_CATEGORY, "1"],
        };
        match self.exec(lease, &argv).await {
            Ok(o) if o.exit_code == 0 => {
                self.r.launched = true;
                self.r.expected_stopped = false;
            }
            Ok(o) => self.r.launch_error = format!("the launch failed: {}", tail(&o)),
            Err(e) => return Err(e),
        }
        if !self.r.launched && self.r.error.is_empty() {
            self.r.error = self.r.launch_error.clone();
        }
        Ok(())
    }

    /// Lets the app run: a game loop ends when its process exits, anything else
    /// runs for `--wait`; screenshots are taken on the way.
    async fn watch(&mut self, lease: &str) -> Result<(), String> {
        let t = Instant::now();
        let deadline = t + self.o.wait;
        let (mut shot_at, mut probe_at) = (t, t);
        let (game_loop, package) = (self.o.game_loop.is_some(), self.r.package.clone());
        if game_loop {
            eprintln!(
                "device: game loop scenario {} for up to {}s",
                self.o.game_loop.unwrap_or_default(),
                self.o.wait.as_secs()
            );
        }
        let mut shots = 0usize;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            // Test Lab ends a game loop when the app's process ends; reaching
            // the timeout is not a failure.
            if game_loop && now >= probe_at {
                probe_at = now + POLL;
                if self
                    .exec(lease, &["pidof", package.as_str()])
                    .await?
                    .exit_code
                    != 0
                {
                    eprintln!(
                        "device: the game loop ended after {}s",
                        t.elapsed().as_secs()
                    );
                    break;
                }
            }
            if !self.o.every.is_zero() && now >= shot_at {
                shot_at = now + self.o.every;
                let png = self.screenshot(lease).await?;
                self.save(&format!("screenshot-{shots:03}.png"), &png)?;
                shots += 1;
            }
            tokio::time::sleep(TICK.min(deadline - now)).await;
        }
        eprintln!(
            "device: ran for {}s, {shots} screenshots",
            t.elapsed().as_secs()
        );
        self.timing("wait", t);
        Ok(())
    }

    /// The end of the run: the final screenshot, the logs, and the verdict.
    async fn collect(&mut self, lease: &str) -> Result<(), String> {
        let t = Instant::now();
        let png = self.screenshot(lease).await?;
        self.save("screenshot-final.png", &png)?;

        let package = self.r.package.clone();
        let check_process = !package.is_empty()
            && !self.r.crashed
            && !self.r.expected_stopped
            && self.o.game_loop.is_none()
            && self.r.launched;
        let process_missing = check_process
            && self
                .exec(lease, &["pidof", package.as_str()])
                .await?
                .exit_code
                != 0;
        let logcat = self.exec(lease, &["logcat", "-d"]).await?;
        let crash_log = self.exec(lease, &["logcat", "-b", "crash", "-d"]).await?;
        self.r.logcat_truncated = logcat.truncated;
        self.r.crash_log_truncated = crash_log.truncated;
        if logcat.truncated || crash_log.truncated {
            eprintln!("device: the API truncated collected logs; result.json records which files are partial");
        }
        write(&self.o.out.join("logcat.txt"), logcat.stdout.as_bytes())?;
        write(&self.o.out.join("crash.txt"), crash_log.stdout.as_bytes())?;
        self.r
            .diagnose(&package, process_missing, &logcat.stdout, &crash_log.stdout);
        self.timing("collect", t);
        Ok(())
    }

    /// `:exec` on an ANDROID lease runs the argv on the device (§4.1). The
    /// wire carries the output as base64 bytes; this answers it as text.
    async fn exec(&self, lease: &str, argv: &[&str]) -> Result<Exec, String> {
        let mut req = sbx::ExecLeaseRequest::default();
        req.name = lease.to_string();
        req.command = argv.iter().map(|a| a.to_string()).collect();
        let r = self
            .client
            .sandboxes()
            .leases()
            .exec(req)
            .await
            .map_err(|e| format!("`{}` failed: {}", argv[0], why(&e)))?;
        Ok(r.into())
    }

    /// `:act` with no actions and a screenshot set only takes a screenshot.
    async fn screenshot(&self, lease: &str) -> Result<Vec<u8>, String> {
        let mut req = sbx::ActLeaseRequest::default();
        req.name = lease.to_string();
        let mut shot = sbx::ScreenshotOptions::default();
        shot.format = Some(sbx::ImageFormat::Png);
        req.screenshot = Some(shot);
        let response = self
            .client
            .sandboxes()
            .leases()
            .act(req)
            .await
            .map_err(|e| format!("the screenshot failed: {}", why(&e)))?;
        let data = response.screenshot.map(|s| s.data).unwrap_or_default();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.as_bytes())
            .map_err(|e| format!("the screenshot is not base64: {e}"))?;
        if bytes.is_empty() {
            return Err("the device answered an empty screenshot".into());
        }
        Ok(bytes)
    }

    fn save(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        write(&self.o.out.join(name), bytes)?;
        self.r.screenshots.push(name.to_string());
        Ok(())
    }

    fn timing(&mut self, step: &'static str, since: Instant) {
        self.r.timings.push((step, since.elapsed().as_millis()));
    }

    /// One selected display on this fresh lease. Do not carry application state
    /// from one size to the next or run a copied private adb driver.
    async fn capture(&mut self, lease: &str) -> Result<(), String> {
        let capture = self.o.scenario.as_ref().expect("capture selected");
        let bytes = std::fs::read(&self.o.app).map_err(|error| error.to_string())?;
        self.r.capture = Some(json!({
            "lease": lease,
            "title": capture.scenario.title,
            "source_commit": capture.scenario.source_commit,
            "scenario_sha256": capture.sha256,
            "apk_sha256": format!("{:x}", Sha256::digest(&bytes)),
            "size": capture.label,
            "requested_display": {"width":capture.display.width,"height":capture.display.height,"density":capture.display.density},
            "steps": [],
        }));
        self.write_capture_receipt()?;
        if capture.scenario.package != self.r.package {
            return Err("installed package differs from the scenario; no input sent".into());
        }
        let dimensions = format!("{}x{}", capture.display.width, capture.display.height);
        let density = capture.display.density.to_string();
        self.checked_exec(lease, &["wm", "size", &dimensions])
            .await?;
        self.checked_exec(lease, &["wm", "density", &density])
            .await?;
        let size = self.checked_exec(lease, &["wm", "size"]).await?;
        if scenario::wm_value(&size.stdout, "size").as_deref() != Some(dimensions.as_str()) {
            return Err("device did not confirm the requested native display override".into());
        }
        let actual_density = self.checked_exec(lease, &["wm", "density"]).await?;
        if scenario::wm_value(&actual_density.stdout, "density").as_deref()
            != Some(density.as_str())
        {
            return Err("device did not confirm the requested native density override".into());
        }
        let has_input = capture.steps.iter().any(scenario::Step::input);
        if has_input {
            // A capture is not authorization to spend. Do not send any input
            // unless Play billing is absent or verifiably disabled. The Android
            // google_apis image does not necessarily include the Play Store.
            let installed = self
                .checked_exec(lease, &["pm", "list", "packages"])
                .await?;
            if !installed
                .stdout
                .lines()
                .any(|line| line.trim() == "package:android")
            {
                return Err(
                    "package inventory lacks its Android control; no scenario input sent".into(),
                );
            }
            if installed
                .stdout
                .lines()
                .any(|line| line.trim() == "package:com.android.vending")
            {
                self.checked_exec(
                    lease,
                    &["pm", "disable-user", "--user", "0", "com.android.vending"],
                )
                .await?;
                let disabled = self
                    .checked_exec(lease, &["pm", "list", "packages", "-d"])
                    .await?;
                if !disabled
                    .stdout
                    .lines()
                    .any(|line| line.trim() == "package:com.android.vending")
                {
                    return Err(
                        "billing guard could not be verified; no scenario input sent".into(),
                    );
                }
            }
        }
        let steps = capture.steps.clone();
        let deadline = Instant::now() + self.o.ttl.saturating_sub(Duration::from_secs(60));
        for (index, step) in steps.iter().enumerate() {
            if Instant::now() >= deadline {
                return Err("scenario exceeded the lease budget".into());
            }
            let operation = match step {
                scenario::Step::Launch => "launch",
                scenario::Step::Wait(_) => "wait",
                scenario::Step::Tap(_) => "tap",
                scenario::Step::Swipe(..) => "swipe",
                scenario::Step::Key(_) => "key",
                scenario::Step::Text(_) => "text",
                scenario::Step::Stop => "stop",
                scenario::Step::Shot(_) => "shot",
            };
            self.record_capture_step(
                json!({"index":index,"operation":operation,"state":"started",
                "started_at_unix_ms":timestamp_ms()}),
            )?;
            if step.input() {
                self.assert_foreground(lease).await?;
            }
            match step {
                scenario::Step::Launch => {
                    self.launch(lease).await?;
                    if !self.r.launched {
                        return Err(self.r.launch_error.clone());
                    }
                }
                scenario::Step::Wait(seconds) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if *seconds > remaining.as_secs_f64() {
                        return Err("scenario wait exceeds the lease budget".into());
                    }
                    tokio::time::sleep(Duration::from_secs_f64(*seconds)).await;
                }
                scenario::Step::Shot(label) => {
                    self.assert_foreground(lease).await?;
                    let png = self.capture_png(lease).await?;
                    let (width, height) = scenario::png_size(&png)?;
                    let name = format!("{label}.png");
                    self.save(&name, &png)?;
                    let receipt = json!({"index":index,"operation":"shot","path":name,"width":width,"height":height,
                        "sha256":format!("{:x}",Sha256::digest(&png)),"state":"success","completed_at_unix_ms":timestamp_ms()});
                    self.complete_capture_step(receipt)?;
                    continue;
                }
                scenario::Step::Tap(fractions) => {
                    let png = self.capture_png(lease).await?;
                    let point = scenario::point(*fractions, scenario::png_size(&png)?);
                    self.checked_exec(lease, &["input", "tap", &point[0], &point[1]])
                        .await?;
                }
                scenario::Step::Swipe(fractions, duration) => {
                    if Duration::from_millis(*duration)
                        > deadline.saturating_duration_since(Instant::now())
                    {
                        return Err("scenario swipe exceeds the lease budget".into());
                    }
                    let png = self.capture_png(lease).await?;
                    let dimensions = scenario::png_size(&png)?;
                    let start = scenario::point([fractions[0], fractions[1]], dimensions);
                    let end = scenario::point([fractions[2], fractions[3]], dimensions);
                    self.checked_exec(
                        lease,
                        &[
                            "input",
                            "swipe",
                            &start[0],
                            &start[1],
                            &end[0],
                            &end[1],
                            &duration.to_string(),
                        ],
                    )
                    .await?;
                }
                scenario::Step::Key(key) => {
                    self.checked_exec(lease, &["input", "keyevent", key])
                        .await?;
                }
                scenario::Step::Text(text) => {
                    // The public API quotes every argv element for the Android
                    // shell. Do not quote twice or log the entered contents.
                    let text = text.replace(' ', "%s");
                    self.checked_exec(lease, &["input", "text", &text]).await?;
                }
                scenario::Step::Stop => {
                    self.checked_exec(lease, &["am", "force-stop", &self.r.package])
                        .await?;
                    self.r.expected_stopped = true;
                }
            }
            self.complete_capture_step(
                json!({"index":index,"operation":operation,"state":"success",
                "completed_at_unix_ms":timestamp_ms()}),
            )?;
        }
        Ok(())
    }

    fn record_capture_step(&mut self, step: Value) -> Result<(), String> {
        self.r.capture.as_mut().expect("capture receipt")["steps"]
            .as_array_mut()
            .expect("step array")
            .push(step);
        self.write_capture_receipt()
    }

    fn complete_capture_step(&mut self, step: Value) -> Result<(), String> {
        let current = self.r.capture.as_mut().expect("capture receipt")["steps"]
            .as_array_mut()
            .expect("step array")
            .last_mut()
            .expect("started step");
        current
            .as_object_mut()
            .expect("step object")
            .extend(step.as_object().expect("completion object").clone());
        self.write_capture_receipt()
    }

    fn write_capture_receipt(&self) -> Result<(), String> {
        write(
            &self.o.out.join("capture.json"),
            serde_json::to_string_pretty(&self.r.capture)
                .map_err(|error| error.to_string())?
                .as_bytes(),
        )
    }

    async fn checked_exec(&self, lease: &str, argv: &[&str]) -> Result<Exec, String> {
        let output = self.exec(lease, argv).await?;
        if output.exit_code != 0 {
            // Do not echo command output; it may contain input text or IDs.
            return Err(format!(
                "device {} command refused with exit {}",
                argv[0], output.exit_code
            ));
        }
        Ok(output)
    }

    async fn assert_foreground(&self, lease: &str) -> Result<(), String> {
        let activities = self
            .checked_exec(lease, &["dumpsys", "activity", "activities"])
            .await?;
        let windows = self.checked_exec(lease, &["dumpsys", "window"]).await?;
        scenario::foreground(&self.r.package, &activities.stdout, &windows.stdout)
    }

    async fn capture_png(&self, lease: &str) -> Result<Vec<u8>, String> {
        let png = self.screenshot(lease).await?;
        let dimensions = scenario::png_size(&png)?;
        let display = self.o.scenario.as_ref().expect("capture selected").display;
        if !display.matches(dimensions) {
            return Err(format!(
                "native PNG {}x{} differs from requested {}x{}; no input sent or image rescaled",
                dimensions.0, dimensions.1, display.width, display.height
            ));
        }
        Ok(png)
    }
}

fn timestamp_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// The environment the lease is created in: `--env`, else the linked env, else
/// the key's scope — as a generated command resolves a lease's parent.
pub(crate) async fn parent_env(
    client: &Client,
    arg: Option<&str>,
    org: Option<&str>,
) -> Result<String, String> {
    if let Some(v) = arg {
        let full = if v.starts_with("orgs/") {
            v.to_string()
        } else {
            // A slug or an id hangs below the linked project, else the key's.
            let mut defaults = crate::Defaults {
                client,
                link: None,
                org: org.map(str::to_string),
            };
            let project = defaults
                .at("orgs/{org}/projects/{project}")
                .await
                .map_err(why_failure)?;
            format!("{project}/envs/{v}")
        };
        return names::resolve(client, &full).await.map_err(why_failure);
    }
    let mut defaults = crate::Defaults {
        client,
        link: None,
        org: org.map(str::to_string),
    };
    defaults
        .at("orgs/{org}/projects/{project}/envs/{env}")
        .await
        .map_err(why_failure)
}

fn why_failure(f: Failure) -> String {
    match f {
        Failure::Usage(s) | Failure::Refused(s) => s,
        Failure::Api(e) => why(&e),
        Failure::Exit(_) => "stopped".into(),
    }
}

fn report_json(r: &Report) -> Value {
    let mut timings = Map::new();
    for (step, ms) in &r.timings {
        timings.insert((*step).to_string(), json!(ms));
    }
    json!({
        "lease": r.lease,
        "model": r.model,
        "os_version": r.os_version,
        "package": r.package,
        "installed": r.installed,
        "launched": r.launched,
        "crashed": r.crashed,
        "platform_interruption": r.platform_interruption.as_ref().map(|e| json!({
            "kind": e.kind,
            "pid": e.pid,
            "dependency": e.dependency,
            "start_log": e.started,
            "error_log": e.error,
            "kill_log": e.killed,
        })),
        "log_truncation": {"logcat": r.logcat_truncated, "crash": r.crash_log_truncated},
        "screenshots": r.screenshots,
        "timings_ms": Value::Object(timings),
        "error": r.error,
        "capture": r.capture,
    })
}

fn report_text(outcome: &Outcome) -> String {
    serde_json::to_string_pretty(&report_json(&outcome.report)).unwrap_or_default() + "\n"
}

fn write_reports(o: &Opts, outcome: &Outcome) -> Result<(), String> {
    std::fs::create_dir_all(&o.out).map_err(|e| format!("{}: {e}", o.out.display()))?;
    write(&o.out.join("result.json"), report_text(outcome).as_bytes())?;
    // An interrupted run is not a verdict, so it writes no JUnit report.
    if outcome.interrupted {
        return Ok(());
    }
    let r = &outcome.report;
    let suite = format!("device {} {}", r.model, r.os_version);
    write(
        &o.out.join("junit.xml"),
        junit(&suite, &cases(r)).as_bytes(),
    )
}

/// One JUnit testcase: `install`, `launch`, or `no-crash`.
struct Case {
    name: &'static str,
    ok: bool,
    message: String,
    infrastructure: bool,
}

fn cases(r: &Report) -> Vec<Case> {
    let install = if r.installed {
        String::new()
    } else if !r.install_error.is_empty() {
        r.install_error.clone()
    } else {
        "the app was not installed".into()
    };
    let launch = if r.launched {
        String::new()
    } else if !r.launch_error.is_empty() {
        r.launch_error.clone()
    } else if !r.installed {
        "not attempted: the install failed".into()
    } else {
        "the app did not launch".into()
    };
    let mut result = vec![
        Case {
            name: "install",
            ok: r.installed,
            message: install,
            infrastructure: false,
        },
        Case {
            name: "launch",
            ok: r.launched,
            message: launch,
            infrastructure: false,
        },
        Case {
            name: "no-crash",
            ok: !r.crashed && r.platform_interruption.is_none(),
            message: r
                .platform_interruption
                .as_ref()
                .map(|e| e.message())
                .unwrap_or_else(|| r.crash.clone()),
            infrastructure: r.platform_interruption.is_some(),
        },
    ];
    if r.capture.is_some() {
        result.push(Case {
            name: "scenario",
            ok: r.error.is_empty(),
            message: r.error.clone(),
            infrastructure: r.platform_interruption.is_some(),
        });
    }
    result
}

/// The JUnit XML Firebase Test Lab also produces: one testsuite per device.
fn junit(suite: &str, cases: &[Case]) -> String {
    let failures = cases.iter().filter(|c| !c.ok && !c.infrastructure).count();
    let errors = cases.iter().filter(|c| !c.ok && c.infrastructure).count();
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<testsuite name=\"{}\" tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\">\n",
        escape(suite),
        cases.len()
    ));
    for c in cases {
        if c.ok {
            xml.push_str(&format!(
                "  <testcase classname=\"device\" name=\"{}\"/>\n",
                escape(c.name)
            ));
        } else {
            let tag = if c.infrastructure { "error" } else { "failure" };
            xml.push_str(&format!(
                "  <testcase classname=\"device\" name=\"{}\">\n    <{tag} message=\"{}\"/>\n  </testcase>\n",
                escape(c.name),
                escape(&c.message)
            ));
        }
    }
    xml.push_str("</testsuite>\n");
    xml
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// `30m`, `20s`, `1h30m`, `500ms` as a Duration; the server takes the same
/// units, so `wire` sends seconds.
fn parse_duration(s: &str) -> Option<Duration> {
    let mut rest = s.trim();
    let mut ms: u128 = 0;
    let mut any = false;
    while !rest.is_empty() {
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        let n: u128 = rest[..end].parse().ok()?;
        rest = &rest[end..];
        let (unit, len) = if rest.starts_with("ms") {
            (1u128, 2)
        } else if rest.starts_with('s') {
            (1000, 1)
        } else if rest.starts_with('m') {
            (60_000, 1)
        } else if rest.starts_with('h') {
            (3_600_000, 1)
        } else {
            return None;
        };
        ms = ms.checked_add(n.checked_mul(unit)?)?;
        rest = &rest[len..];
        any = true;
    }
    let ms = u64::try_from(ms).ok()?;
    any.then(|| Duration::from_millis(ms))
}

pub(crate) fn duration(s: &str, flag: &str) -> Result<Duration, String> {
    parse_duration(s)
        .ok_or_else(|| format!("`{flag} {s}` is not a duration such as 20s, 5m, 1h30m"))
}

/// The byte ranges of one upload: the first replace, the rest append. An empty
/// file still needs one call, so it exists.
fn chunks(len: usize, chunk: usize) -> Vec<(usize, usize)> {
    if len == 0 {
        return vec![(0, 0)];
    }
    (0..len)
        .step_by(chunk)
        .map(|from| (from, (from + chunk).min(len)))
        .collect()
}

/// A fatal exception in `text` for `package`: the log names it as the process
/// at or after the exception.
fn fatal_exception(text: &str, package: &str) -> bool {
    let Some(at) = text.find("FATAL EXCEPTION") else {
        return false;
    };
    text[at..].contains(&format!("Process: {package}"))
}

fn anr(text: &str, package: &str) -> bool {
    text.contains(&format!("ANR in {package}"))
}

/// What in the logs says the app crashed, if anything.
fn crash_evidence(package: &str, logs: &[(&str, &str)]) -> Option<String> {
    for (what, text) in logs {
        if anr(text, package) {
            return Some(format!("{what} shows ANR in {package}"));
        }
    }
    for (what, text) in logs {
        if fatal_exception(text, package) {
            return Some(format!("{what} shows FATAL EXCEPTION for {package}"));
        }
    }
    None
}

/// The lease's effective model and OS version, from the server's answer.
fn device_of(lease: &sbx::Lease) -> (String, String) {
    let device = lease
        .spec
        .as_ref()
        .and_then(|s| s.device.clone())
        .unwrap_or_default();
    (device.model, device.os_version)
}

fn refused(lease: &sbx::Lease) -> String {
    let why = lease
        .status
        .as_ref()
        .and_then(|s| s.refusal.as_ref())
        .map(|r| r.as_str().to_string())
        .unwrap_or_default();
    if why.is_empty() {
        "the lease was refused".into()
    } else {
        format!("the lease was refused: {why}")
    }
}

fn ended(lease: &sbx::Lease) -> String {
    match lease.status.as_ref().and_then(|s| s.end_reason.as_ref()) {
        Some(r) => format!("the lease ended before it was ready: {}", r.as_str()),
        None => "the lease ended before it was ready".into(),
    }
}

/// One finished device command, its output decoded.
struct Exec {
    exit_code: i32,
    stdout: String,
    stderr: String,
    truncated: bool,
}

impl From<sbx::ExecLeaseResponse> for Exec {
    fn from(r: sbx::ExecLeaseResponse) -> Self {
        Self {
            exit_code: r.exit_code,
            stdout: text(&r.stdout),
            stderr: text(&r.stderr),
            truncated: r.truncated,
        }
    }
}

/// A wire `bytes` field (base64) as text.
fn text(b64: &str) -> String {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The last line of a command's output, for a message.
fn tail(o: &Exec) -> String {
    let text = if o.stderr.trim().is_empty() {
        o.stdout.trim()
    } else {
        o.stderr.trim()
    };
    let line = text.lines().next_back().unwrap_or_default().trim();
    let line: String = line.chars().take(200).collect();
    if line.is_empty() {
        format!("exit code {}", o.exit_code)
    } else {
        line
    }
}

pub(crate) use sylphx_build_lease::{release_request, state, why, wire};

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse() {
        let cases: &[(&str, Option<u64>)] = &[
            ("30m", Some(1800)),
            ("20s", Some(20)),
            ("5s", Some(5)),
            ("1h", Some(3600)),
            ("1h30m", Some(5400)),
            ("2m30s", Some(150)),
            ("0s", Some(0)),
            ("", None),
            ("m", None),
            ("30", None),
            ("30x", None),
            ("-5s", None),
            ("1h30", None),
        ];
        for (text, want) in cases {
            assert_eq!(parse_duration(text).map(|d| d.as_secs()), *want, "`{text}`");
        }
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(wire(Duration::from_secs(1800)), "1800s");
        assert_eq!(wire(Duration::from_millis(500)), "0s");
    }

    #[test]
    fn chunks_cover_every_byte() {
        type Case<'a> = (usize, usize, &'a [(usize, usize)]);
        let cases: &[Case] = &[
            (0, 12, &[(0, 0)]),
            (1, 12, &[(0, 1)]),
            (12, 12, &[(0, 12)]),
            (25, 12, &[(0, 12), (12, 24), (24, 25)]),
        ];
        for (len, chunk, want) in cases {
            let got = chunks(*len, *chunk);
            assert_eq!(&got[..], *want, "{len} bytes in {chunk} chunks");
            assert_eq!(got.first().map(|c| c.0), Some(0));
            assert_eq!(got.last().map(|c| c.1), Some(*len));
            for pair in got.windows(2) {
                assert_eq!(pair[0].1, pair[1].0, "no gap or overlap");
            }
        }
    }

    #[test]
    fn crashes_are_recognized() {
        let package = "com.example.game";
        let fatal = "I/ActivityManager: Start\nE/AndroidRuntime: FATAL EXCEPTION: main\nE/AndroidRuntime: Process: com.example.game, PID: 12\n";
        let cases: &[(&str, bool)] = &[
            (fatal, true),
            ("E/ActivityManager: ANR in com.example.game\n", true),
            (
                "E/AndroidRuntime: FATAL EXCEPTION: main\nE/AndroidRuntime: Process: com.other.app, PID: 9\n",
                false,
            ),
            (
                "Process: com.example.game\nE/AndroidRuntime: FATAL EXCEPTION: main\n",
                false,
            ),
            ("nothing at all", false),
        ];
        for (text, want) in cases {
            assert_eq!(
                crash_evidence(package, &[("logcat", *text)]).is_some(),
                *want,
                "{text}"
            );
        }
        // The crash log counts too, and the evidence names where it was seen.
        let evidence = crash_evidence(package, &[("the crash log", fatal), ("logcat", "quiet")]);
        assert_eq!(
            evidence.as_deref(),
            Some("the crash log shows FATAL EXCEPTION for com.example.game")
        );
    }

    #[test]
    fn junit_names_the_testcases_and_their_failures() {
        let report = Report {
            model: "pixel_7".into(),
            os_version: "14".into(),
            installed: true,
            launched: false,
            launch_error: "the launch failed: Error: monkey aborted".into(),
            ..Default::default()
        };
        let xml = junit("device pixel_7 14", &cases(&report));
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(xml.contains(
            "<testsuite name=\"device pixel_7 14\" tests=\"3\" failures=\"1\" errors=\"0\">"
        ));
        assert!(xml.contains("<testcase classname=\"device\" name=\"install\"/>"));
        assert!(xml.contains("<testcase classname=\"device\" name=\"no-crash\"/>"));
        assert!(xml.contains("<testcase classname=\"device\" name=\"launch\">"));
        assert!(xml.contains("<failure message=\"the launch failed: Error: monkey aborted\"/>"));
        assert!(xml.ends_with("</testsuite>\n"));
    }

    #[test]
    fn exec_output_is_decoded_from_the_wire() {
        assert_eq!(text("U3VjY2Vzcwo="), "Success\n");
        assert_eq!(text("not base64!"), "");
        let o = Exec {
            exit_code: 1,
            stdout: "a\nlast line\n".into(),
            stderr: String::new(),
            truncated: false,
        };
        assert_eq!(tail(&o), "last line");
        let mut wire = sbx::ExecLeaseResponse::default();
        wire.stdout = "cGFydGlhbAo=".into();
        wire.truncated = true;
        let decoded = Exec::from(wire);
        assert_eq!(decoded.stdout, "partial\n");
        assert!(decoded.truncated);
    }

    fn interrupted_log() -> String {
        [
            "10-31 02:08:35.355   526   555 I ActivityManager: Start proc 7255:example.game/u0a190 for next-top-activity {example.game/.Main}",
            "10-31 02:08:36.280   526  2389 E ActivityManager: java.lang.SecurityException: Package example.services is currently frozen!",
            "10-31 02:08:36.364   526  2389 I ActivityManager: Killing 7255:example.game/u0a190 (adj 0): stop example.services due to start failure",
        ].join("\n")
    }

    #[test]
    fn a_witnessed_device_interruption_is_an_infrastructure_verdict() {
        let mut report = Report {
            installed: true,
            launched: true,
            logcat_truncated: true,
            ..Default::default()
        };
        report.diagnose("example.game", true, &interrupted_log(), "");
        assert!(!report.crashed);
        assert_eq!(report.platform_interruption.as_ref().unwrap().pid, 7255);
        let json = report_json(&report);
        assert_eq!(
            json["platform_interruption"]["dependency"],
            "example.services"
        );
        assert_eq!(json["log_truncation"]["logcat"], true);
        let xml = junit("device", &cases(&report));
        assert!(xml.contains("failures=\"0\" errors=\"1\""));
        assert!(xml.contains("<error message="));
        assert!(!xml.contains("<failure"));
        assert_eq!(
            Outcome {
                report,
                infra: None,
                interrupted: false
            }
            .code(),
            2
        );
    }

    #[test]
    fn explicit_app_crashes_and_anrs_keep_the_app_failure_verdict() {
        for crash in [
            "FATAL EXCEPTION: main\nProcess: example.game, PID: 7255\njava.lang.RuntimeException: failed",
            "ANR in example.game",
        ] {
            let mut report = Report { installed: true, launched: true, ..Default::default() };
            report.diagnose("example.game", true, &interrupted_log(), crash);
            assert!(report.crashed);
            assert!(report.platform_interruption.is_none());
            assert!(junit("device", &cases(&report)).contains("failures=\"1\" errors=\"0\""));
            assert_eq!(Outcome { report, infra: None, interrupted: false }.code(), 1);
        }
    }

    #[test]
    fn missing_process_without_causal_evidence_keeps_existing_failure() {
        let mut report = Report {
            installed: true,
            launched: true,
            ..Default::default()
        };
        report.diagnose("example.game", true, "another package was frozen", "");
        assert!(report.crashed);
        assert!(report.platform_interruption.is_none());
        assert!(report.error.contains("process was gone"));
        assert_eq!(
            Outcome {
                report,
                infra: None,
                interrupted: false
            }
            .code(),
            1
        );
    }

    #[test]
    fn live_or_intentionally_stopped_processes_do_not_get_an_interruption_verdict() {
        let mut report = Report {
            installed: true,
            launched: true,
            ..Default::default()
        };
        report.diagnose("example.game", false, &interrupted_log(), "");
        assert!(!report.crashed);
        assert!(report.platform_interruption.is_none());
        assert_eq!(
            Outcome {
                report,
                infra: None,
                interrupted: false
            }
            .code(),
            0
        );
    }

    #[test]
    fn xml_escapes_what_it_must() {
        assert_eq!(escape("a<b&c\"d'e"), "a&lt;b&amp;c&quot;d&apos;e");
    }

    #[test]
    fn a_failed_install_is_not_a_passing_run() {
        let outcome = Outcome {
            report: Report {
                installed: false,
                ..Default::default()
            },
            infra: None,
            interrupted: false,
        };
        assert_eq!(outcome.code(), 1);
        let infra = Outcome {
            report: Report {
                installed: true,
                launched: true,
                ..Default::default()
            },
            infra: Some("the upload failed".into()),
            interrupted: false,
        };
        assert_eq!(infra.code(), 2);
        assert_eq!(Outcome::interrupted().code(), 130);
    }

    #[test]
    fn a_scenario_guard_refusal_is_also_a_failed_junit_case() {
        let report = Report {
            installed: true,
            launched: true,
            capture: Some(json!({"steps":[]})),
            error: "focused window is not the target".into(),
            ..Default::default()
        };
        let xml = junit("capture", &cases(&report));
        assert!(xml.contains("tests=\"4\" failures=\"1\""));
        assert!(xml.contains("name=\"scenario\""));
        assert!(xml.contains("focused window is not the target"));
    }
}
