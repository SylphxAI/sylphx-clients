//! `--output table|json|yaml|name` (spec §8.5): tables show name, Ready, and
//! age; `name` prints names only, for scripts.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Table,
    Json,
    Yaml,
    Name,
}

impl Format {
    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "table" => Self::Table,
            "json" => Self::Json,
            "yaml" => Self::Yaml,
            "name" => Self::Name,
            other => {
                return Err(format!(
                    "unknown output format `{other}` (table, json, yaml, name)"
                ))
            }
        })
    }
}

/// The items of a List response: its one array field.
pub fn list_items(v: &Value) -> Option<&Vec<Value>> {
    let obj = v.as_object()?;
    obj.iter()
        .filter(|(k, _)| k.as_str() != "next_page_token")
        .find_map(|(_, v)| v.as_array())
}

pub fn render(v: &Value, format: Format) -> String {
    match format {
        Format::Json => serde_json::to_string_pretty(v).unwrap_or_default(),
        Format::Yaml => serde_yaml_ng::to_string(v)
            .unwrap_or_default()
            .trim_end()
            .to_string(),
        Format::Name => match list_items(v) {
            Some(items) => items
                .iter()
                .filter_map(|i| i.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            None => v
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        Format::Table => match list_items(v) {
            Some(items) => table(items),
            None => serde_yaml_ng::to_string(v)
                .unwrap_or_default()
                .trim_end()
                .to_string(),
        },
    }
}

fn ready(item: &Value) -> String {
    item.get("status")
        .and_then(|s| s.get("conditions"))
        .and_then(Value::as_array)
        .and_then(|cs| {
            cs.iter()
                .find(|c| c.get("type").and_then(Value::as_str) == Some("Ready"))
        })
        .and_then(|c| c.get("status").and_then(Value::as_str))
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "-".into())
}

fn age(item: &Value) -> String {
    let Some(t) = item
        .get("meta")
        .and_then(|m| m.get("create_time"))
        .and_then(Value::as_str)
    else {
        return "-".into();
    };
    match seconds_since(t) {
        Some(s) if s < 120 => format!("{s}s"),
        Some(s) if s < 7200 => format!("{}m", s / 60),
        Some(s) if s < 172_800 => format!("{}h", s / 3600),
        Some(s) => format!("{}d", s / 86_400),
        None => "-".into(),
    }
}

/// Seconds from an RFC 3339 UTC timestamp (`2026-09-22T18:00:00Z`) to now.
fn seconds_since(t: &str) -> Option<u64> {
    let (date, time) = t.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut tt = time.split(':');
    let (h, mi) = (
        tt.next()?.parse::<i64>().ok()?,
        tt.next()?.parse::<i64>().ok()?,
    );
    let s = tt.next()?.split('.').next()?.parse::<i64>().ok()?;
    // Days from civil (Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let then = days * 86_400 + h * 3600 + mi * 60 + s;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    u64::try_from(now - then).ok()
}

fn table(items: &[Value]) -> String {
    let rows: Vec<[String; 3]> = items
        .iter()
        .map(|i| {
            [
                i.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("-")
                    .to_string(),
                ready(i),
                age(i),
            ]
        })
        .collect();
    let w = rows.iter().map(|r| r[0].len()).max().unwrap_or(0).max(4);
    let mut out = format!("{:<w$}  {:<5}  AGE", "NAME", "READY");
    for r in rows {
        out.push_str(&format!("\n{:<w$}  {:<5}  {}", r[0], r[1], r[2]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_list_renders_as_a_table_and_as_names() {
        let v = json!({"databases": [
            {"name": "orgs/o/projects/p/envs/e/databases/main", "status": {"conditions": [{"type": "Ready", "status": "TRUE"}]}},
            {"name": "orgs/o/projects/p/envs/e/databases/logs"}
        ], "next_page_token": ""});
        let t = render(&v, Format::Table);
        assert!(t.starts_with("NAME"));
        assert!(t.contains("databases/main  true"));
        assert_eq!(
            render(&v, Format::Name),
            "orgs/o/projects/p/envs/e/databases/main\norgs/o/projects/p/envs/e/databases/logs"
        );
    }

    #[test]
    fn age_counts_from_create_time() {
        assert!(seconds_since("2000-01-01T00:00:00Z").unwrap() > 800_000_000);
        assert_eq!(seconds_since("nonsense"), None);
    }
}
