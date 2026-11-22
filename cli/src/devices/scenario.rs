//! Pure scenario validation, display geometry and foreground evidence.
//! Execution stays on the ordinary public lease in the parent module.
use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Display {
    pub width: u32,
    pub height: u32,
    pub density: u32,
}

impl Display {
    pub fn parse(size: &str, density: u32) -> Result<Self, String> {
        let (width, height) = size
            .split_once('x')
            .ok_or("display size must be WIDTHxHEIGHT")?;
        let width = width.parse::<u32>().map_err(|_| "invalid display width")?;
        let height = height
            .parse::<u32>()
            .map_err(|_| "invalid display height")?;
        if width == 0 || height == 0 || density == 0 {
            return Err("display width, height and density must be positive".into());
        }
        Ok(Self {
            width,
            height,
            density,
        })
    }

    pub fn matches(self, (width, height): (u32, u32)) -> bool {
        (self.width == width && self.height == height)
            || (self.width == height && self.height == width)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Scenario {
    pub package: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "sourceCommit")]
    pub source_commit: String,
    pub sizes: BTreeMap<String, (String, u32)>,
    pub steps: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Step {
    Launch,
    Wait(f64),
    Tap([f64; 2]),
    Swipe([f64; 4], u64),
    Shot(String),
    Key(String),
    Text(String),
    Stop,
}

impl Scenario {
    pub fn parse(bytes: &[u8], size: &str) -> Result<(Self, Display, Vec<Step>), String> {
        let scenario: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid scenario JSON: {error}"))?;
        if scenario.package.split('.').count() < 2
            || scenario.package.split('.').any(|part| {
                part.is_empty() || !part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            })
        {
            return Err("scenario package must be an Android application id".into());
        }
        let (dimensions, density) = scenario.sizes.get(size).ok_or_else(|| {
            format!("scenario has no size `{size}`; choose one declared size per fresh lease")
        })?;
        let display = Display::parse(dimensions, *density)?;
        if scenario.steps.is_empty() {
            return Err("scenario requires at least one step".into());
        }
        if !scenario.steps.iter().any(|step| step.get("shot").is_some()) {
            return Err("capture scenario requires at least one named shot".into());
        }
        let mut labels = BTreeSet::new();
        let steps = scenario
            .steps
            .iter()
            .map(|value| {
                let step = Step::parse(value)?;
                if let Step::Shot(label) = &step {
                    if !labels.insert(label.clone()) {
                        return Err("scenario screenshot labels must be unique".into());
                    }
                }
                Ok(step)
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok((scenario, display, steps))
    }
}

impl Step {
    fn parse(value: &Value) -> Result<Self, String> {
        let object = value.as_object().ok_or("scenario step must be an object")?;
        if object.len() != 1 {
            return Err("each scenario step must contain exactly one operation".into());
        }
        let (operation, value) = object.iter().next().expect("one operation");
        match operation.as_str() {
            "launch" if value.as_bool() == Some(true) => Ok(Self::Launch),
            "stop" if value.as_bool() == Some(true) => Ok(Self::Stop),
            "wait" => value
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(Self::Wait)
                .ok_or("wait must be finite nonnegative seconds".into()),
            "tap" => Ok(Self::Tap(fractions::<2>(value)?)),
            "swipe" => {
                let values = value
                    .as_array()
                    .ok_or("swipe requires four fractions and optional milliseconds")?;
                if !(4..=5).contains(&values.len()) {
                    return Err("swipe requires four fractions and optional milliseconds".into());
                }
                let duration = values
                    .get(4)
                    .map(|v| {
                        v.as_u64()
                            .filter(|n| *n > 0)
                            .ok_or("swipe duration must be positive milliseconds")
                    })
                    .transpose()?
                    .unwrap_or(300);
                Ok(Self::Swipe(
                    fractions::<4>(&Value::Array(values[..4].to_vec()))?,
                    duration,
                ))
            }
            "shot" => {
                let label = value.as_str().ok_or("shot requires a filename label")?;
                if label == "screenshot-final" {
                    return Err("screenshot-final is reserved for the diagnostic frame".into());
                }
                if label.is_empty()
                    || !label
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
                {
                    return Err(
                        "shot label accepts only letters, digits, hyphen and underscore".into(),
                    );
                }
                Ok(Self::Shot(label.into()))
            }
            "key" => {
                let key = value
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| value.as_u64().map(|n| n.to_string()))
                    .ok_or("key requires an Android keycode name or number")?;
                if key.is_empty() || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
                    return Err("invalid Android keycode".into());
                }
                Ok(Self::Key(key))
            }
            "text" => {
                let text = value.as_str().ok_or("text requires a string")?;
                if text.chars().any(char::is_control) {
                    return Err("text must not contain control characters".into());
                }
                Ok(Self::Text(text.into()))
            }
            _ => Err(format!("unsupported scenario operation `{operation}`")),
        }
    }

    pub fn input(&self) -> bool {
        matches!(
            self,
            Self::Tap(_) | Self::Swipe(..) | Self::Key(_) | Self::Text(_)
        )
    }
}

fn fractions<const N: usize>(value: &Value) -> Result<[f64; N], String> {
    let values = value
        .as_array()
        .filter(|v| v.len() == N)
        .ok_or_else(|| format!("expected {N} coordinate fractions"))?;
    let mut result = [0.0; N];
    for (index, value) in values.iter().enumerate() {
        result[index] = value
            .as_f64()
            .filter(|n| n.is_finite() && (0.0..=1.0).contains(n))
            .ok_or("coordinates must be finite fractions between zero and one")?;
    }
    Ok(result)
}

pub(super) fn point(fractions: [f64; 2], (width, height): (u32, u32)) -> [String; 2] {
    [
        (fractions[0] * f64::from(width.saturating_sub(1)))
            .round()
            .to_string(),
        (fractions[1] * f64::from(height.saturating_sub(1)))
            .round()
            .to_string(),
    ]
}

/// Android omits the Override line when the requested value equals the
/// physical default. Prefer the override when present, otherwise physical.
pub(super) fn wm_value(output: &str, field: &str) -> Option<String> {
    let value = |kind| {
        output.lines().find_map(|line| {
            line.trim()
                .strip_prefix(&format!("{kind} {field}: "))
                .map(str::trim)
                .map(str::to_string)
        })
    };
    value("Override").or_else(|| value("Physical"))
}

pub(super) fn png_size(bytes: &[u8]) -> Result<(u32, u32), String> {
    if bytes.len() < 33
        || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
        || bytes[8..12] != 13u32.to_be_bytes()
        || &bytes[12..16] != b"IHDR"
    {
        return Err("screenshot is not a PNG with a complete IHDR".into());
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("four bytes"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("four bytes"));
    if width == 0 || height == 0 {
        return Err("screenshot has zero dimensions".into());
    }
    Ok((width, height))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusClass {
    SelfOwner,
    Other,
    Missing,
    Ambiguous,
}

fn push_owners(dst: &mut Vec<String>, srcs: &[&str]) {
    for &v in srcs {
        if dst.len() < 4 && !dst.iter().any(|e| e == v) {
            dst.push(v.into());
        }
    }
}

fn bounded(v: &str) -> String {
    if v.chars().count() <= 96 {
        v.into()
    } else {
        format!("{}...", v.chars().take(93).collect::<String>())
    }
}

fn join_or_none(v: &[String]) -> String {
    if v.is_empty() {
        "none".into()
    } else {
        bounded(&v.join(","))
    }
}

fn is_ident(s: &str, dollar: bool) -> bool {
    !s.is_empty()
        && s.split('.').all(|p| {
            let mut b = p.bytes();
            b.next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_' || (dollar && c == b'$'))
                && b.all(|c| c.is_ascii_alphanumeric() || c == b'_' || (dollar && c == b'$'))
        })
}

fn is_pkg(s: &str) -> bool {
    s == "android" || (s.contains('.') && is_ident(s, false))
}

fn extract_components(line: &str) -> Vec<(&str, &str)> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || "_./$".contains(c)))
        .filter_map(|w| {
            let (p, c) = w.split_once('/')?;
            (!c.contains('/') && is_pkg(p) && is_ident(c.strip_prefix('.').unwrap_or(c), true))
                .then_some((p, w))
        })
        .collect()
}

fn component_packages(line: &str) -> Vec<&str> {
    let mut pkgs = Vec::new();
    for (p, _) in extract_components(line) {
        if !pkgs.contains(&p) {
            pkgs.push(p);
        }
    }
    pkgs
}

fn parse_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.trim_start().strip_prefix(key)?.trim_start();
    rest.strip_prefix('=')
        .or_else(|| rest.strip_prefix(':'))
        .map(str::trim)
}

fn window_id_and_title(value: &str) -> Option<(&str, &str)> {
    let (_, tail) = value.split_once("Window{")?;
    let inner = tail[..tail.rfind('}').unwrap_or(tail.len())].trim();
    let id = inner
        .split(|c: char| c.is_whitespace() || c == '}')
        .next()
        .filter(|s| !s.is_empty())?;
    let mut rest = inner[id.len()..].trim_start();
    if let Some(t) = rest.split_whitespace().next().filter(|t| {
        t.strip_prefix('u')
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    }) {
        rest = rest[t.len()..].trim_start();
    }
    Some((id, rest))
}

fn window_block_owners<'a>(windows: &'a str, identity: &str) -> Vec<(bool, Vec<&'a str>)> {
    let needle = format!("Window{{{identity}");
    let matches_id = |mut s: &str| loop {
        let Some(pos) = s.find(&needle) else {
            return false;
        };
        if s[pos + needle.len()..].starts_with(|c: char| c.is_whitespace() || c == '}') {
            return true;
        }
        s = &s[pos + 1..];
    };
    let (mut records, mut cur): (_, Option<(bool, Vec<&'a str>)>) = (Vec::new(), None);
    for line in windows.lines() {
        let t = line.trim_start();
        if t.starts_with("Window #") {
            records.extend(cur.take());
            if matches_id(t) {
                cur = Some((false, Vec::new()));
            }
        } else if !line.starts_with(char::is_whitespace) && line.starts_with("WINDOW MANAGER ") {
            records.extend(cur.take());
        } else if let Some((saw, pkgs)) = cur.as_mut().filter(|_| t.starts_with("mOwnerUid=")) {
            *saw = true;
            for p in t
                .split_whitespace()
                .filter_map(|p| p.strip_prefix("package="))
                .map(|p| p.trim_end_matches(|c: char| ",;".contains(c)))
                .filter(|p| !p.is_empty() && *p != "null" && is_pkg(p))
            {
                if !pkgs.contains(&p) {
                    pkgs.push(p);
                }
            }
        }
    }
    records.extend(cur);
    records
}

fn sanitize_window(value: &str, components: &[(&str, &str)]) -> String {
    if value.is_empty() || value == "null" || value.starts_with("null ") {
        return "null".into();
    }
    let title = window_id_and_title(value)
        .map(|(_, t)| t.trim().trim_end_matches('}').trim())
        .unwrap_or("");
    if !components.is_empty() {
        let mut names = Vec::new();
        for (_, c) in components {
            if names.len() < 2 && !names.contains(c) {
                names.push(*c);
            }
        }
        let j = names.join(",");
        return bounded(&if title.starts_with("SurfaceView") {
            format!("SurfaceView[{j}]")
        } else {
            j
        });
    }
    for (pre, lbl) in [
        ("PopupWindow", "PopupWindow"),
        ("Application Not Responding", "ApplicationNotResponding"),
        ("Application Error", "ApplicationError"),
    ] {
        if title.starts_with(pre) {
            return lbl.into();
        }
    }
    if title.starts_with("SurfaceView") {
        return title
            .split(|c: char| !(c.is_ascii_alphanumeric() || "_.".contains(c)))
            .find(|t| is_pkg(t))
            .map_or_else(
                || "SurfaceView".into(),
                |p| bounded(&format!("SurfaceView[{p}]")),
            );
    }
    if let Some(rest) = title.strip_prefix("Splash Screen") {
        let tail = rest.trim();
        return if is_pkg(tail) {
            bounded(&format!("SplashScreen({tail})"))
        } else {
            "SplashScreen".into()
        };
    }
    const SYS: &str = "StatusBar NavigationBar NavigationBar0 InputMethod NotificationShade Keyguard BiometricPrompt AssistPreviewPanel DockedStackDivider PipMenuActivity";
    if SYS.split_whitespace().any(|w| w == title) || (!title.is_empty() && is_pkg(title)) {
        bounded(title)
    } else if title.is_empty() {
        "untitled".into()
    } else {
        "<redacted>".into()
    }
}

fn classify_pkgs(package: &str, pkgs: &[&str], allow_self: bool) -> FocusClass {
    match pkgs {
        [] => FocusClass::Missing,
        [p] if *p == package => {
            if allow_self {
                FocusClass::SelfOwner
            } else {
                FocusClass::Ambiguous
            }
        }
        [_] => FocusClass::Other,
        _ => FocusClass::Ambiguous,
    }
}

fn assess_one(
    package: &str,
    windows: &str,
    display: Option<u32>,
    value: &str,
) -> (Option<u32>, bool, FocusClass, Vec<String>, String) {
    let components = extract_components(value);
    let comp_pkgs = component_packages(value);
    let window = sanitize_window(value, &components);
    let is_null = value.is_empty() || value == "null" || value.starts_with("null ");
    let mut owners = Vec::new();
    let records = if is_null {
        Vec::new()
    } else {
        window_id_and_title(value)
            .map(|(id, _)| window_block_owners(windows, id))
            .unwrap_or_default()
    };
    let class = match records.as_slice() {
        _ if is_null => FocusClass::Missing,
        [_, _, ..] => {
            records
                .iter()
                .for_each(|(_, p)| push_owners(&mut owners, p));
            push_owners(&mut owners, &comp_pkgs);
            FocusClass::Ambiguous
        }
        [(true, pkgs)] => {
            push_owners(&mut owners, pkgs);
            match pkgs.as_slice() {
                [] => {
                    push_owners(&mut owners, &comp_pkgs);
                    classify_pkgs(package, &comp_pkgs, false)
                }
                [p] if *p == package && (comp_pkgs.is_empty() || comp_pkgs == [package]) => {
                    FocusClass::SelfOwner
                }
                [p] if *p == package => {
                    push_owners(&mut owners, &comp_pkgs);
                    FocusClass::Ambiguous
                }
                [_] if comp_pkgs.contains(&package) => {
                    push_owners(&mut owners, &[package]);
                    FocusClass::Ambiguous
                }
                [_] => FocusClass::Other,
                _ => FocusClass::Ambiguous,
            }
        }
        _ => {
            push_owners(&mut owners, &comp_pkgs);
            classify_pkgs(package, &comp_pkgs, true)
        }
    };
    (display, is_null, class, owners, window)
}

fn assess_focus_summary(package: &str, windows: &str) -> (FocusClass, Vec<String>, String) {
    let (mut apps, mut entries, mut display) = (Vec::new(), Vec::new(), None);
    for line in windows.lines() {
        let t = line.trim_start();
        if !line.starts_with(char::is_whitespace) && line.starts_with("WINDOW MANAGER ") {
            display = None;
        } else if t.starts_with("Display:") {
            display = t
                .split_whitespace()
                .find_map(|p| p.strip_prefix("mDisplayId="))
                .and_then(|s| {
                    s.trim_end_matches(|c: char| !c.is_ascii_digit())
                        .parse()
                        .ok()
                });
        } else if let Some(rest) = t.strip_prefix("Display #") {
            display = rest
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|s| s.parse().ok());
        }
        if let Some(v) = parse_field(line, "mFocusedApp").filter(|v| !v.is_empty() && *v != "null")
        {
            push_owners(&mut apps, &component_packages(v));
        }
        if let Some(v) = parse_field(line, "mCurrentFocus") {
            entries.push(assess_one(package, windows, display, v));
        }
    }
    let primary_ok = entries.iter().any(|e| e.0 == Some(0) && !e.1);
    let active: Vec<_> = if primary_ok {
        entries
            .into_iter()
            .filter(|e| !(e.1 && e.0.is_some_and(|id| id != 0)))
            .collect()
    } else {
        entries
    };
    let (class, owners, window) = match active.as_slice() {
        [] => (FocusClass::Missing, Vec::new(), "absent".into()),
        [(_, _, c, o, w)] => (*c, o.clone(), w.clone()),
        many => {
            let (mut owners, mut wins) = (Vec::new(), Vec::new());
            for (_, _, _, o, w) in many {
                o.iter()
                    .for_each(|p| push_owners(&mut owners, &[p.as_str()]));
                if wins.len() < 2 && !wins.contains(w) {
                    wins.push(w.clone());
                }
            }
            let class = if many.iter().all(|e| e.2 == FocusClass::SelfOwner) {
                FocusClass::SelfOwner
            } else if many.iter().all(|e| e.2 == FocusClass::Missing) {
                FocusClass::Missing
            } else if many.iter().all(|e| e.2 == FocusClass::Other) && owners.len() == 1 {
                FocusClass::Other
            } else {
                FocusClass::Ambiguous
            };
            (class, owners, bounded(&wins.join(",")))
        }
    };
    let label = match class {
        FocusClass::SelfOwner => "self",
        FocusClass::Other => "other",
        FocusClass::Missing => "missing",
        FocusClass::Ambiguous => "ambiguous",
    };
    (
        class,
        owners.clone(),
        format!(
            "classification={label}, owner={}, window={window}, focused_app={}",
            join_or_none(&owners),
            join_or_none(&apps)
        ),
    )
}

/// Require both the resumed activity and focused window, never a substring
/// match against an unrelated window. PopupWindow and slash-less titles
/// resolve via the matching `Window #` ownership record.
pub(super) fn foreground(package: &str, activities: &str, windows: &str) -> Result<(), String> {
    let (class, owners, summary) = assess_focus_summary(package, windows);
    let resumed: Vec<_> = activities
        .lines()
        .filter(|line| {
            ["mResumedActivity", "topResumedActivity", "ResumedActivity"]
                .iter()
                .any(|m| line.contains(m))
        })
        .map(component_packages)
        .collect();
    if resumed.is_empty() || resumed.iter().any(|found| found != &[package]) {
        return Err(format!(
            "resumed activity does not uniquely belong to the scenario package ({summary})"
        ));
    }
    if class != FocusClass::SelfOwner || owners != [package] {
        return Err(format!(
            "focused window owner does not uniquely belong to the scenario package ({summary})"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parser_selects_one_size_and_rejects_ambiguous_or_unsafe_steps() {
        let scenario = json!({"package":"com.example.game", "sizes":{"phone":["1080x2400",420],"tablet":["2560x1600",240]},
            "steps":[{"launch":true},{"wait":2},{"shot":"first"},{"tap":[0.5,1.0]}]});
        let parse =
            |value: &Value, size| Scenario::parse(&serde_json::to_vec(value).unwrap(), size);
        let (_, display, steps) = parse(&scenario, "tablet").unwrap();
        assert_eq!(display.width, 2560);
        assert!(steps[3].input());
        assert!(parse(&scenario, "missing").is_err());
        for step in [
            json!({"shot":"../escape"}),
            json!({"shot":"screenshot-final"}),
            json!({"tap":[-0.1,0.5]}),
            json!({"wait":-1}),
            json!({"launch":true,"tap":[0.5,0.5]}),
            json!({"exec":"sh"}),
            json!({"key":"BACK;exit"}),
        ] {
            let mut invalid = scenario.clone();
            invalid["steps"] = json!([step, {"shot":"valid-control"}]);
            assert!(parse(&invalid, "phone").is_err());
        }
        let mut duplicate = scenario;
        duplicate["steps"] = json!([{"shot":"same"},{"shot":"same"}]);
        assert!(parse(&duplicate, "phone").is_err());
    }

    #[test]
    fn geometry_uses_actual_native_orientation_and_never_outside_the_frame() {
        let display = Display::parse("1080x2400", 420).unwrap();
        assert!(display.matches((2400, 1080)));
        assert!(!display.matches((1280, 800)));
        assert_eq!(point([1.0, 1.0], (2400, 1080)), ["2399", "1079"]);
        assert_eq!(point([0.0, 0.0], (2560, 1600)), ["0", "0"]);
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(2560u32.to_be_bytes());
        png.extend(1600u32.to_be_bytes());
        png.extend([0; 9]);
        assert_eq!(png_size(&png).unwrap(), (2560, 1600));
        assert!(png_size(b"not a screenshot").is_err());
        assert_eq!(
            wm_value("Physical size: 1080x2400\n", "size").as_deref(),
            Some("1080x2400")
        );
        assert_eq!(
            wm_value(
                "Physical size: 1080x2400\nOverride size: 2560x1600\n",
                "size"
            )
            .as_deref(),
            Some("2560x1600")
        );
        assert_eq!(
            wm_value("Physical density: 420\n", "density").as_deref(),
            Some("420")
        );
        assert_eq!(wm_value("", "size"), None);
    }

    #[test]
    fn foreground_requires_both_unique_current_owners() {
        let activity = "mResumedActivity: ActivityRecord{abc u0 com.example.game/.Main t1}";
        let focus = "mCurrentFocus=Window{abcd u0 com.example.game/.Main}";
        assert!(foreground("com.example.game", activity, focus).is_ok());
        let null_err = foreground("com.example.game", activity, "mCurrentFocus=null").unwrap_err();
        assert!(null_err.contains("classification=missing"), "{null_err}");
        let launcher_err = foreground(
            "com.example.game",
            activity,
            "mCurrentFocus=Window{a u0 com.android.launcher/.Home}",
        )
        .unwrap_err();
        assert!(
            launcher_err.contains("classification=other"),
            "{launcher_err}"
        );
        assert!(
            launcher_err.contains("owner=com.android.launcher"),
            "{launcher_err}"
        );
        let resumed_err = foreground("com.example.game", "", focus).unwrap_err();
        assert!(resumed_err.contains("classification=self"), "{resumed_err}");
        let popup = "mCurrentFocus=Window{a u0 PopupWindow:dead}\nWindow #0 Window{a u0 PopupWindow:dead}\n mOwnerUid=123 package=com.example.game\nWindow #1 Window{b u0 other}\n mOwnerUid=456 package=com.other.app";
        assert!(foreground("com.example.game", activity, popup).is_ok());
        assert!(foreground("com.other.app", activity, popup).is_err());
        let popup_other = foreground(
            "com.example.game",
            activity,
            &popup.replace("package=com.example.game", "package=com.other.app"),
        )
        .unwrap_err();
        assert!(
            popup_other.contains("classification=other"),
            "{popup_other}"
        );
        assert!(popup_other.contains("owner=com.other.app"), "{popup_other}");
        assert!(popup_other.contains("window=PopupWindow"), "{popup_other}");
    }

    #[test]
    fn foreground_accepts_valid_native_engine_surfaceview_slashless_and_multidisplay() {
        let activity = "topResumedActivity=ActivityRecord{abc u0 com.example.app/.Main t1}";
        for valid in [
            "mCurrentFocus: Window{a1b2c3 u0 com.example.app/android.app.NativeActivity}",
            "mCurrentFocus=Window{a1b2c3 u0 SurfaceView[com.example.app/com.example.app.NativeMain](BLAST)#0}",
            "mCurrentFocus=Window{a1b2c3 u0 com.example.app/.Main (v2.0/1)}",
            "mCurrentFocus=Window{a1b2c3 u0 com.example.app}\nWINDOW MANAGER WINDOWS\n  Window #0 Window{a1b2c3 u0 com.example.app}:\n    mOwnerUid=10185 showForAllUsers=false package=com.example.app appop=NONE",
            "mCurrentFocus=Window{a1b2c3}\n  Window #0 Window{a1b2c3}:\n    mOwnerUid=10185 package=com.example.app",
            "mCurrentFocus=Window{a1b2c3 u0 Splash Screen com.example.app}\n  Window #0 Window{a1b2c3 u0 Splash Screen com.example.app}:\n    mOwnerUid=10185 package=com.example.app",
            "mCurrentFocus=Window{a1b2c3 u0 SurfaceView[com.example.app](BLAST)#0}\n  Window #0 Window{a1b2c3 u0 SurfaceView[com.example.app](BLAST)#0}:\n    mOwnerUid=10185 package=com.example.app",
            "Display: mDisplayId=0 (organized)\n  mCurrentFocus=Window{a1 u0 com.example.app/.Main}\nDisplay: mDisplayId=1 (organized)\n  mCurrentFocus=null",
            "Display: mDisplayId=0 (organized)\n  mCurrentFocus=Window{a1 u0 com.example.app/.Main}\nDisplay: mDisplayId=1 (organized)\n  mCurrentFocus=Window{a2 u0 com.example.app/.Overlay}",
        ] {
            assert!(foreground("com.example.app", activity, valid).is_ok(), "{valid}");
        }
    }

    #[test]
    fn foreground_classifies_other_missing_ambiguous_and_redacts_titles() {
        let activity = "mResumedActivity: ActivityRecord{abc u0 com.example.app/.Main t1}";
        let system_overlay = concat!(
            "  mCurrentFocus=Window{999 u0 com.android.systemui/com.android.systemui.usb.UsbPermissionActivity}\n",
            "  mFocusedApp=ActivityRecord{abc u0 com.example.app/.Main t1}\n",
            "  Window #0 Window{999 u0 com.android.systemui/com.android.systemui.usb.UsbPermissionActivity}:\n",
            "    mOwnerUid=10100 showForAllUsers=true package=com.android.systemui appop=NONE",
        );
        let err = foreground("com.example.app", activity, system_overlay).unwrap_err();
        assert!(err.contains("classification=other"), "{err}");
        assert!(err.contains("owner=com.android.systemui"), "{err}");
        assert!(err.contains("focused_app=com.example.app"), "{err}");

        let chooser =
            "mCurrentFocus=Window{777 u0 android/com.android.internal.app.ChooserActivity}";
        let err = foreground("com.example.app", activity, chooser).unwrap_err();
        assert!(err.contains("classification=other"), "{err}");
        assert!(err.contains("owner=android"), "{err}");

        let pii_title = concat!(
            "  mCurrentFocus=Window{444 u0 alice@example.com secret_token=xyz123}\n",
            "  Window #0 Window{444 u0 alice@example.com secret_token=xyz123}:\n",
            "    mOwnerUid=10200 package=com.other.app",
        );
        let err = foreground("com.example.app", activity, pii_title).unwrap_err();
        assert!(err.contains("classification=other"), "{err}");
        assert!(err.contains("owner=com.other.app"), "{err}");
        assert!(err.contains("window=<redacted>"), "{err}");
        assert!(
            !err.contains("alice@example.com") && !err.contains("secret_token"),
            "{err}"
        );

        for missing in [
            "",
            "mCurrentFocus=null\nmFocusedApp=ActivityRecord{abc u0 com.example.app/.Main t1}",
            "mCurrentFocus=Window{123 u0 PopupWindow:deadbeef}",
            "mCurrentFocus=Window{123 u0 com.example.app}",
            "mCurrentFocus=Window{123 u0 PopupWindow:dead}\nWindow #0 Window{123 u0 PopupWindow:dead}\n mOwnerUid=1000 package=null",
        ] {
            let err = foreground("com.example.app", activity, missing).unwrap_err();
            assert!(err.contains("classification=missing"), "{missing} => {err}");
        }

        for ambiguous in [
            "mCurrentFocus=Window{123 u0 com.example.app/.Main com.other.app/.Overlay}",
            "Display: mDisplayId=0 (organized)\n  mCurrentFocus=Window{1 u0 com.example.app/.Main}\nDisplay: mDisplayId=1 (organized)\n  mCurrentFocus=Window{2 u0 com.android.systemui/.Dialog}",
            "Display: mDisplayId=0 (organized)\n  mCurrentFocus=null\nDisplay: mDisplayId=1 (organized)\n  mCurrentFocus=Window{2 u0 com.example.app/.Main}",
            "mCurrentFocus=Window{123 u0 com.example.app/.Main}\nWindow #0 Window{123 u0 com.example.app/.Main}:\n  mOwnerUid=10200 package=com.other.app",
            "mCurrentFocus=Window{123 u0 PopupWindow:a}\nWindow #0 Window{123 u0 PopupWindow:a}:\n  mOwnerUid=1 package=com.example.app\nWindow #1 Window{123 u0 PopupWindow:a}:\n  mOwnerUid=2 package=com.other.app",
        ] {
            let err = foreground("com.example.app", activity, ambiguous).unwrap_err();
            assert!(err.contains("classification=ambiguous"), "{ambiguous} => {err}");
        }
    }
}
