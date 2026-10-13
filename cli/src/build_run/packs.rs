//! Content-addressed layers with path-anchored boundaries. Size splits restart
//! at every anchor, so an edit cannot shift pack membership through the tree.

use std::path::Path;

use sha2::{Digest, Sha256};

use super::sync::{self, Mode, Tree};

const RAW_CAP: u64 = 4 << 20;
const KEY_HEADER: &[u8] = b"sylphx-tree-pack 1\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pack {
    pub key: String,
    pub paths: Vec<String>,
    pub raw: u64,
}

fn eligible(path: &str, mode: Mode) -> bool {
    mode != Mode::Link
        && !path.is_empty()
        && !Path::new(path).is_absolute()
        && !path
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | '\"' | '\''))
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

fn anchor(path: &str) -> bool {
    // The final six bits are SHA256(path) modulo 64, independent of content.
    Sha256::digest(path.as_bytes())[31] % 64 == 0
}

fn key(tree: &Tree, paths: &[String]) -> String {
    let mut h = Sha256::new();
    h.update(KEY_HEADER);
    for path in paths {
        h.update(sync::line(path, &tree[path]).as_bytes());
    }
    sync::hex(&h.finalize())
}

fn flush(out: &mut Vec<Pack>, tree: &Tree, paths: &mut Vec<String>, raw: &mut u64) {
    if paths.is_empty() {
        return;
    }
    out.push(Pack {
        key: key(tree, paths),
        paths: std::mem::take(paths),
        raw: std::mem::take(raw),
    });
}

/// Plans sorted, deduplicated regular-file packs from `paths`. Unsafe names and
/// links stay on the direct sync path. Each anchor starts a fresh segment before
/// that path; each segment is independently split at 4 MiB of raw content. A
/// larger file is a singleton, allowing it to be cached without enlarging peers.
pub fn plan(tree: &Tree, paths: &[String]) -> Vec<Pack> {
    let mut sorted: Vec<_> = paths.iter().collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut raw = 0;
    for path in sorted {
        let Some(e) = tree.get(path).filter(|e| eligible(path, e.mode)) else {
            continue;
        };
        if anchor(path) || e.size > RAW_CAP - raw {
            flush(&mut out, tree, &mut current, &mut raw);
        }
        raw += e.size;
        current.push(path.clone());
        if e.size > RAW_CAP {
            flush(&mut out, tree, &mut current, &mut raw);
        }
    }
    flush(&mut out, tree, &mut current, &mut raw);
    out
}

/// Encodes deterministic gzip/pax tar bytes only for the scanned manifest.
/// Content, type, size or executable-mode mutations fail instead of poisoning
/// the immutable pack cache. The shared sync tar writer is the only serializer.
pub fn encode(root: &Path, tree: &Tree, pack: &Pack) -> Result<Vec<u8>, String> {
    if pack.paths.is_empty() || pack.paths.windows(2).any(|p| p[0] >= p[1]) {
        return Err("pack paths must be nonempty, sorted and unique".into());
    }
    let mut raw = 0u64;
    for path in &pack.paths {
        let e = tree
            .get(path)
            .filter(|e| eligible(path, e.mode))
            .ok_or_else(|| format!("{path}: pack requires a safe regular-file manifest entry"))?;
        raw = raw
            .checked_add(e.size)
            .ok_or_else(|| "pack raw size overflow".to_string())?;
    }
    if raw != pack.raw || (raw > RAW_CAP && pack.paths.len() != 1) {
        return Err("pack raw size does not match a bounded manifest pack".into());
    }
    if key(tree, &pack.paths) != pack.key {
        return Err("pack key does not match the scanned manifest".into());
    }
    sync::checked_tar(root, &pack.paths, tree)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn entry(bytes: &[u8]) -> sync::Entry {
        sync::Entry {
            digest: sync::sha256_hex(bytes),
            mode: Mode::File,
            size: bytes.len() as u64,
        }
    }

    fn fixture() -> Tree {
        (0..2048)
            .map(|i| (format!("src/{i:04}.rs"), entry(b"original")))
            .collect()
    }

    fn all(tree: &Tree) -> Vec<Pack> {
        plan(tree, &tree.keys().cloned().collect::<Vec<_>>())
    }

    fn segment(tree: &Tree) -> (String, String, String) {
        let anchors: Vec<_> = tree.keys().filter(|p| anchor(p)).collect();
        let (start, end, victim) = anchors
            .windows(2)
            .find_map(|pair| {
                let victim = tree
                    .range(pair[0].clone()..pair[1].clone())
                    .map(|(p, _)| p)
                    .find(|p| *p != pair[0]);
                victim.map(|victim| (pair[0], pair[1], victim))
            })
            .map(|(start, end, victim)| (start.clone(), end.clone(), victim.clone()))
            .expect("fixture has an anchored segment with a non-anchor file");
        (start, end, victim)
    }

    fn outside(packs: Vec<Pack>, start: &str, end: &str) -> Vec<Pack> {
        packs
            .into_iter()
            .filter(|p| {
                p.paths
                    .iter()
                    .all(|path| path.as_str() < start || path.as_str() >= end)
            })
            .collect()
    }

    #[test]
    fn a_content_edit_changes_only_its_pack() {
        let mut tree = fixture();
        let before = all(&tree);
        let victim = tree.keys().nth(1000).unwrap().clone();
        tree.insert(victim.clone(), entry(b"modified"));
        let after = all(&tree);
        assert_eq!(before.len(), after.len());
        let changed: Vec<_> = before
            .iter()
            .zip(&after)
            .filter(|(a, b)| a.key != b.key)
            .collect();
        assert_eq!(changed.len(), 1);
        assert!(changed[0].0.paths.contains(&victim));
        assert_eq!(changed[0].0.paths, changed[0].1.paths);
        for (a, b) in before.iter().zip(&after) {
            assert_eq!(a.paths, b.paths);
        }
    }

    #[test]
    fn insertion_removal_and_size_edits_stay_inside_their_anchor_segment() {
        let tree = fixture();
        let (start, end, victim) = segment(&tree);
        let baseline = outside(all(&tree), &start, &end);
        let mut removed = tree.clone();
        removed.remove(&victim);
        assert_eq!(outside(all(&removed), &start, &end), baseline);
        let mut inserted = tree.clone();
        inserted.insert(format!("{victim}.new"), entry(b"new"));
        assert_eq!(outside(all(&inserted), &start, &end), baseline);
        let mut grown = tree;
        grown.get_mut(&victim).unwrap().size = RAW_CAP;
        grown.get_mut(&victim).unwrap().digest = sync::sha256_hex(b"grown");
        assert_eq!(outside(all(&grown), &start, &end), baseline);
    }

    #[test]
    fn anchors_begin_packs_and_removing_an_anchor_does_not_shift_other_segments() {
        let mut tree = fixture();
        let before = all(&tree);
        for pack in &before {
            assert!(pack.paths.iter().skip(1).all(|p| !anchor(p)));
        }
        assert!(before.iter().skip(1).all(|p| anchor(&p.paths[0])));
        let anchors: Vec<_> = tree.keys().filter(|p| anchor(p)).cloned().collect();
        let baseline = outside(before, &anchors[0], &anchors[2]);
        tree.remove(&anchors[1]);
        assert_eq!(outside(all(&tree), &anchors[0], &anchors[2]), baseline);
    }

    #[test]
    fn caps_restart_independently_and_large_files_are_singletons() {
        let paths: Vec<_> = (0..100)
            .map(|i| format!("file-{i:03}"))
            .filter(|p| !anchor(p))
            .take(5)
            .collect();
        let mut tree = Tree::new();
        for path in &paths {
            let mut e = entry(b"sized");
            e.size = 2 << 20;
            tree.insert(path.clone(), e);
        }
        let p = all(&tree);
        assert_eq!(
            p.iter().map(|p| p.paths.len()).collect::<Vec<_>>(),
            [2, 2, 1]
        );
        assert!(p.iter().all(|p| p.raw <= RAW_CAP));
        tree.get_mut(&paths[2]).unwrap().size = RAW_CAP + 1;
        let p = all(&tree);
        let big = p.iter().find(|p| p.paths.contains(&paths[2])).unwrap();
        assert_eq!(big.paths, vec![paths[2].clone()]);
        assert_eq!(big.raw, RAW_CAP + 1);
        assert!(p.iter().all(|p| p.raw <= RAW_CAP || p.paths.len() == 1));
        assert_eq!(
            p.iter().flat_map(|p| &p.paths).collect::<Vec<_>>(),
            paths.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn eligibility_is_planned_and_empty_files_are_cached() {
        let mut tree = Tree::new();
        for path in [
            "normal",
            "empty",
            "sp ace",
            "unicode/文件",
            "back\\slash",
            "double\"quote",
            "single'quote",
            "tab\tname",
            "line\nname",
            "nul\0name",
            "del\u{7f}name",
            "../escape",
            "/absolute",
            "a/../b",
            "a/./b",
            "a//b",
            "a/",
            "",
            "link",
        ] {
            tree.insert(path.into(), entry(b""));
        }
        tree.get_mut("link").unwrap().mode = Mode::Link;
        let mut paths: Vec<_> = tree.keys().cloned().collect();
        paths.reverse();
        paths.push("normal".into());
        paths.push("missing".into());
        let packs = plan(&tree, &paths);
        let assigned: BTreeSet<_> = packs.iter().flat_map(|p| p.paths.iter().cloned()).collect();
        assert_eq!(
            assigned,
            ["empty", "normal", "sp ace", "unicode/文件"]
                .map(String::from)
                .into()
        );
        assert_eq!(packs, all(&tree));
        assert!(packs.iter().all(|p| p.raw == 0));
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "sylphx-packs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            let full = self.0.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, bytes).unwrap();
        }

        fn scan(&self, paths: &[String]) -> Tree {
            sync::scan(&self.0, paths, &mut sync::StatCache::default()).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn checked_encoding_rejects_mutations_and_wrong_manifest_keys() {
        let root = Scratch::new();
        root.write("a", b"original");
        let paths = vec!["a".into()];
        let tree = root.scan(&paths);
        let pack = &plan(&tree, &paths)[0];
        let bytes = encode(&root.0, &tree, pack).unwrap();
        assert_eq!(bytes, encode(&root.0, &tree, pack).unwrap());
        root.write("a", b"modified");
        assert!(encode(&root.0, &tree, pack)
            .unwrap_err()
            .contains("changed since scan"));
        root.write("a", b"short");
        assert!(encode(&root.0, &tree, pack).is_err());
        root.write("a", b"original and larger");
        assert!(encode(&root.0, &tree, pack).is_err());
        root.write("a", b"original");
        assert_eq!(bytes, encode(&root.0, &tree, pack).unwrap());
        let mut wrong = pack.clone();
        wrong.key = "0".repeat(64);
        assert!(encode(&root.0, &tree, &wrong).unwrap_err().contains("key"));
        wrong = pack.clone();
        wrong.raw += 1;
        assert!(encode(&root.0, &tree, &wrong).unwrap_err().contains("size"));
    }

    #[cfg(unix)]
    #[test]
    fn mode_edits_change_keys_and_unscanned_modes_and_links_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let root = Scratch::new();
        root.write("run", b"#!/bin/sh\n");
        let paths = vec!["run".into()];
        let tree = root.scan(&paths);
        let before = plan(&tree, &paths);
        std::fs::set_permissions(root.0.join("run"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(encode(&root.0, &tree, &before[0])
            .unwrap_err()
            .contains("mode"));
        let tree = root.scan(&paths);
        let after = plan(&tree, &paths);
        assert_ne!(before[0].key, after[0].key);
        assert!(encode(&root.0, &tree, &after[0]).is_ok());
        root.write("target", b"#!/bin/sh\n");
        std::fs::remove_file(root.0.join("run")).unwrap();
        std::os::unix::fs::symlink("target", root.0.join("run")).unwrap();
        assert!(encode(&root.0, &tree, &after[0])
            .unwrap_err()
            .contains("regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn deterministic_pax_tar_extracts_exact_current_contents_and_modes() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        let root = Scratch::new();
        let dest = Scratch::new();
        let long = format!("{}/文件.rs", "directory".repeat(15));
        let paths = vec![
            "empty".into(),
            "sp ace".into(),
            "run".into(),
            "large".into(),
            long.clone(),
        ];
        root.write("empty", b"");
        root.write("run", b"#!/bin/sh\nexit 0\n");
        root.write(&long, b"pax path\n");
        root.write("large", &vec![b'x'; RAW_CAP as usize + 1]);
        // Re-extract a freshly scanned edit into the same destination, including
        // an executable-to-regular mode change, not only the initial snapshot.
        for (contents, mode) in [
            (b"current contents\n".as_slice(), 0o755),
            (b"edited contents\n".as_slice(), 0o644),
        ] {
            root.write("sp ace", contents);
            std::fs::set_permissions(root.0.join("run"), std::fs::Permissions::from_mode(mode))
                .unwrap();
            let tree = root.scan(&paths);
            for (i, pack) in plan(&tree, &paths).iter().enumerate() {
                let bytes = encode(&root.0, &tree, pack).unwrap();
                assert_eq!(bytes, encode(&root.0, &tree, pack).unwrap());
                assert_eq!(pack.key.len(), 64);
                assert!(pack
                    .key
                    .bytes()
                    .all(|b| { b.is_ascii_digit() || (b'a'..=b'f').contains(&b) }));
                let archive = root.0.join(format!("{i}.tar.gz"));
                std::fs::write(&archive, bytes).unwrap();
                let out = Command::new("tar")
                    .arg("-xzf")
                    .arg(&archive)
                    .arg("-C")
                    .arg(&dest.0)
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            assert_eq!(dest.scan(&paths), tree);
            assert_eq!(std::fs::read(dest.0.join("sp ace")).unwrap(), contents);
            assert_eq!(
                std::fs::metadata(dest.0.join("run"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                mode,
            );
        }
        assert_eq!(std::fs::read(dest.0.join(&long)).unwrap(), b"pax path\n");
        assert_eq!(
            sync::file_set(&dest.0)
                .unwrap()
                .into_iter()
                .collect::<BTreeSet<_>>(),
            paths.into_iter().collect(),
        );
    }
}
