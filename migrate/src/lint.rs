//! Static policy for the supported Slice 1 PostgreSQL schema subset.
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
};

use serde::Serialize;

use super::{
    sql::{self, Kind, Statement, Token},
    Migration,
};

type Tokens<'a> = [Token<'a>];
type Object = Vec<String>;

#[derive(Debug, Serialize)]
pub struct Diagnostic<'a> {
    pub(super) file: &'a str,
    pub(super) line: usize,
    pub(super) rule_id: &'static str,
    pub(super) hint: &'static str,
}

pub(super) struct Checked<'a> {
    pub(super) diagnostics: Vec<Diagnostic<'a>>,
    pub(super) spans: Vec<Range<usize>>,
    pub(super) concurrent: bool,
    pub(super) overrides: Vec<&'a str>,
}

fn emit<'a>(out: &mut Checked<'a>, m: &'a Migration, line: usize, (rule_id, hint): Rule) {
    out.diagnostics.push(Diagnostic {
        file: &m.file,
        line,
        rule_id,
        hint,
    });
}

type Rule = (&'static str, &'static str);
const LEX: Rule = (
    "MIG001",
    "Use supported PostgreSQL quoting, balanced delimiters, and UTF-8 SQL.",
);
const UNSUPPORTED: Rule = (
    "MIG002",
    "Use supported schema DDL only; move data changes and dynamic SQL to an out-of-band job.",
);
const ADD_NOT_NULL: Rule = ("MIG101", "Add a nullable column or use a simple non-NULL literal DEFAULT; backfill separately, then enforce NOT NULL.");
const COLUMN_TYPE: Rule = (
    "MIG102",
    "Add a new typed column, dual-write, backfill separately, then retire the old column.",
);
const CONSTRAINT: Rule = ("MIG103", "Add a CHECK or FOREIGN KEY constraint with NOT VALID, then VALIDATE it in a separate migration.");
const INDEX: Rule = (
    "MIG104",
    "Use CREATE INDEX CONCURRENTLY or DROP INDEX CONCURRENTLY in its own migration file.",
);
const DROP: Rule = ("MIG105", "Use expand/contract, then put '-- sylphx:allow-destructive <ticket>: <reason>' immediately before the reviewed statement.");
const RLS: Rule = ("MIG106", "ENABLE and FORCE ROW LEVEL SECURITY on this tenant table in the same migration, before commit.");
const CONCURRENT: Rule = (
    "MIG107",
    "Put the concurrent index operation alone in a file without BEGIN or COMMIT.",
);
const TRANSACTION: Rule = (
    "MIG108",
    "Remove explicit transaction control; the runner owns transaction and ledger boundaries.",
);
const RESERVED: Rule = (
    "MIG109",
    "Do not modify or reference migration ledger objects in migration SQL.",
);

fn starts(t: &Tokens<'_>, words: &[&str]) -> bool {
    t.len() >= words.len() && t.iter().zip(words).all(|(t, w)| t.is(w))
}
fn has(t: &Tokens<'_>, words: &[&str]) -> bool {
    t.windows(words.len()).any(|w| starts(w, words))
}
fn skip<'t, 'a>(t: &'t Tokens<'a>, words: &[&str]) -> &'t Tokens<'a> {
    if starts(t, words) {
        &t[words.len()..]
    } else {
        t
    }
}
fn object<'t, 'a>(t: &'t Tokens<'a>) -> Option<(Object, &'t Tokens<'a>)> {
    let mut parts = vec![t.first()?.ident()?];
    let mut rest = &t[1..];
    while starts(rest, &["."]) {
        parts.push(rest.get(1)?.ident()?);
        rest = &rest[2..];
    }
    // Normalize explicit public with its unqualified spelling. The runner
    // fixes search_path to public; all other schemas must be explicit.
    if parts.len() == 2 && parts[0] == "public" {
        parts.remove(0);
    }
    Some((parts, rest))
}

fn split_outer<'t, 'a>(t: &'t Tokens<'a>) -> Option<Vec<&'t Tokens<'a>>> {
    let (mut depth, mut start) = (0usize, 0);
    let mut parts = Vec::new();
    for (i, token) in t.iter().enumerate() {
        if token.is("(") || token.is("[") {
            depth += 1;
        }
        if token.is(")") || token.is("]") {
            depth = depth.checked_sub(1)?;
        }
        if token.is(",") && depth == 0 {
            parts.push(&t[start..i]);
            start = i + 1;
        }
    }
    if depth != 0 {
        return None;
    }
    parts.push(&t[start..]);
    Some(parts)
}

// Require a column-list CREATE TABLE, not CREATE TABLE AS (which can
// populate the target from SELECT, TABLE, VALUES, or EXECUTE).
fn plain_table(tokens: &Tokens<'_>) -> bool {
    if !starts(tokens, &["("]) {
        return false;
    }
    let mut depth = 0usize;
    for t in tokens {
        if t.is("(") {
            depth += 1;
        } else if t.is(")") {
            let Some(next) = depth.checked_sub(1) else {
                return false;
            };
            depth = next;
        } else if depth == 0 && t.is("as") {
            return false;
        }
    }
    depth == 0
}

fn literal_default(t: &Tokens<'_>) -> bool {
    let Some(i) = t.iter().position(|t| t.is("default")) else {
        return false;
    };
    let mut rest = &t[i + 1..];
    if starts(rest, &["+"]) || starts(rest, &["-"]) {
        rest = &rest[1..];
    }
    let Some(first) = rest.first() else {
        return false;
    };
    if first.kind != Kind::String
        && first.kind != Kind::Number
        && !first.is("true")
        && !first.is("false")
    {
        return false;
    }
    rest = &rest[1..];
    if first.kind == Kind::Number
        && starts(rest, &["."])
        && rest.get(1).is_some_and(|t| t.kind == Kind::Number)
    {
        rest = &rest[2..];
    }
    // Deliberately conservative: casts, function calls and expressions need
    // catalog volatility/rewriting evidence which a static check cannot know.
    rest.is_empty()
        || [
            "not",
            "null",
            "constraint",
            "check",
            "references",
            "unique",
            "primary",
            "collate",
        ]
        .iter()
        .any(|w| starts(rest, &[*w]))
}

fn allow_destructive<'a>(s: &Statement<'a>) -> Option<&'a str> {
    s.comments.last().and_then(|c| {
        let note = c.trim().strip_prefix("sylphx:allow-destructive ")?.trim();
        let (ticket, reason) = note.split_once(':')?;
        (ticket.chars().any(|c| c.is_ascii_digit())
            && ticket
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'#')
            && !reason.trim().is_empty())
        .then_some(note)
    })
}

pub(super) fn check(m: &Migration) -> Checked<'_> {
    let mut out = Checked {
        diagnostics: Vec::new(),
        spans: Vec::new(),
        concurrent: false,
        overrides: Vec::new(),
    };
    let statements = match sql::scan(&m.sql) {
        Ok(s) if s.is_empty() => {
            emit(&mut out, m, 1, UNSUPPORTED);
            return out;
        }
        Ok(s) => s,
        Err(line) => {
            emit(&mut out, m, line, LEX);
            return out;
        }
    };
    let mut fresh = BTreeSet::new();
    let mut fresh_indexes = BTreeSet::new();
    let mut tenant = BTreeMap::new();
    for s in &statements {
        let t = &s.tokens[..];
        let mut rules = Vec::new();
        let mut destructive = false;
        let word = |w| starts(t, &[w]);
        if t.iter().filter_map(Token::ident).any(|i| {
            matches!(
                i.as_str(),
                "_sylphx_migrations" | "_sqlx_migrations" | "atlas_schema_revisions"
            )
        }) {
            rules.push(RESERVED);
        }
        let create_index = word("create")
            && (starts(t, &["create", "index"]) || starts(t, &["create", "unique", "index"]));
        let drop_index = starts(t, &["drop", "index"]);
        if create_index || drop_index {
            let rest = if starts(t, &["create", "unique", "index"]) {
                &t[3..]
            } else {
                &t[2..]
            };
            let concurrently = starts(rest, &["concurrently"]);
            let rest = skip(
                skip(rest, &["concurrently"]),
                if create_index {
                    &["if", "not", "exists"]
                } else {
                    &["if", "exists"]
                },
            );
            let mut safe_fresh = false;
            if create_index {
                let (index, tail) = if starts(rest, &["on"]) {
                    (None, rest)
                } else if let Some((index, tail)) = object(rest) {
                    (Some(index), tail)
                } else {
                    (None, &rest[..0])
                };
                if let Some((table, _)) = starts(tail, &["on"])
                    .then(|| object(skip(&tail[1..], &["only"])))
                    .flatten()
                {
                    safe_fresh = fresh.contains(&table);
                    if safe_fresh {
                        if let Some(mut index) = index {
                            if index.len() == 1 && table.len() == 2 {
                                index.insert(0, table[0].clone());
                            }
                            fresh_indexes.insert(index);
                        }
                    }
                }
                // IF NOT EXISTS does not prove that the named existing index
                // is valid or matches this definition after interruption.
                if concurrently && has(t, &["if", "not", "exists"]) {
                    rules.push(CONCURRENT);
                }
            } else if let Some((index, tail)) = object(rest) {
                safe_fresh = (tail.is_empty() || starts(tail, &["restrict"]))
                    && fresh_indexes.remove(&index);
            }
            if concurrently {
                out.concurrent = true;
                if statements.len() != 1 {
                    rules.push(CONCURRENT);
                }
            } else if !safe_fresh {
                rules.push(INDEX);
            }
        } else if starts(t, &["create", "table"]) {
            let guarded = starts(&t[2..], &["if", "not", "exists"]);
            let decl = object(skip(&t[2..], &["if", "not", "exists"]));
            if let Some((table, rest)) = decl.filter(|(_, r)| plain_table(r)) {
                if !guarded && !has(rest, &["partition", "of"]) {
                    fresh.insert(table.clone());
                }
                let tenant_col = rest.windows(2).any(|w| {
                    (w[0].is("(") || w[0].is(","))
                        && w[1].ident().is_some_and(|i| {
                            matches!(i.as_str(), "tenant_id" | "org_id" | "organization_id")
                        })
                });
                if tenant_col
                    || s.comments
                        .iter()
                        .any(|c| c.trim() == "sylphx:tenant-scoped")
                {
                    tenant.insert(table, (s.line, false, false));
                }
            } else {
                rules.push(UNSUPPORTED);
            }
        } else if starts(t, &["alter", "table"]) {
            if let Some((table, rest)) = object(skip(skip(&t[2..], &["if", "exists"]), &["only"])) {
                if let Some(parts) = split_outer(rest) {
                    for a in parts {
                        let action = skip(a, &["alter"]);
                        let action = skip(action, &["column"]);
                        let constraints = [
                            "constraint",
                            "check",
                            "foreign",
                            "primary",
                            "unique",
                            "exclude",
                        ];
                        if starts(a, &["add"]) && constraints.iter().any(|x| starts(&a[1..], &[*x]))
                        {
                            if !has(a, &["not", "valid"]) && !has(a, &["using", "index"]) {
                                rules.push(CONSTRAINT);
                            }
                        } else if starts(a, &["add"]) {
                            if (has(a, &["not", "null"]) || has(a, &["default"]))
                                && !literal_default(a)
                                && !fresh.contains(&table)
                            {
                                rules.push(ADD_NOT_NULL);
                            }
                            if !fresh.contains(&table)
                                && (has(a, &["stored"])
                                    || has(a, &["identity"])
                                    || has(a, &["serial"])
                                    || has(a, &["bigserial"])
                                    || has(a, &["smallserial"]))
                            {
                                rules.push(ADD_NOT_NULL);
                            }
                            if !fresh.contains(&table)
                                && ["unique", "primary", "references", "check"]
                                    .iter()
                                    .any(|w| has(a, &[*w]))
                            {
                                rules.push(CONSTRAINT);
                            }
                        } else if starts(a, &["alter"])
                            && action.len() > 1
                            && (starts(&action[1..], &["type"])
                                || starts(&action[1..], &["set", "data", "type"]))
                        {
                            rules.push(COLUMN_TYPE);
                        } else if starts(a, &["drop"]) {
                            destructive |= !starts(a, &["drop", "constraint"]);
                        } else if starts(a, &["enable", "row", "level", "security"])
                            || starts(a, &["force", "row", "level", "security"])
                        {
                            if let Some(state) = tenant.get_mut(&table) {
                                state.1 |= starts(a, &["enable"]);
                                state.2 |= starts(a, &["force"]);
                            }
                        } else if starts(a, &["validate", "constraint"])
                            || (starts(a, &["alter"])
                                && action.len() > 1
                                && (starts(&action[1..], &["set", "default"])
                                    || starts(&action[1..], &["drop", "default"])
                                    || starts(&action[1..], &["drop", "not", "null"])))
                        {
                            // Supported schema-only constraint/default changes.
                        } else {
                            rules.push(UNSUPPORTED);
                        }
                    }
                } else {
                    rules.push(LEX);
                }
            } else {
                rules.push(UNSUPPORTED);
            }
        } else if starts(t, &["drop", "table"]) || word("truncate") {
            destructive = true;
        } else if [
            "begin",
            "start",
            "commit",
            "end",
            "rollback",
            "abort",
            "savepoint",
            "release",
            "prepare",
        ]
        .iter()
        .any(|w| word(w))
        {
            rules.push(TRANSACTION);
        } else if ![
            &["create", "schema"][..],
            &["create", "type"],
            &["create", "policy"],
            &["alter", "policy"],
            &["comment", "on"],
        ]
        .iter()
        .any(|p| starts(t, p))
        {
            rules.push(UNSUPPORTED);
        } else if starts(t, &["create", "schema"])
            && !object(skip(&t[2..], &["if", "not", "exists"]))
                .is_some_and(|(_, tail)| tail.is_empty())
        {
            // CREATE SCHEMA can otherwise embed unchecked CREATE TABLE/INDEX.
            rules.push(UNSUPPORTED);
        }
        if destructive {
            if let Some(note) = allow_destructive(s) {
                out.overrides.push(note);
            } else {
                rules.push(DROP);
            }
        }
        rules.sort_unstable();
        rules.dedup();
        for rule in rules {
            emit(&mut out, m, s.line, rule);
        }
        out.spans.push(s.span.clone());
    }
    for (line, enable, force) in tenant.into_values() {
        if !enable || !force {
            emit(&mut out, m, line, RLS);
        }
    }
    out.diagnostics.sort_by_key(|d| (d.line, d.rule_id));
    if m.legacy_lint_reviewed {
        // The exact source and complete findings have already been checked.
        out.diagnostics.clear();
    }
    out
}
