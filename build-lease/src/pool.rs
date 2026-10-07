//! The warm Volume pool: node-local Sandboxes Volumes that keep a build's
//! state (a workspace's `target/`, or BuildKit's layers and cache mounts)
//! from one lease to the next, one pool per build scope.
//!
//! A pool is the Volumes that carry one `purpose` label and one scope label
//! (`build-repo` for `sylphx build run`, `build-scope` for the platform's
//! image builds), in one region. A free Volume of the pool is warm; a busy
//! one is skipped, never waited on for long; a full region gives up its least
//! recently used free Volume of the same purpose. Everything on a pool Volume
//! is a cache: losing one makes a build slower, never different.

use std::collections::HashSet;

use sylphx::sandboxes as sbx;

/// The label naming what a pool Volume is for.
pub const PURPOSE_LABEL: &str = "purpose";
/// The label naming a pool Volume's region; home-region Volumes have none.
pub const REGION_LABEL: &str = "build-region";
/// Fast node-local storage (warm build state on NVMe); it pins every lease
/// that mounts it to its node.
pub const VOLUME_CLASS: &str = "local";

/// A Volume's label `key`.
#[must_use]
pub fn label<'a>(v: &'a sbx::Volume, key: &str) -> Option<&'a str> {
    v.meta
        .as_ref()
        .and_then(|m| m.labels.get(key))
        .map(String::as_str)
}

#[must_use]
pub fn vstate(v: &sbx::Volume) -> sbx::VolumeState {
    v.status
        .as_ref()
        .and_then(|s| s.state.clone())
        .unwrap_or(sbx::VolumeState::Unknown(String::new()))
}

/// Whether a Volume is gone or going (it no longer counts toward a pool).
#[must_use]
pub fn is_dead(v: &sbx::Volume) -> bool {
    matches!(
        vstate(v),
        sbx::VolumeState::Failed | sbx::VolumeState::Deleting
    )
}

/// Whether `v` is one of the pool's Volumes: its `purpose`, its `scope_label`
/// equal to `scope`, in `region` (`None`, the home region, is the pool
/// without a region label).
#[must_use]
pub fn in_pool(
    v: &sbx::Volume,
    purpose: &str,
    scope_label: &str,
    scope: &str,
    region: Option<&str>,
) -> bool {
    label(v, PURPOSE_LABEL) == Some(purpose)
        && label(v, scope_label) == Some(scope)
        && label(v, REGION_LABEL) == region
}

/// The free warm Volumes a build may take, in a stable order, without those
/// whose node had no room for it (`full`). When the pool has no room for a
/// fresh Volume (`room` false) and every free one was full, `full` is
/// cleared: waiting on a warm node is then the only way to run.
pub fn free_warm<'a>(
    pool: &'a [sbx::Volume],
    full: &mut HashSet<String>,
    room: bool,
) -> Vec<&'a sbx::Volume> {
    let mut free: Vec<&sbx::Volume> = pool
        .iter()
        .filter(|v| vstate(v) == sbx::VolumeState::Available)
        .collect();
    free.sort_by(|a, b| a.name.cmp(&b.name));
    if !room && free.iter().all(|v| full.contains(&v.name)) {
        full.clear();
    }
    free.retain(|v| !full.contains(&v.name));
    free
}

/// The free Volume of `purpose` in `region`, of any scope, that was used
/// least recently (its last attach or detach is its `update_time`), among
/// those `also` accepts.
#[must_use]
pub fn lru_free<'a>(
    all: &'a [sbx::Volume],
    purpose: &str,
    region: Option<&str>,
    also: impl Fn(&sbx::Volume) -> bool,
) -> Option<&'a sbx::Volume> {
    all.iter()
        .filter(|v| {
            label(v, PURPOSE_LABEL) == Some(purpose)
                && label(v, REGION_LABEL) == region
                && vstate(v) == sbx::VolumeState::Available
                && also(v)
        })
        .min_by(|a, b| used(a).cmp(used(b)).then_with(|| a.name.cmp(&b.name)))
}

/// The free Volumes of `purpose` (any region) last used before `cutoff`, an
/// RFC 3339 UTC time in the API's own form (`2026-10-06T00:00:00.000Z`), so
/// the comparison is the times' own order.
#[must_use]
pub fn idle_before<'a>(
    all: &'a [sbx::Volume],
    purpose: &str,
    cutoff: &str,
) -> Vec<&'a sbx::Volume> {
    all.iter()
        .filter(|v| {
            label(v, PURPOSE_LABEL) == Some(purpose)
                && vstate(v) == sbx::VolumeState::Available
                && !used(v).is_empty()
                && used(v) < cutoff
        })
        .collect()
}

fn used(v: &sbx::Volume) -> &str {
    v.meta.as_ref().map_or("", |m| m.update_time.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "build-workspace";

    fn vol(
        name: &str,
        labels: &[(&str, &str)],
        state: sbx::VolumeState,
        used: &str,
    ) -> sbx::Volume {
        let mut meta = sylphx::common::ResourceMeta::default();
        for (k, v) in labels {
            meta.labels.insert((*k).to_string(), (*v).to_string());
        }
        meta.update_time = used.into();
        let mut st = sbx::VolumeStatus::default();
        st.state = Some(state);
        let mut v = sbx::Volume::default();
        v.name = name.into();
        v.meta = Some(meta);
        v.status = Some(st);
        v
    }

    /// A pool is one purpose, one scope and one region: the home region is
    /// the pool without a region label.
    #[test]
    fn a_pool_is_one_purpose_scope_and_region() {
        let a = sbx::VolumeState::Available;
        let home = vol(
            "h",
            &[(PURPOSE_LABEL, P), ("build-repo", "r")],
            a.clone(),
            "",
        );
        let gra = vol(
            "g",
            &[
                (PURPOSE_LABEL, P),
                ("build-repo", "r"),
                (REGION_LABEL, "gra"),
            ],
            a.clone(),
            "",
        );
        let other = vol(
            "o",
            &[(PURPOSE_LABEL, P), ("build-repo", "s")],
            a.clone(),
            "",
        );
        let image = vol(
            "i",
            &[(PURPOSE_LABEL, "image-build-state"), ("build-repo", "r")],
            a,
            "",
        );
        assert!(in_pool(&home, P, "build-repo", "r", None));
        assert!(!in_pool(&gra, P, "build-repo", "r", None));
        assert!(in_pool(&gra, P, "build-repo", "r", Some("gra")));
        assert!(!in_pool(&home, P, "build-repo", "r", Some("gra")));
        assert!(!in_pool(&gra, P, "build-repo", "r", Some("fra")));
        assert!(!in_pool(&other, P, "build-repo", "r", None));
        assert!(!in_pool(&image, P, "build-repo", "r", None));
        assert!(!in_pool(&home, P, "build-scope", "r", None));
    }

    /// A full region gives up its least recently used free Volume of the
    /// same purpose, of any scope, and never an attached, creating,
    /// other-region, other-purpose or refused one.
    #[test]
    fn the_least_recently_used_free_volume_gives_way() {
        use sbx::VolumeState::{Attached, Available, Creating};
        let l = |r: &'static str| vec![(PURPOSE_LABEL, P), ("build-repo", r)];
        let all = vec![
            vol("busy", &l("busy"), Attached, "2026-10-05T00:00:00.000Z"),
            vol("new", &l("new"), Creating, "2026-10-05T00:00:00.000Z"),
            vol(
                "gra",
                &[
                    (PURPOSE_LABEL, P),
                    ("build-repo", "gra"),
                    (REGION_LABEL, "gra"),
                ],
                Available,
                "2026-10-05T00:00:00.000Z",
            ),
            vol(
                "recent",
                &l("recent"),
                Available,
                "2026-10-06T01:54:00.000Z",
            ),
            vol(
                "oldest",
                &l("oldest"),
                Available,
                "2026-10-05T20:45:00.000Z",
            ),
            vol(
                "data",
                &[(PURPOSE_LABEL, "data")],
                Available,
                "2026-10-01T00:00:00.000Z",
            ),
        ];
        let name = |v: Option<&sbx::Volume>| v.map(|v| v.name.clone());
        assert_eq!(
            name(lru_free(&all, P, None, |_| true)).as_deref(),
            Some("oldest")
        );
        assert_eq!(
            name(lru_free(&all, P, Some("gra"), |_| true)).as_deref(),
            Some("gra")
        );
        assert!(lru_free(&all[..2], P, None, |_| true).is_none());
        // A narrower pick (one tenant's Volumes) never takes another's.
        assert_eq!(
            name(lru_free(&all, P, None, |v| v.name != "oldest")).as_deref(),
            Some("recent")
        );
    }

    /// A build takes a free warm Volume whose node has room: one whose node
    /// had none is skipped while another free warm Volume or room for a
    /// fresh one remains, and is waited on again only when neither does.
    #[test]
    fn a_build_skips_a_warm_volume_whose_node_is_full() {
        use sbx::VolumeState::{Attached, Available};
        let pool = vec![
            vol("v-b", &[], Available, ""),
            vol("v-a", &[], Available, ""),
            vol("v-c", &[], Attached, ""),
        ];
        let names = |f: Vec<&sbx::Volume>| f.iter().map(|v| v.name.clone()).collect::<Vec<_>>();
        let mut full = HashSet::new();
        assert_eq!(names(free_warm(&pool, &mut full, true)), ["v-a", "v-b"]);
        full.insert("v-a".to_string());
        assert_eq!(names(free_warm(&pool, &mut full, true)), ["v-b"]);
        full.insert("v-b".to_string());
        assert!(free_warm(&pool, &mut full, true).is_empty());
        assert_eq!(full.len(), 2);
        assert_eq!(names(free_warm(&pool, &mut full, false)), ["v-a", "v-b"]);
        assert!(full.is_empty());
    }

    /// Idle expiry takes only free Volumes of the purpose last used before
    /// the cutoff; a Volume with no recorded use is left alone.
    #[test]
    fn only_free_volumes_idle_past_the_cutoff_expire() {
        use sbx::VolumeState::{Attached, Available};
        let p = [(PURPOSE_LABEL, P)];
        let all = vec![
            vol("old", &p, Available, "2026-09-20T00:00:00.000Z"),
            vol("old-busy", &p, Attached, "2026-09-20T00:00:00.000Z"),
            vol("fresh", &p, Available, "2026-10-06T00:00:00.000Z"),
            vol("unknown", &p, Available, ""),
            vol(
                "other",
                &[(PURPOSE_LABEL, "data")],
                Available,
                "2026-09-01T00:00:00.000Z",
            ),
        ];
        let got: Vec<&str> = idle_before(&all, P, "2026-09-22T00:00:00.000Z")
            .iter()
            .map(|v| v.name.as_str())
            .collect();
        assert_eq!(got, ["old"]);
    }
}
