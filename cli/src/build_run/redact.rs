//! Redact credentials from byte streams, including matches split across events.

#[derive(Default)]
pub struct Redactor {
    pending: Vec<u8>,
}

impl Redactor {
    /// Emit all bytes known not to be an unfinished credential. Only a suffix
    /// that is a prefix of a secret is retained, bounded by the longest secret.
    pub fn push(&mut self, mut bytes: &[u8], secrets: &[String], mut emit: impl FnMut(&[u8])) {
        if secrets.is_empty() {
            emit(bytes);
            return;
        }
        while !bytes.is_empty() {
            if self.pending.is_empty() {
                let consumed = drain(bytes, secrets, false, &mut emit);
                self.pending.extend_from_slice(&bytes[consumed..]);
                return;
            }
            let take = secrets
                .iter()
                .map(String::len)
                .max()
                .unwrap_or(0)
                .min(bytes.len());
            self.pending.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            let consumed = drain(&self.pending, secrets, false, &mut emit);
            self.pending.drain(..consumed);
        }
    }

    pub fn finish(&mut self, secrets: &[String], mut emit: impl FnMut(&[u8])) {
        drain(&self.pending, secrets, true, &mut emit);
        self.pending.clear();
    }
}

fn drain(bytes: &[u8], secrets: &[String], end: bool, emit: &mut impl FnMut(&[u8])) -> usize {
    let mut cursor = 0;
    let mut plain = 0;
    while cursor < bytes.len() {
        let suffix = &bytes[cursor..];
        // A longer candidate may straddle the next event, even when a
        // shorter secret is already complete. Wait before choosing it.
        if !end
            && secrets
                .iter()
                .any(|secret| secret.len() > suffix.len() && secret.as_bytes().starts_with(suffix))
        {
            break;
        }
        let matched = secrets
            .iter()
            .filter(|secret| !secret.is_empty() && suffix.starts_with(secret.as_bytes()))
            .map(String::len)
            .max();
        if let Some(length) = matched {
            if plain < cursor {
                emit(&bytes[plain..cursor]);
            }
            emit(b"[redacted]");
            cursor += length;
            plain = cursor;
        } else {
            cursor += 1;
        }
    }
    if plain < cursor {
        emit(&bytes[plain..cursor]);
    }
    cursor
}

/// Flush both channels on every exit, including a lost command stream.
pub struct Output<'a> {
    out: &'a super::Out,
    held: &'a super::Held,
    stdout: Redactor,
    stderr: Redactor,
}

impl<'a> Output<'a> {
    pub fn new(out: &'a super::Out, held: &'a super::Held) -> Self {
        Self {
            out,
            held,
            stdout: Redactor::default(),
            stderr: Redactor::default(),
        }
    }

    pub fn stdout(&mut self, bytes: &[u8]) {
        self.held.with(|h| {
            self.stdout
                .push(bytes, &h.output_secrets, |b| self.out.stdout(b))
        });
    }

    pub fn stderr(&mut self, bytes: &[u8]) {
        self.held.with(|h| {
            self.stderr
                .push(bytes, &h.output_secrets, |b| self.out.stderr(b))
        });
    }
}

impl Drop for Output<'_> {
    fn drop(&mut self) {
        self.held.with(|h| {
            self.stdout
                .finish(&h.output_secrets, |b| self.out.stdout(b));
            self.stderr
                .finish(&h.output_secrets, |b| self.out.stderr(b));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_tokens_headers_and_binary_streams_at_every_chunk_boundary() {
        let secrets = vec![
            "source-token".into(),
            "Authorization: Bearer source-token".into(),
        ];
        let bytes = b"\xffbefore Authorization: Bearer source-token source-token after\x00";
        for split in 0..=bytes.len() {
            let mut redactor = Redactor::default();
            let mut output = Vec::new();
            redactor.push(&bytes[..split], &secrets, |chunk| {
                output.extend_from_slice(chunk)
            });
            redactor.push(&bytes[split..], &secrets, |chunk| {
                output.extend_from_slice(chunk)
            });
            redactor.finish(&secrets, |chunk| output.extend_from_slice(chunk));
            assert_eq!(
                output, b"\xffbefore [redacted] [redacted] after\x00",
                "split {split}"
            );
        }
    }

    #[test]
    fn keeps_ordinary_logs_and_flushes_nonmatching_secret_prefix_at_end() {
        let mut redactor = Redactor::default();
        let mut output = Vec::new();
        redactor.push(
            b"fetch https://github.com/acme/repo secret-pre",
            &["secret-prefix".into()],
            |chunk| output.extend_from_slice(chunk),
        );
        assert_eq!(output, b"fetch https://github.com/acme/repo ");
        redactor.finish(&["secret-prefix".into()], |chunk| {
            output.extend_from_slice(chunk)
        });
        assert_eq!(output, b"fetch https://github.com/acme/repo secret-pre");
    }
}
