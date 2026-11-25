//! PostgreSQL lexical boundaries used by lint and execution. Words inside
//! comments, strings, quoted identifiers and dollar quotes aren't keywords.
use std::ops::Range;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Word,
    Ident,
    String,
    Number,
    Symbol,
}

#[derive(Debug)]
pub(super) struct Token<'a> {
    pub(super) kind: Kind,
    pub(super) text: &'a str,
}

impl Token<'_> {
    pub(super) fn is(&self, word: &str) -> bool {
        matches!(self.kind, Kind::Word | Kind::Symbol) && self.text.eq_ignore_ascii_case(word)
    }
    pub(super) fn ident(&self) -> Option<String> {
        match self.kind {
            Kind::Word => Some(self.text.to_ascii_lowercase()),
            Kind::Ident => Some(self.text[1..self.text.len() - 1].replace("\"\"", "\"")),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(super) struct Statement<'a> {
    pub(super) line: usize,
    pub(super) span: Range<usize>,
    pub(super) tokens: Vec<Token<'a>>,
    pub(super) comments: Vec<&'a str>,
}

fn word_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic() || !b.is_ascii()
}
fn word_part(b: u8) -> bool {
    word_start(b) || b.is_ascii_digit()
}

pub(super) fn scan(sql: &str) -> Result<Vec<Statement<'_>>, usize> {
    let bytes = sql.as_bytes();
    let (mut i, mut line) = (0, 1);
    let mut result = Vec::new();
    let mut statement = Statement {
        line,
        span: 0..0,
        tokens: Vec::new(),
        comments: Vec::new(),
    };
    while i < bytes.len() {
        let start = i;
        let b = bytes[i];
        i += 1;
        match b {
            b'\n' => {
                line += 1;
                continue;
            }
            b' ' | b'\t' | b'\r' | 0x0c => continue,
            b';' => {
                if !statement.tokens.is_empty() {
                    statement.span.end = start;
                    result.push(statement);
                }
                statement = Statement {
                    line,
                    span: i..i,
                    tokens: Vec::new(),
                    comments: Vec::new(),
                };
                continue;
            }
            b'-' if bytes.get(i) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                    i += 1;
                }
                if statement.tokens.is_empty() {
                    statement.comments.push(&sql[start + 2..i]);
                }
                continue;
            }
            b'/' if bytes.get(i) == Some(&b'*') => {
                i += 1;
                let mut depth = 1usize;
                while depth != 0 {
                    match bytes.get(i..i + 2) {
                        Some(b"/*") => {
                            depth += 1;
                            i += 2;
                        }
                        Some(b"*/") => {
                            depth -= 1;
                            i += 2;
                        }
                        Some(_) => i += 1,
                        None => return Err(line),
                    }
                }
                line += bytes[start..i].iter().filter(|&&b| b == b'\n').count();
                continue;
            }
            // Do not guess the identity of a Unicode-escaped identifier.
            b'U' | b'u' if bytes.get(i..i + 2) == Some(b"&\"") => return Err(line),
            0 => return Err(line),
            _ => {}
        }
        let escaped = matches!(b, b'e' | b'E') && bytes.get(i) == Some(&b'\'');
        // Like standard strings, Unicode strings use doubled quotes, not
        // a backslash to escape a quote. PostgreSQL decodes the contents.
        let unicode = matches!(b, b'u' | b'U') && bytes.get(i..i + 2) == Some(b"&'");
        let kind = if matches!(b, b'\'' | b'"') || escaped || unicode {
            let quote = if unicode {
                i += 2;
                b'\''
            } else if escaped {
                i += 1;
                b'\''
            } else {
                b
            };
            loop {
                let c = *bytes.get(i).ok_or(line)?;
                i += 1;
                if c == quote {
                    if bytes.get(i) != Some(&quote) {
                        break;
                    }
                    i += 1;
                } else if c == b'\\' && escaped {
                    bytes.get(i).ok_or(line)?;
                    i += 1;
                }
            }
            if quote == b'"' {
                Kind::Ident
            } else {
                Kind::String
            }
        } else if b == b'$' {
            let mut end = i;
            if bytes.get(end).is_some_and(|&b| word_start(b)) {
                while bytes.get(end).is_some_and(|&b| word_part(b)) {
                    end += 1;
                }
            }
            if bytes.get(end) != Some(&b'$') {
                return Err(line);
            }
            let tag = &sql[start..=end];
            i = end + 1 + sql[end + 1..].find(tag).ok_or(line)? + tag.len();
            Kind::String
        } else if word_start(b) {
            while bytes.get(i).is_some_and(|&b| word_part(b) || b == b'$') {
                i += 1;
            }
            Kind::Word
        } else if b.is_ascii_digit() {
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            Kind::Number
        } else {
            Kind::Symbol
        };
        if statement.tokens.is_empty() {
            statement.line = line;
            statement.span.start = start;
        }
        statement.tokens.push(Token {
            kind,
            text: &sql[start..i],
        });
        line += bytes[start..i].iter().filter(|&&b| b == b'\n').count();
    }
    if !statement.tokens.is_empty() {
        statement.span.end = sql.len();
        result.push(statement);
    }
    Ok(result)
}
