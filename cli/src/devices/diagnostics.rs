//! Positive Android dependency-interruption evidence from threadtime logcat.
//! A missing process alone cannot identify its cause. Match an exact started
//! process and Android's explicit dependency kill reason; a start-failure
//! diagnosis also requires the same thread's frozen dependency error.

#[derive(Debug)]
pub(super) struct Interruption {
    pub kind: &'static str,
    pub pid: u32,
    pub dependency: String,
    pub started: String,
    pub error: Option<String>,
    pub killed: String,
}

impl Interruption {
    pub fn message(&self) -> String {
        format!(
            "Android interrupted process {} because dependency {} failed ({}); see platform_interruption evidence",
            self.pid, self.dependency, self.kind
        )
    }
}

struct Event<'a> {
    time: u64,
    server: &'a str,
    thread: &'a str,
    level: &'a str,
    message: &'a str,
    raw: &'a str,
}

fn event(line: &str) -> Option<Event<'_>> {
    let (prefix, message) = line.split_once("ActivityManager:")?;
    let fields: Vec<_> = prefix.split_whitespace().collect();
    let [date, clock, server, thread, level] = fields.as_slice() else {
        return None;
    };
    server.parse::<u32>().ok()?;
    thread.parse::<u32>().ok()?;
    Some(Event {
        time: timestamp(date, clock)?,
        server,
        thread,
        level,
        message: message.trim(),
        raw: line,
    })
}

// A logcat threadtime date has no year. A year rollover is deliberately not
// joined: unknown/reordered timestamps must not become causal evidence.
fn timestamp(date: &str, clock: &str) -> Option<u64> {
    let (month, day) = date.split_once('-')?;
    let month: usize = month.parse().ok()?;
    let day: u64 = day.parse().ok()?;
    let days = [31u64, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&month) || day == 0 || day > days[month - 1] {
        return None;
    }
    let mut parts = clock.split(':');
    let hour: u64 = parts.next()?.parse().ok()?;
    let minute: u64 = parts.next()?.parse().ok()?;
    let (second, millis) = parts.next()?.split_once('.')?;
    let second: u64 = second.parse().ok()?;
    if parts.next().is_some() || millis.len() != 3 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let millis: u64 = millis.parse().ok()?;
    Some(
        (((days[..month - 1].iter().sum::<u64>() + day) * 24 + hour) * 60 + minute) * 60_000
            + second * 1_000
            + millis,
    )
}

fn process<'a>(text: &'a str, package: &str) -> Option<(u32, &'a str)> {
    let token = text.split_whitespace().next()?;
    let (pid, rest) = token.split_once(':')?;
    let (name, user) = rest.split_once('/')?;
    if name != package || user.is_empty() {
        return None;
    }
    Some((pid.parse().ok()?, token))
}

/// Only use for a missing launcher process after successfully clearing logcat.
/// A later start of that package invalidates an earlier kill. App-crash/ANR
/// evidence is handled by the caller and takes precedence over this diagnosis.
pub(super) fn dependency_interruption(package: &str, log: &str) -> Option<Interruption> {
    let mut started: Option<(Event<'_>, u32, &str)> = None;
    let mut frozen: Option<(Event<'_>, &str)> = None;
    let mut result = None;
    for line in log.lines() {
        let Some(e) = event(line) else { continue };
        if let Some(text) = e.message.strip_prefix("Start proc ") {
            if let Some((pid, identity)) = process(text, package) {
                started = Some((e, pid, identity));
                frozen = None;
                result = None;
            }
            continue;
        }
        let Some((start, pid, identity)) = started.as_ref() else {
            continue;
        };
        if e.server != start.server || e.time < start.time {
            continue;
        }
        if e.level == "E" {
            if let Some(dependency) = e
                .message
                .strip_prefix("java.lang.SecurityException: Package ")
                .and_then(|s| s.strip_suffix(" is currently frozen!"))
                .filter(|s| !s.is_empty() && !s.chars().any(char::is_whitespace))
            {
                frozen = Some((e, dependency));
            }
            continue;
        }
        let Some(text) = e.message.strip_prefix("Killing ") else {
            continue;
        };
        if e.level != "I" || process(text, package) != Some((*pid, *identity)) {
            continue;
        }
        if let Some(reason) = text.split_once("): depends on provider ").map(|(_, s)| s) {
            if let Some((provider, dying)) = reason.split_once(" in dying proc ") {
                let dependency = dying.split_whitespace().next().unwrap_or_default();
                if !dependency.is_empty()
                    && provider
                        .split_once('/')
                        .is_some_and(|(p, component)| p == dependency && !component.is_empty())
                {
                    result = Some(Interruption {
                        kind: "dependency_died",
                        pid: *pid,
                        dependency: dependency.into(),
                        started: start.raw.into(),
                        error: None,
                        killed: e.raw.into(),
                    });
                }
            }
            continue;
        }
        let Some((error, dependency)) = frozen.as_ref() else {
            continue;
        };
        // Android's same system-server thread must name the same dependency
        // in the explicit kill reason, immediately after the frozen error.
        let reason = format!(": stop {dependency} due to start failure");
        if e.server == error.server
            && e.thread == error.thread
            && e.time.checked_sub(error.time).is_some_and(|ms| ms <= 2_000)
            && text.ends_with(&reason)
        {
            result = Some(Interruption {
                kind: "frozen_dependency_start_failure",
                pid: *pid,
                dependency: (*dependency).into(),
                started: start.raw.into(),
                error: Some(error.raw.into()),
                killed: e.raw.into(),
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const PACKAGE: &str = "example.game";
    const START: &str = "10-31 02:08:35.355   526   555 I ActivityManager: Start proc 7255:example.game/u0a190 for next-top-activity {example.game/.Main}";
    const ERROR: &str = "10-31 02:08:36.280   526  2389 E ActivityManager: java.lang.SecurityException: Package example.services is currently frozen!";
    const KILL: &str = "10-31 02:08:36.364   526  2389 I ActivityManager: Killing 7255:example.game/u0a190 (adj 0): stop example.services due to start failure";
    fn log() -> String {
        [START, ERROR, KILL].join("\n")
    }

    #[test]
    fn joins_exact_process_dependency_thread_and_time() {
        let e = dependency_interruption(PACKAGE, &log()).unwrap();
        assert_eq!(e.pid, 7255);
        assert_eq!(e.dependency, "example.services");
        assert_eq!(e.started, START);
        assert_eq!(e.error.as_deref(), Some(ERROR));
        assert_eq!(e.killed, KILL);
    }

    #[test]
    fn refuses_missing_stale_or_unrelated_evidence() {
        for invalid in [
            [ERROR, KILL].join("\n"),
            [START, KILL].join("\n"),
            log().replace("Killing 7255:", "Killing 7256:"),
            log().replace(
                "Killing 7255:example.game/",
                "Killing 7255:example.game.other/",
            ),
            log().replace("stop example.services", "stop other.services"),
            log().replace("526  2389 I", "526  2390 I"),
            log().replace("526  2389 I", "527  2389 I"),
            log().replace("02:08:36.364", "02:08:39.364"),
            log().replace("02:08:36.364", "02:08:35.364"),
            log().replace("02:08:36.364", "bad-time"),
            [ERROR, START, KILL].join("\n"),
            format!(
                "{}\n{}",
                log(),
                START.replace("7255", "8000").replace("35.355", "40.355")
            ),
        ] {
            assert!(
                dependency_interruption(PACKAGE, &invalid).is_none(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn ordinary_android_kills_are_not_dependency_interruptions() {
        for reason in [
            "low memory",
            "crash",
            "user request",
            "stop example.services",
        ] {
            let text = log().replace("stop example.services due to start failure", reason);
            assert!(dependency_interruption(PACKAGE, &text).is_none());
        }
    }

    #[test]
    fn joins_an_explicit_dying_provider_kill_to_the_started_process() {
        let kill = "10-31 02:08:36.364   526  1738 I ActivityManager: Killing 7255:example.game/u0a190 (adj 0): depends on provider example.services/.fonts.Provider in dying proc example.services (adj -10000)";
        let log = [START, kill].join("\n");
        let evidence = dependency_interruption(PACKAGE, &log).unwrap();
        assert_eq!(evidence.kind, "dependency_died");
        assert_eq!(evidence.dependency, "example.services");
        assert!(evidence.error.is_none());
        for invalid in [
            kill.to_string(),
            log.replace("Killing 7255:", "Killing 8000:"),
            log.replace(
                "dying proc example.services",
                "dying proc unrelated.services",
            ),
            log.replace("example.services/.fonts.Provider", "example.services/"),
            log.replace("02:08:36.364", "02:08:34.364"),
        ] {
            assert!(
                dependency_interruption(PACKAGE, &invalid).is_none(),
                "{invalid}"
            );
        }
    }
}
