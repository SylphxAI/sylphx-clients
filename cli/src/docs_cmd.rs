//! `sylphx data docs sync|search|read`: a Markdown tree kept in one of the
//! environment's search indexes, chunked by heading with its paths and line
//! numbers, and searched and read back through the Search API
//! (`sylphx_mcp::docs`; the MCP server's `docs_search` and `docs_read` are
//! the same calls).
//!
//! `sync` is meant to run on each merge of the repository that holds the
//! docs (a CI step with an environment key holding `data:write`); readers
//! need only `data:read` on that environment, not the repository.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{json, Value};
use sylphx::Client;
use sylphx_mcp::docs::{self, DocsIndex};

use crate::output::{self, Format};
use crate::Failure;

fn index_args(c: Command) -> Command {
    c.arg(
        Arg::new("index")
            .long("index")
            .value_name("ID")
            .help("The search index's id in the environment; default: SYLPHX_DOCS_INDEX"),
    )
    .arg(Arg::new("source").long("source").value_name("NAME").help(
        "Only this source's documents, when the index holds several; default: SYLPHX_DOCS_SOURCE",
    ))
}

pub fn command() -> Command {
    Command::new("docs")
        .about("Markdown docs in a search index: sync a tree on each merge, search it, read it by path and line")
        .subcommand_required(true)
        .subcommand(index_args(
            Command::new("sync")
                .about("Write a Markdown tree to a search index, one document per heading section, and delete what is gone")
                .long_about("Chunks every matching Markdown file at its #, ## and ### headings (outside code fences) and writes each chunk as a document with its path, title, heading, first and last line, text and the revision. Then deletes the documents it wrote for this source (none named: those with no source) at any other revision, so removed files and sections leave no stale hits; other documents in the index are never touched. --dry-run counts what it would write and delete. A run that matches no file is refused, so a wrong pattern never empties the index.\n\nExample (on each merge to main):\n  sylphx data docs sync --index docs --revision \"$GITHUB_SHA\" README.md 'docs/**/*.md'")
                .arg(
                    Arg::new("patterns")
                        .value_name("GLOB")
                        .num_args(0..)
                        .action(ArgAction::Append)
                        .help("Paths below --root to index (`*`, `**`, `?`); default: **/*.md"),
                )
                .arg(
                    Arg::new("root")
                        .long("root")
                        .value_name("DIR")
                        .default_value(".")
                        .help("The tree's root; paths are stored relative to it"),
                )
                .arg(
                    Arg::new("revision")
                        .long("revision")
                        .value_name("REV")
                        .help("The tree's revision, e.g. the commit SHA; default: the current time"),
                )
                .arg(
                    Arg::new("dry-run")
                        .long("dry-run")
                        .action(ArgAction::SetTrue)
                        .help("Count files and chunks; write and delete nothing"),
                ),
        ))
        .subcommand(index_args(
            Command::new("search")
                .about("Search the docs: the best sections as path:line with the matching line")
                .arg(
                    Arg::new("query")
                        .value_name("QUERY")
                        .required(true)
                        .num_args(1..)
                        .help("Words (all must match), \"quoted phrases\", `or`, -word"),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .value_name("N")
                        .value_parser(clap::value_parser!(u32).range(1..=i64::from(docs::MAX_SEARCH_LIMIT)))
                        .help("Hits to return; default 8"),
                ),
        ))
        .subcommand(index_args(
            Command::new("read")
                .about("Read a doc by path, whole or a line range")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .help("The file's path, e.g. docs/api/limits.md; PATH:LINE reads from that line"),
                )
                .arg(
                    Arg::new("lines")
                        .long("lines")
                        .value_name("A-B")
                        .help("Lines A to B, inclusive (A- reads from A)"),
                ),
        ))
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn docs_index(m: &ArgMatches) -> Result<DocsIndex, Failure> {
    let index = m
        .get_one::<String>("index")
        .cloned()
        .or_else(|| env(docs::INDEX_ENV))
        .ok_or_else(|| {
            Failure::Usage(format!(
                "name the search index: --index ID or {}",
                docs::INDEX_ENV
            ))
        })?;
    let source = m
        .get_one::<String>("source")
        .cloned()
        .or_else(|| env(docs::SOURCE_ENV));
    Ok(DocsIndex { index, source })
}

/// `A-B`, `A-` or `A` as (start, end).
fn parse_lines(s: &str) -> Result<(Option<usize>, Option<usize>), Failure> {
    let bad = || Failure::Usage(format!("--lines takes A-B, A- or A, got {s:?}"));
    let num = |x: &str| x.trim().parse::<usize>().map_err(|_| bad());
    match s.split_once('-') {
        Some((a, b)) if b.trim().is_empty() => Ok((Some(num(a)?), None)),
        Some((a, b)) => Ok((Some(num(a)?), Some(num(b)?))),
        None => {
            let a = num(s)?;
            Ok((Some(a), Some(a)))
        }
    }
}

pub async fn run(client: &Client, m: &ArgMatches, format: Format) -> Result<(), Failure> {
    let (verb, vm) = m.subcommand().expect("subcommand_required");
    let docs = docs_index(vm)?;
    match verb {
        "sync" => {
            let root = PathBuf::from(vm.get_one::<String>("root").expect("defaulted"));
            let patterns: Vec<String> = vm
                .get_many::<String>("patterns")
                .map(|p| p.cloned().collect())
                .filter(|p: &Vec<String>| !p.is_empty())
                .unwrap_or_else(|| vec!["**/*.md".to_string()]);
            let files = docs::collect_files(&root, &patterns)
                .map_err(|e| Failure::Refused(format!("reading {}: {e}", root.display())))?;
            let revision = vm
                .get_one::<String>("revision")
                .cloned()
                .unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis().to_string())
                        .unwrap_or_default()
                });
            let r = docs::sync(client, &docs, &revision, &files, vm.get_flag("dry-run")).await?;
            let v = json!({
                "index": docs.index, "source": docs.source, "revision": revision,
                "files": r.files, "chunks": r.chunks, "written": r.written, "deleted": r.deleted,
                "stale": r.stale, "dry_run": vm.get_flag("dry-run"),
            });
            if format == Format::Table && vm.get_flag("dry-run") {
                println!(
                    "dry run: {} files, {} chunks to write, {} stale to delete (index {})",
                    r.files, r.chunks, r.stale, docs.index
                );
            } else if format == Format::Table {
                println!(
                    "{} files, {} chunks: {} written, {} stale deleted (index {}, revision {revision})",
                    r.files, r.chunks, r.written, r.deleted, docs.index
                );
            } else {
                println!("{}", output::render(&v, format));
            }
            Ok(())
        }
        "search" => {
            let query = vm
                .get_many::<String>("query")
                .expect("required")
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            let v =
                docs::search(client, &docs, &query, vm.get_one::<u32>("limit").copied()).await?;
            if format == Format::Table {
                print_hits(&v);
            } else {
                println!("{}", output::render(&v, format));
            }
            Ok(())
        }
        "read" => {
            let raw = vm.get_one::<String>("path").expect("required");
            let (path, mut range) = match raw.rsplit_once(':') {
                Some((p, l)) if !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()) => {
                    (p.to_string(), (l.parse().ok(), None))
                }
                _ => (raw.clone(), (None, None)),
            };
            if let Some(l) = vm.get_one::<String>("lines") {
                range = parse_lines(l)?;
            }
            let v = docs::read(client, &docs, &path, range.0, range.1).await?;
            if format == Format::Table {
                println!("{}", v["text"].as_str().unwrap_or_default());
                if v["truncated"] == true {
                    eprintln!(
                        "({} of {} lines; read on with --lines {}-)",
                        v["end_line"],
                        v["total_lines"],
                        v["end_line"].as_u64().unwrap_or(0) + 1
                    );
                }
            } else {
                println!("{}", output::render(&v, format));
            }
            Ok(())
        }
        other => Err(Failure::Usage(format!(
            "unknown command `data docs {other}`"
        ))),
    }
}

fn print_hits(v: &Value) {
    let hits = v["hits"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        println!("no match");
        return;
    }
    for h in hits {
        let heading = h["heading"].as_str().unwrap_or_default();
        println!(
            "{}{}",
            h["ref"].as_str().unwrap_or_default(),
            if heading.is_empty() {
                String::new()
            } else {
                format!("  {heading}")
            }
        );
        println!("    {}", h["snippet"].as_str().unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_ranges_parse() {
        assert_eq!(parse_lines("10-40").ok(), Some((Some(10), Some(40))));
        assert_eq!(parse_lines("10-").ok(), Some((Some(10), None)));
        assert_eq!(parse_lines("7").ok(), Some((Some(7), Some(7))));
        assert!(parse_lines("x-1").is_err());
    }

    #[test]
    fn the_command_tree_is_valid() {
        command().debug_assert();
    }
}
