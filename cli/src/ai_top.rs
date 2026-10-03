//! `sylphx ai top`: the view logic, with no I/O. It reads what
//! `GET /v1/operator/seats` returns (a list of seats, each with its
//! capacity windows) plus a local history of weekly-window readings, and
//! produces seat counts, the earliest reset, runway and accounts needed at
//! the 24h and 6h pace, and the red lines for anything that needs a person.
//! `ai_top_run.rs` does the fetching, the history file and the refresh loop.
//!
//! Semantics follow `janus capacity`: one seat supplies 100/168 % per hour of
//! its weekly window; demand is the measured weekly-quota burn (the positive
//! rises between readings) divided by the hours covered; the runway is
//! simulated through each seat's own reset, not divided; nothing is guessed
//! without history. The seats API has no plan tier, so every seat weighs 1.
//!
//! Not in the seats API yet, so shown as `n/a` and listed in `MISSING`:
//! sessions and subagents (seat, model, effort, cache hit) and the
//! API-equivalent value; both wait for the per-request receipts (W-1611).

use std::collections::BTreeMap;

use serde_json::{json, Value};

pub const WEEK_H: f64 = 168.0;
/// % per hour of its weekly window one seat supplies.
pub const SUPPLY_7D: f64 = 100.0 / WEEK_H;
/// Buy for the average plus this much: load is bursty, seats get held.
pub const HEADROOM: f64 = 1.25;
/// Less history than this says nothing about a pace.
pub const MIN_SPAN_H: f64 = 6.0;
const HORIZON_H: f64 = 14.0 * 24.0;
const STEP_H: f64 = 0.25;
/// A seat whose newest reading is older than this is flagged.
pub const STALE_S: i64 = 3 * 3600;
/// Two readings further apart than this are not paired for burn.
const MAX_PAIR_GAP_S: i64 = 2 * 3600;
/// History keeps a reading at most this often.
pub const MIN_SAMPLE_GAP_S: i64 = 240;

/// What the view needs and the API does not carry yet.
pub const MISSING: &[&str] = &[
    "sessions and subagents: session id, agent id, serving seat, model, effective effort, cache hit (W-1611 receipts)",
    "API-equivalent value: per-request token and price ledger, /v1/usage is deleted (W-1611)",
    "seat plan tier: needed to weigh a mixed pool (every seat counts as 1)",
];

// ---- time -------------------------------------------------------------

/// `2026-10-03T14:20:00.000Z` (or `+00:00`) to epoch seconds.
pub fn parse_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, sec) = (n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut t = days * 86_400 + h * 3600 + mi * 60 + sec;
    // A numeric offset after the seconds (fraction skipped).
    let rest = s[19..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if let Some(sign) = rest.chars().next().filter(|c| matches!(c, '+' | '-')) {
        let oh: i64 = rest.get(1..3)?.parse().ok()?;
        let om: i64 = rest.get(4..6).and_then(|v| v.parse().ok()).unwrap_or(0);
        let off = oh * 3600 + om * 60;
        t -= if sign == '+' { off } else { -off };
    }
    Some(t)
}

/// Epoch seconds as `2026-10-03 14:20Z`.
pub fn fmt_time(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60
    )
}

/// `3h12m`, `45m`, `2d4h`.
pub fn fmt_span(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 86_400 {
        format!("{}d{}h", s / 86_400, s % 86_400 / 3600)
    } else if s >= 3600 {
        format!("{}h{:02}m", s / 3600, s % 3600 / 60)
    } else {
        format!("{}m", s / 60)
    }
}

// ---- the seats API ----------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub key: String,
    /// Fraction of the window used (1.0 is full).
    pub utilization: Option<f64>,
    pub seconds: Option<i64>,
    pub reset: Option<i64>,
    pub observed: Option<i64>,
}

impl Window {
    fn lower(&self) -> String {
        self.key.to_ascii_lowercase()
    }
    pub fn is_weekly(&self) -> bool {
        let k = self.lower();
        self.seconds == Some(604_800) || k.contains("week") || k.contains("7d")
    }
    pub fn is_five_hour(&self) -> bool {
        self.seconds == Some(18_000) || self.lower().contains("5h")
    }
    /// The window still counts: it has not reset yet (no reset time counts).
    pub fn current(&self, now: i64) -> bool {
        self.reset.is_none_or(|r| r > now)
    }
    pub fn full(&self, now: i64) -> bool {
        self.current(now) && self.utilization.is_some_and(|u| u >= 1.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Seat {
    pub id: String,
    pub provider: String,
    /// `capacity_profiles.status`: active, draining, reauth_required,
    /// subscription_required, quarantined, disabled.
    pub state: String,
    pub last_success: Option<i64>,
    pub windows: Vec<Window>,
}

impl Seat {
    pub fn weekly(&self) -> Option<&Window> {
        self.windows.iter().find(|w| w.is_weekly())
    }
    pub fn five_hour(&self) -> Option<&Window> {
        self.windows.iter().find(|w| w.is_five_hour())
    }
    fn newest_reading(&self) -> Option<i64> {
        self.windows.iter().filter_map(|w| w.observed).max()
    }
}

/// The body of `GET /v1/operator/seats`.
pub fn parse_seats(v: &Value) -> Result<Vec<Seat>, String> {
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or("the seats answer has no `data` list")?;
    let time = |x: &Value| x.as_str().and_then(parse_time);
    Ok(data
        .iter()
        .map(|s| Seat {
            id: s["id"].as_str().unwrap_or("?").to_string(),
            provider: s["provider"].as_str().unwrap_or("?").to_string(),
            state: s["state"].as_str().unwrap_or("?").to_string(),
            last_success: time(&s["last_success_at"]),
            windows: s["windows"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|w| Window {
                    key: w["window_key"].as_str().unwrap_or("").to_string(),
                    utilization: w["utilization"].as_f64(),
                    seconds: w["limit_window_seconds"].as_i64(),
                    reset: time(&w["reset_at"]),
                    observed: time(&w["observed_at"]),
                })
                .collect(),
        })
        .collect())
}

// ---- classification ---------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Usable,
    /// A routing window is full; `back` is when the last full one resets.
    Spent {
        back: Option<i64>,
    },
    /// Not in rotation; `person` when only a human can fix it.
    Out {
        reason: String,
        person: bool,
    },
}

pub fn classify(seat: &Seat, now: i64) -> Status {
    match seat.state.as_str() {
        "active" => {
            let full: Vec<&Window> = seat.windows.iter().filter(|w| w.full(now)).collect();
            let cooling = seat
                .windows
                .iter()
                .any(|w| w.key == "seat_429" && w.reset.is_some_and(|r| r > now));
            if full.is_empty() && !cooling {
                Status::Usable
            } else {
                let back = full.iter().filter_map(|w| w.reset).max();
                Status::Spent { back }
            }
        }
        "draining" => Status::Out {
            reason: "draining".into(),
            person: false,
        },
        "disabled" => Status::Out {
            reason: "disabled".into(),
            person: false,
        },
        "reauth_required" => Status::Out {
            reason: "login needed (reauth_required)".into(),
            person: true,
        },
        "subscription_required" => Status::Out {
            reason: "on hold (subscription_required)".into(),
            person: true,
        },
        "quarantined" => Status::Out {
            reason: "alarm: quarantined".into(),
            person: true,
        },
        other => Status::Out {
            reason: format!("alarm: unknown state `{other}`"),
            person: true,
        },
    }
}

fn in_rotation(s: &Status) -> bool {
    !matches!(s, Status::Out { .. })
}

// ---- history ----------------------------------------------------------

/// One reading of every seat's weekly utilisation.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub t: i64,
    pub util7: BTreeMap<String, f64>,
}

pub fn history_from_json(v: &Value) -> Vec<Sample> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let t = s.get("t")?.as_i64()?;
            let util7 = s
                .get("u")?
                .as_object()?
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_f64()?)))
                .collect();
            Some(Sample { t, util7 })
        })
        .collect()
}

pub fn history_to_json(h: &[Sample]) -> Value {
    Value::Array(h.iter().map(|s| json!({"t": s.t, "u": s.util7})).collect())
}

/// Adds this reading (at most one per `MIN_SAMPLE_GAP_S`) and forgets what is
/// older than a week. Returns whether the history changed.
pub fn record(history: &mut Vec<Sample>, seats: &[Seat], now: i64) -> bool {
    let before = history.len();
    history.retain(|s| s.t > now - 7 * 86_400 && s.t <= now + 3600);
    let mut changed = history.len() != before;
    if history.last().is_none_or(|l| now - l.t >= MIN_SAMPLE_GAP_S) {
        let util7: BTreeMap<String, f64> = seats
            .iter()
            .filter_map(|s| {
                Some((
                    s.id.clone(),
                    s.weekly().filter(|w| w.current(now))?.utilization?,
                ))
            })
            .collect();
        if !util7.is_empty() {
            history.push(Sample { t: now, util7 });
            changed = true;
        }
    }
    changed
}

/// Hours the history covers (at most a week) and the pool's summed positive
/// weekly-window rises, in percent, over the trailing 24h and 6h. A fall (a
/// reset) or a gap over two hours is skipped, as in Janus.
pub fn burn(history: &[Sample], now: i64) -> (f64, f64, f64) {
    let span = match (history.first(), history.last()) {
        (Some(a), Some(b)) => ((b.t - a.t) as f64 / 3600.0).clamp(0.0, WEEK_H),
        _ => 0.0,
    };
    let (mut h24, mut h6) = (0.0, 0.0);
    for w in history.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if b.t - a.t > MAX_PAIR_GAP_S || b.t <= now - 24 * 3600 {
            continue;
        }
        for (id, u1) in &b.util7 {
            let Some(u0) = a.util7.get(id) else { continue };
            let rise = (u1 - u0) * 100.0;
            if rise <= 0.0 {
                continue;
            }
            h24 += rise;
            if b.t > now - 6 * 3600 {
                h6 += rise;
            }
        }
    }
    (span, h24, h6)
}

// ---- forecast ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Runway {
    Unknown,
    /// Refills keep up for the whole horizon.
    Sustainable,
    /// Demand first exceeds what the pool can serve at this epoch.
    DryAt(i64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pace {
    pub label: &'static str,
    /// Weekly-window % per hour, in seat units.
    pub demand: Option<f64>,
    /// Seats the demand needs, before headroom.
    pub need: Option<f64>,
    /// Seats to hold, with headroom.
    pub recommend: Option<usize>,
    /// Seats to add to reach `recommend`.
    pub to_add: Option<usize>,
    pub runway: Runway,
}

struct Sim {
    left: f64,
    reset: f64,
}

/// Steps forward until demand `d` (weekly % per hour) cannot be served. Each
/// rotation seat has its remaining weekly room now and a full window at its
/// own reset, spent soonest-reset first.
fn simulate(seats: &[(&Seat, &Status)], d: f64, now: i64) -> Runway {
    if d <= 0.0 {
        return Runway::Sustainable;
    }
    let mut sims: Vec<Sim> = seats
        .iter()
        .filter(|(_, st)| in_rotation(st))
        .map(|(s, _)| {
            let w = s.weekly();
            let left = match w {
                Some(w) if !w.current(now) => 100.0,
                Some(w) => (1.0 - w.utilization.unwrap_or(0.0)).max(0.0) * 100.0,
                None => 100.0,
            };
            let reset = w
                .and_then(|w| w.reset)
                .filter(|r| *r > now)
                .map_or(now as f64 + WEEK_H * 3600.0, |r| r as f64);
            Sim { left, reset }
        })
        .collect();
    if sims.is_empty() {
        return Runway::DryAt(now);
    }
    let mut t = now as f64;
    let end = t + HORIZON_H * 3600.0;
    while t < end {
        for a in sims.iter_mut() {
            while a.reset <= t {
                a.left = 100.0;
                a.reset += WEEK_H * 3600.0;
            }
        }
        sims.sort_by(|x, y| x.reset.total_cmp(&y.reset));
        let mut need = d * STEP_H;
        for a in sims.iter_mut() {
            if need <= 1e-9 {
                break;
            }
            let x = need.min(a.left);
            a.left -= x;
            need -= x;
        }
        if need > 1e-6 {
            return Runway::DryAt(t as i64);
        }
        t += STEP_H * 3600.0;
    }
    Runway::Sustainable
}

fn pace(
    label: &'static str,
    demand: Option<f64>,
    supply: usize,
    seats: &[(&Seat, &Status)],
    now: i64,
) -> Pace {
    let need = demand.map(|d| d / SUPPLY_7D);
    let recommend = need.map(|n| (n * HEADROOM).ceil().max(1.0) as usize);
    Pace {
        label,
        demand,
        need,
        recommend,
        to_add: recommend.map(|r| r.saturating_sub(supply)),
        runway: demand.map_or(Runway::Unknown, |d| simulate(seats, d, now)),
    }
}

// ---- the view ---------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SeatRow {
    pub seat: Seat,
    pub status: Status,
    /// Newest reading is older than `STALE_S` (or there is none).
    pub stale: bool,
}

#[derive(Debug, Clone)]
pub struct View {
    pub now: i64,
    pub rows: Vec<SeatRow>,
    pub total: usize,
    pub usable: usize,
    pub spent: usize,
    /// In rotation: usable plus spent.
    pub supply: usize,
    pub out: usize,
    /// Next time a spent seat comes back, and which.
    pub next_refill: Option<(i64, String)>,
    /// Earliest weekly reset among seats in rotation.
    pub next_weekly_reset: Option<(i64, String)>,
    pub span_h: f64,
    pub paces: [Pace; 2],
    pub red: Vec<String>,
}

pub fn build(seats: &[Seat], history: &[Sample], now: i64) -> View {
    let rows: Vec<SeatRow> = seats
        .iter()
        .map(|s| {
            let status = classify(s, now);
            let stale = in_rotation(&status)
                && s.state == "active"
                && s.newest_reading().is_none_or(|t| now - t > STALE_S);
            SeatRow {
                seat: s.clone(),
                status,
                stale,
            }
        })
        .collect();
    let count = |f: &dyn Fn(&Status) -> bool| rows.iter().filter(|r| f(&r.status)).count();
    let usable = count(&|s| matches!(s, Status::Usable));
    let spent = count(&|s| matches!(s, Status::Spent { .. }));
    let out = count(&|s| matches!(s, Status::Out { .. }));
    let supply = usable + spent;

    let next_refill = rows
        .iter()
        .filter_map(|r| match r.status {
            Status::Spent { back: Some(b) } => Some((b, r.seat.id.clone())),
            _ => None,
        })
        .min_by_key(|x| x.0);
    let next_weekly_reset = rows
        .iter()
        .filter(|r| in_rotation(&r.status))
        .filter_map(|r| {
            Some((
                r.seat.weekly()?.reset.filter(|x| *x > now)?,
                r.seat.id.clone(),
            ))
        })
        .min_by_key(|x| x.0);

    let (span_h, h24, h6) = burn(history, now);
    let rate = |sum: f64, h: f64| (span_h >= MIN_SPAN_H).then(|| sum / span_h.min(h));
    let pairs: Vec<(&Seat, &Status)> = rows.iter().map(|r| (&r.seat, &r.status)).collect();
    let paces = [
        pace("24h", rate(h24, 24.0), supply, &pairs, now),
        pace("6h", rate(h6, 6.0), supply, &pairs, now),
    ];

    let mut red = Vec::new();
    for r in &rows {
        match &r.status {
            Status::Out {
                reason,
                person: true,
            } => red.push(format!(
                "seat {} ({}): {reason}",
                r.seat.id, r.seat.provider
            )),
            _ if r.stale => red.push(format!(
                "seat {} ({}): alarm: no fresh reading{}",
                r.seat.id,
                r.seat.provider,
                r.seat
                    .newest_reading()
                    .map_or(String::new(), |t| format!(" for {}", fmt_span(now - t)))
            )),
            _ => {}
        }
    }
    if rows.is_empty() {
        red.push("alarm: the gateway lists no seats".into());
    } else if usable == 0 {
        red.push("alarm: no usable seat right now".into());
    }
    for p in &paces {
        if let Runway::DryAt(t) = p.runway {
            red.push(format!(
                "alarm: pool runs dry at {} ({} from now) at the {} pace",
                fmt_time(t),
                fmt_span(t - now),
                p.label
            ));
        }
    }
    if let Some(n) = paces[0].to_add.filter(|n| *n > 0) {
        red.push(format!(
            "alarm: add {n} seat(s): the 24h pace needs {}",
            paces[0].recommend.unwrap_or(0)
        ));
    }

    View {
        now,
        total: rows.len(),
        usable,
        spent,
        supply,
        out,
        next_refill,
        next_weekly_reset,
        span_h,
        paces,
        red,
        rows,
    }
}

// ---- output -----------------------------------------------------------

fn runway_json(r: &Runway) -> Value {
    match r {
        Runway::Unknown => json!({"kind": "unknown"}),
        Runway::Sustainable => json!({"kind": "sustainable"}),
        Runway::DryAt(t) => json!({"kind": "dry_at", "at": fmt_time(*t), "epoch": t}),
    }
}

impl View {
    pub fn to_json(&self) -> Value {
        let at = |x: &Option<(i64, String)>| {
            x.as_ref()
                .map(|(t, id)| json!({"at": fmt_time(*t), "epoch": t, "seat": id}))
        };
        json!({
            "generated_at": fmt_time(self.now),
            "seats": {
                "total": self.total, "usable": self.usable, "spent": self.spent,
                "in_rotation": self.supply, "out_of_rotation": self.out,
            },
            "next_refill": at(&self.next_refill),
            "next_weekly_reset": at(&self.next_weekly_reset),
            "history_span_hours": self.span_h,
            "pace": self.paces.iter().map(|p| json!({
                "window": p.label, "demand_percent_per_hour": p.demand,
                "seats_needed": p.need, "seats_recommended": p.recommend,
                "seats_to_add": p.to_add, "runway": runway_json(&p.runway),
            })).collect::<Vec<_>>(),
            "seat_list": self.rows.iter().map(|r| json!({
                "id": r.seat.id, "provider": r.seat.provider, "state": r.seat.state,
                "status": match &r.status {
                    Status::Usable => "usable", Status::Spent { .. } => "spent", Status::Out { .. } => "out",
                },
                "five_hour_percent": r.seat.five_hour().and_then(|w| w.utilization).map(|u| u * 100.0),
                "weekly_percent": r.seat.weekly().and_then(|w| w.utilization).map(|u| u * 100.0),
                "weekly_reset": r.seat.weekly().and_then(|w| w.reset).map(fmt_time),
                "back_at": match r.status { Status::Spent { back: Some(b) } => Some(fmt_time(b)), _ => None },
                "stale": r.stale,
            })).collect::<Vec<_>>(),
            "red": self.red,
            "sessions": Value::Null,
            "api_equivalent_value": Value::Null,
            "missing_api_fields": MISSING,
        })
    }

    /// The screen. `color` paints red lines red.
    pub fn render(&self, color: bool) -> String {
        let now = self.now;
        let paint = |s: &str, code: &str| {
            if color {
                format!("\x1b[{code}m{s}\x1b[0m")
            } else {
                s.to_string()
            }
        };
        let pct = |w: Option<&Window>| {
            w.and_then(|w| w.utilization)
                .map_or("n/a".to_string(), |u| format!("{:.0}%", u * 100.0))
        };
        let mut o = String::new();
        o.push_str(&format!("sylphx ai top   {}\n\n", fmt_time(now)));
        o.push_str(&format!(
            "SEATS   {} usable / {} spent / {} out   ({} total, {} in rotation)\n",
            self.usable, self.spent, self.out, self.total, self.supply
        ));
        let when = |x: &Option<(i64, String)>| match x {
            Some((t, id)) => format!("{} (in {}, seat {id})", fmt_time(*t), fmt_span(t - now)),
            None => "none".to_string(),
        };
        o.push_str(&format!(
            "REFILL  next spent seat back: {}\n",
            when(&self.next_refill)
        ));
        o.push_str(&format!(
            "RESET   earliest weekly reset: {}\n\n",
            when(&self.next_weekly_reset)
        ));

        o.push_str("CAPACITY\n");
        if self.span_h < MIN_SPAN_H {
            o.push_str(&format!(
                "  pace n/a: {:.1}h of readings kept locally, need {MIN_SPAN_H:.0}h (keep `sylphx ai top` or `--once` running)\n",
                self.span_h
            ));
        }
        for p in &self.paces {
            let f = |v: Option<f64>, unit: &str| {
                v.map_or("n/a".to_string(), |v| format!("{v:.2}{unit}"))
            };
            let runway = match &p.runway {
                Runway::Unknown => "n/a".to_string(),
                Runway::Sustainable => "sustainable (14d+)".to_string(),
                Runway::DryAt(t) => format!("dry at {} (in {})", fmt_time(*t), fmt_span(t - now)),
            };
            o.push_str(&format!(
                "  {:>3} pace  demand {:>9}  seats needed {:>6}  hold {:>3}  add {:>3}  runway {}\n",
                p.label,
                f(p.demand, "%/h"),
                f(p.need, ""),
                p.recommend.map_or("n/a".into(), |v| v.to_string()),
                p.to_add.map_or("n/a".into(), |v| v.to_string()),
                runway
            ));
        }

        o.push_str("\nSEAT                          PROVIDER     STATE            5H    WEEK  WEEK RESET          NOTE\n");
        for r in &self.rows {
            let s = &r.seat;
            let note = match &r.status {
                Status::Usable => String::new(),
                Status::Spent { back: Some(b) } => format!("spent, back in {}", fmt_span(b - now)),
                Status::Spent { back: None } => "spent".into(),
                Status::Out { reason, .. } => reason.clone(),
            };
            let note = if r.stale {
                format!("{note} stale").trim().to_string()
            } else {
                note
            };
            o.push_str(&format!(
                "{:<29} {:<12} {:<15} {:>5} {:>6}  {:<19} {}\n",
                trunc(&s.id, 29),
                trunc(&s.provider, 12),
                trunc(&s.state, 15),
                pct(s.five_hour()),
                pct(s.weekly()),
                s.weekly()
                    .and_then(|w| w.reset)
                    .map_or("n/a".into(), fmt_time),
                note
            ));
        }

        o.push_str("\nSESSIONS AND SUBAGENTS   seat / model / effort / cache hit: n/a (not in the gateway API yet, W-1611)\n");
        o.push_str("API-EQUIVALENT VALUE     n/a (not in the gateway API yet, W-1611)\n");

        o.push('\n');
        if self.red.is_empty() {
            o.push_str("Nothing needs a person.\n");
        } else {
            for line in &self.red {
                o.push_str(&paint(&format!("RED  {line}"), "1;31"));
                o.push('\n');
            }
        }
        o
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).chain(std::iter::once('~')).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_789_999_980;
    const H: i64 = 3600;

    fn win(key: &str, util: f64, reset_in: i64, seen_ago: i64) -> Value {
        json!({
            "window_key": key, "utilization": util, "used_percent": util * 100.0,
            "limit_window_seconds": if key.contains("5h") { json!(18000) } else { json!(604800) },
            "reset_at": fmt_time_full(NOW + reset_in), "observed_at": fmt_time_full(NOW - seen_ago),
            "limit_reached": util >= 1.0, "state": "AVAILABLE",
        })
    }

    fn fmt_time_full(t: i64) -> String {
        format!("{}:00.000Z", fmt_time(t).trim_end_matches('Z')).replace(' ', "T")
    }

    fn seat(id: &str, state: &str, windows: Vec<Value>) -> Value {
        json!({"id": id, "provider": "claude", "state": state, "last_success_at": null,
               "quota_pressure": null, "windows": windows})
    }

    /// A seat `into_h` hours into its weekly window at `u7`.
    fn weekly_seat(id: &str, u7: f64, into_h: i64) -> Value {
        seat(
            id,
            "active",
            vec![win("claude_7d", u7, (168 - into_h) * H, 60)],
        )
    }

    fn seats(v: Vec<Value>) -> Vec<Seat> {
        parse_seats(&json!({"object": "list", "data": v})).unwrap()
    }

    fn sample(t: i64, u: &[(&str, f64)]) -> Sample {
        Sample {
            t,
            util7: u.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    #[test]
    fn time_round_trips_and_reads_offsets() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_time("2026-10-03T14:20:05.123Z")
                .map(fmt_time)
                .as_deref(),
            Some("2026-10-03 14:20Z")
        );
        assert_eq!(
            parse_time("2026-10-03T15:20:00+01:00"),
            parse_time("2026-10-03T14:20:00Z")
        );
        assert_eq!(
            parse_time("2024-02-29T00:00:00Z").map(fmt_time).as_deref(),
            Some("2024-02-29 00:00Z")
        );
        assert_eq!(parse_time("garbage"), None);
        assert_eq!(fmt_span(3 * H + 12 * 60), "3h12m");
        assert_eq!(fmt_span(26 * H), "1d2h");
    }

    #[test]
    fn parses_the_seats_projection_and_refuses_a_foreign_body() {
        let s = seats(vec![seat(
            "s1",
            "active",
            vec![
                win("claude_5h", 0.2, 3600, 30),
                win("claude_7d", 0.5, 80 * H, 30),
            ],
        )]);
        assert_eq!(s[0].five_hour().unwrap().utilization, Some(0.2));
        assert_eq!(s[0].weekly().unwrap().reset, Some(NOW + 80 * H));
        assert!(parse_seats(&json!({"error": "x"})).is_err());
    }

    #[test]
    fn classifies_usable_spent_and_out_of_rotation() {
        let s = seats(vec![
            seat("ok", "active", vec![win("claude_7d", 0.4, 50 * H, 60)]),
            seat(
                "full",
                "active",
                vec![
                    win("claude_5h", 1.0, 2 * H, 60),
                    win("claude_7d", 0.5, 50 * H, 60),
                ],
            ),
            seat("old-full", "active", vec![win("claude_5h", 1.0, -H, 60)]),
            seat("login", "reauth_required", vec![]),
            seat("hold", "subscription_required", vec![]),
            seat("bad", "quarantined", vec![]),
            seat("off", "disabled", vec![]),
        ]);
        let st: Vec<Status> = s.iter().map(|x| classify(x, NOW)).collect();
        assert_eq!(st[0], Status::Usable);
        assert_eq!(
            st[1],
            Status::Spent {
                back: Some(NOW + 2 * H)
            }
        );
        assert_eq!(
            st[2],
            Status::Usable,
            "a window past its reset no longer binds"
        );
        assert!(matches!(&st[3], Status::Out { person: true, .. }));
        assert!(matches!(&st[4], Status::Out { person: true, .. }));
        assert!(matches!(&st[5], Status::Out { person: true, .. }));
        assert!(matches!(&st[6], Status::Out { person: false, .. }));
    }

    #[test]
    fn earliest_reset_and_refill_come_from_the_right_seats() {
        let s = seats(vec![
            seat("a", "active", vec![win("claude_7d", 1.0, 30 * H, 60)]),
            seat(
                "b",
                "active",
                vec![
                    win("claude_5h", 1.0, H, 60),
                    win("claude_7d", 0.5, 90 * H, 60),
                ],
            ),
            seat("c", "active", vec![win("claude_7d", 0.1, 20 * H, 60)]),
            seat("d", "disabled", vec![win("claude_7d", 0.1, 2 * H, 60)]),
        ]);
        let v = build(&s, &[], NOW);
        assert_eq!(v.next_refill, Some((NOW + H, "b".into())));
        assert_eq!(
            v.next_weekly_reset,
            Some((NOW + 20 * H, "c".into())),
            "a disabled seat's reset does not count"
        );
        assert_eq!((v.usable, v.spent, v.out, v.supply), (1, 2, 1, 3));
    }

    #[test]
    fn demand_is_the_measured_burn_over_the_covered_hours() {
        let s = seats(vec![weekly_seat("a", 0.5, 84)]);
        // 12 hours of hourly readings; 1% of the window each hour.
        let h: Vec<Sample> = (0..=12)
            .map(|i| sample(NOW - (12 - i) * H, &[("a", 0.38 + 0.01 * i as f64)]))
            .collect();
        let v = build(&s, &h, NOW);
        assert!((v.span_h - 12.0).abs() < 1e-9);
        // 12 points in 12h: the 24h pace divides by the 12h covered.
        let d24 = v.paces[0].demand.unwrap();
        assert!((d24 - 1.0).abs() < 1e-6, "{d24}");
        assert!(
            (v.paces[1].demand.unwrap() - 1.0).abs() < 1e-6,
            "6h pace is 1%/h too"
        );
        // 1%/h needs 1.68 seats; with headroom 3 to hold; one in rotation, so add 2.
        assert!((v.paces[0].need.unwrap() - 1.68).abs() < 1e-6);
        assert_eq!(v.paces[0].recommend, Some(3));
        assert_eq!(v.paces[0].to_add, Some(2));
        // 1%/h beats one seat's 0.595%/h: it runs dry, and that is a red line.
        assert!(matches!(v.paces[0].runway, Runway::DryAt(_)));
        assert!(v
            .red
            .iter()
            .any(|l| l.contains("runs dry") && l.contains("24h")));
        assert!(v.red.iter().any(|l| l.contains("add 2 seat")));
    }

    #[test]
    fn six_hour_pace_can_differ_from_the_day() {
        let s = seats(vec![weekly_seat("a", 0.5, 84)]);
        // Idle for 18h, then 6%/h... 1% per hour for the last 6 hours only.
        let mut h: Vec<Sample> = (0..=18)
            .map(|i| sample(NOW - (24 - i) * H, &[("a", 0.2)]))
            .collect();
        h.extend((1..=6).map(|i| sample(NOW - (6 - i) * H, &[("a", 0.2 + 0.06 * i as f64)])));
        let v = build(&s, &h, NOW);
        let (d24, d6) = (v.paces[0].demand.unwrap(), v.paces[1].demand.unwrap());
        assert!((d6 - 6.0).abs() < 1e-6, "{d6}");
        assert!((d24 - 36.0 / 24.0).abs() < 1e-6, "{d24}");
    }

    #[test]
    fn short_history_or_none_is_unknown_never_guessed() {
        let s = seats(vec![weekly_seat("a", 0.5, 84)]);
        let h = [
            sample(NOW - 3 * H, &[("a", 0.4)]),
            sample(NOW, &[("a", 0.5)]),
        ];
        for hist in [&[][..], &h[..]] {
            let v = build(&s, hist, NOW);
            assert_eq!(v.paces[0].demand, None);
            assert_eq!(v.paces[0].runway, Runway::Unknown);
            assert_eq!(v.paces[0].recommend, None);
            assert!(v.render(false).contains("pace n/a"));
        }
    }

    #[test]
    fn a_reset_is_not_negative_burn_and_a_gap_is_not_paired() {
        let h = vec![
            sample(NOW - 10 * H, &[("a", 0.9)]),
            sample(NOW - 9 * H, &[("a", 0.0)]),
            sample(NOW - 8 * H, &[("a", 0.01)]),
            sample(NOW - H, &[("a", 0.5)]),
            sample(NOW, &[("a", 0.51)]),
        ];
        let (span, h24, h6) = burn(&h, NOW);
        assert!((span - 10.0).abs() < 1e-9);
        assert!(
            (h24 - 2.0).abs() < 1e-6,
            "1% + 1%; the fall and the 7h gap add nothing: {h24}"
        );
        assert!((h6 - 1.0).abs() < 1e-6);
    }

    #[test]
    fn runway_is_simulated_through_resets() {
        // Two seats at 90%, 8h from their reset: 20% left, but both refill in 8h.
        let s = seats(vec![weekly_seat("a", 0.9, 160), weekly_seat("b", 0.9, 160)]);
        let st: Vec<Status> = s.iter().map(|x| classify(x, NOW)).collect();
        let pairs: Vec<(&Seat, &Status)> = s.iter().zip(st.iter()).collect();
        assert_eq!(
            simulate(&pairs, 1.0, NOW),
            Runway::Sustainable,
            "resets rescue it"
        );
        match simulate(&pairs, 5.0, NOW) {
            Runway::DryAt(t) => assert!(((t - NOW) as f64 / 3600.0 - 4.0).abs() <= STEP_H + 1e-9),
            r => panic!("{r:?}"),
        }
        assert!(matches!(simulate(&pairs, 50.0, NOW), Runway::DryAt(_)));
        // One seat, a day of room: 0.5%/h holds to its reset, 1%/h is dry after ~184h.
        let s = seats(vec![weekly_seat("a", 0.1, 84)]);
        let st = [classify(&s[0], NOW)];
        let pairs: Vec<(&Seat, &Status)> = s.iter().zip(st.iter()).collect();
        assert_eq!(simulate(&pairs, 0.5, NOW), Runway::Sustainable);
        assert!(
            matches!(simulate(&pairs, 1.0, NOW), Runway::DryAt(t) if ((t - NOW) as f64 / 3600.0 - 184.0).abs() < 0.5)
        );
    }

    #[test]
    fn held_and_login_seats_are_not_supply() {
        let s = seats(vec![
            weekly_seat("a", 0.5, 84),
            seat(
                "b",
                "reauth_required",
                vec![win("claude_7d", 0.1, 10 * H, 60)],
            ),
        ]);
        let v = build(&s, &[], NOW);
        assert_eq!(v.supply, 1);
        assert_eq!(v.next_weekly_reset.unwrap().1, "a");
    }

    #[test]
    fn red_lines_name_every_seat_that_needs_a_person() {
        let s = seats(vec![
            weekly_seat("fine", 0.3, 50),
            seat("login", "reauth_required", vec![]),
            seat("hold", "subscription_required", vec![]),
            seat("bad", "quarantined", vec![]),
            seat("off", "disabled", vec![]),
            seat("old", "active", vec![win("claude_7d", 0.3, 50 * H, 4 * H)]),
        ]);
        let v = build(&s, &[], NOW);
        let red = v.red.join("\n");
        assert!(red.contains("seat login") && red.contains("login needed"));
        assert!(red.contains("seat hold") && red.contains("on hold"));
        assert!(red.contains("seat bad") && red.contains("alarm"));
        assert!(red.contains("seat old") && red.contains("no fresh reading for 4h00m"));
        assert!(!red.contains("seat off") && !red.contains("seat fine"));
        let text = v.render(true);
        assert!(text.contains("\x1b[1;31mRED  seat login"));
        assert!(!v.render(false).contains('\x1b'));
    }

    #[test]
    fn a_healthy_pool_has_no_red_and_an_empty_one_is_an_alarm() {
        let v = build(&seats(vec![weekly_seat("a", 0.3, 50)]), &[], NOW);
        assert!(v.red.is_empty());
        assert!(v.render(false).contains("Nothing needs a person."));
        let v = build(&[], &[], NOW);
        assert!(v.red.iter().any(|l| l.contains("no seats")));
        let v = build(
            &seats(vec![seat(
                "a",
                "active",
                vec![win("claude_7d", 1.0, 5 * H, 60)],
            )]),
            &[],
            NOW,
        );
        assert!(v.red.iter().any(|l| l.contains("no usable seat")));
    }

    #[test]
    fn json_view_names_missing_fields_and_nulls_them() {
        let j = build(&seats(vec![weekly_seat("a", 0.3, 50)]), &[], NOW).to_json();
        assert!(j["sessions"].is_null() && j["api_equivalent_value"].is_null());
        assert_eq!(
            j["missing_api_fields"].as_array().unwrap().len(),
            MISSING.len()
        );
        assert_eq!(j["seats"]["usable"], 1);
        assert_eq!(j["seat_list"][0]["weekly_percent"], 30.0);
        assert_eq!(j["pace"][0]["runway"]["kind"], "unknown");
    }

    #[test]
    fn record_keeps_a_week_and_one_reading_per_gap() {
        let s = seats(vec![weekly_seat("a", 0.3, 50)]);
        let mut h = vec![sample(NOW - 8 * 86_400, &[("a", 0.1)])];
        assert!(record(&mut h, &s, NOW));
        assert_eq!(h.len(), 1, "the old one is dropped, this one added");
        assert!(!record(&mut h, &s, NOW + 60), "inside the gap: nothing new");
        assert!(record(&mut h, &s, NOW + MIN_SAMPLE_GAP_S));
        assert_eq!(h.len(), 2);
        let back = history_from_json(&history_to_json(&h));
        assert_eq!(back, h);
    }
}
