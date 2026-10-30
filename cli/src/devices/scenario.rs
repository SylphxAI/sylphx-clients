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

fn component_packages(line: &str) -> Vec<&str> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || "_./$".contains(c)))
        .filter_map(|word| word.split_once('/').map(|(package, _)| package))
        .filter(|package| package.contains('.'))
        .collect()
}

/// Require both the resumed activity and focused window, never a substring
/// match against an unrelated window. PopupWindow titles resolve via owner.
pub(super) fn foreground(package: &str, activities: &str, windows: &str) -> Result<(), String> {
    let resumed: Vec<_> = activities
        .lines()
        .filter(|line| {
            ["mResumedActivity", "topResumedActivity", "ResumedActivity"]
                .iter()
                .any(|marker| line.contains(marker))
        })
        .map(component_packages)
        .collect();
    if resumed.is_empty() || resumed.iter().any(|found| found != &[package]) {
        return Err("resumed activity does not uniquely belong to the scenario package".into());
    }
    let focused: Vec<_> = windows
        .lines()
        .filter(|line| line.contains("mCurrentFocus"))
        .collect();
    if focused.len() != 1 {
        return Err("focused window is absent or ambiguous".into());
    }
    let focus = focused[0];
    if component_packages(focus) == [package] {
        return Ok(());
    }
    let identity = focus
        .split_once("Window{")
        .and_then(|(_, tail)| tail.split_whitespace().next())
        .ok_or("cannot resolve focused window identity")?;
    let headers: Vec<_> = windows
        .match_indices("Window #")
        .map(|(offset, _)| offset)
        .collect();
    let owners: Vec<_> = headers
        .iter()
        .enumerate()
        .filter_map(|(index, start)| {
            let block = &windows[*start..headers.get(index + 1).copied().unwrap_or(windows.len())];
            let header = block.lines().next().unwrap_or_default();
            if !header.contains(&format!("Window{{{identity} ")) {
                return None;
            }
            Some(
                block
                    .lines()
                    .filter(|line| line.trim_start().starts_with("mOwnerUid="))
                    .flat_map(|line| line.split_whitespace())
                    .filter_map(|part| part.strip_prefix("package="))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    if owners != [vec![package]] {
        return Err("focused window owner does not uniquely belong to the scenario package".into());
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
        assert!(foreground("com.example.game", activity, "mCurrentFocus=null").is_err());
        assert!(foreground(
            "com.example.game",
            activity,
            "mCurrentFocus=Window{a u0 com.android.launcher/.Home}"
        )
        .is_err());
        assert!(foreground("com.example.game", "", focus).is_err());
        let popup = "mCurrentFocus=Window{a u0 PopupWindow:dead}\nWindow #0 Window{a u0 PopupWindow:dead}\n mOwnerUid=123 package=com.example.game\nWindow #1 Window{b u0 other}\n mOwnerUid=456 package=com.other.app";
        assert!(foreground("com.example.game", activity, popup).is_ok());
        assert!(foreground("com.other.app", activity, popup).is_err());
        assert!(foreground(
            "com.example.game",
            activity,
            &popup.replace("package=com.example.game", "package=com.other.app")
        )
        .is_err());
    }
}
