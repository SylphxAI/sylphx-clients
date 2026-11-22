//! Bounded, byte-preserving Android log snapshots over the exec API.

use super::{write, Run};
use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};

const MAX_LOG_BYTES: usize = 32 * 1024 * 1024;
// Even the base64 wire representation of a chunk is below the 1 MiB cap.
const CHUNK_BYTES: usize = 512 * 1024;
const SNAPSHOT_PREFIX: &str = "/data/local/tmp/sylphx-log.";
// Read one byte past the ceiling. pipefail also rejects a failed logcat, so a
// short but incomplete dump cannot be accepted and hashed as a full snapshot.
const SNAPSHOT: &str = r#"
set -e -o pipefail
file=$(mktemp /data/local/tmp/sylphx-log.XXXXXXXXXX)
trap 'rm -f "$file"' EXIT
logcat -d "$@" | head -c "$(( $0 + 1 ))" > "$file"
size=$(wc -c < "$file")
digest=$(sha256sum < "$file")
printf '%s %s %s\n' "$file" "$size" "${digest%% *}"
trap - EXIT
"#;

impl Run<'_> {
    pub(super) async fn save_log(
        &self,
        lease: &str,
        file: &str,
        args: &[&str],
    ) -> Result<Vec<u8>, String> {
        let limit = MAX_LOG_BYTES.to_string();
        let mut argv = vec!["sh", "-c", SNAPSHOT, limit.as_str()];
        argv.extend_from_slice(args);
        let metadata = self.log_exec(lease, &argv).await?;
        let metadata = std::str::from_utf8(&metadata).map_err(|_| "invalid snapshot metadata")?;
        let mut fields = metadata.split_ascii_whitespace();
        let path = fields
            .next()
            .filter(|path| {
                path.strip_prefix(SNAPSHOT_PREFIX).is_some_and(|suffix| {
                    suffix.len() == 10 && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
                })
            })
            .ok_or("invalid snapshot path")?;
        let received = self.read_log(lease, path, fields).await;
        if let Err(error) = self.log_exec(lease, &["rm", "-f", path]).await {
            eprintln!("warning: could not remove Android log snapshot: {error}");
        }
        let bytes = received?;
        write(&self.o.out.join(file), &bytes)?;
        Ok(bytes)
    }

    async fn read_log(
        &self,
        lease: &str,
        path: &str,
        mut fields: std::str::SplitAsciiWhitespace<'_>,
    ) -> Result<Vec<u8>, String> {
        let size = fields
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or("invalid snapshot byte length")?;
        if size > MAX_LOG_BYTES {
            return Err("log snapshot exceeds the 32 MiB limit".into());
        }
        let digest = fields
            .next()
            .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or("invalid snapshot SHA-256")?;
        if fields.next().is_some() {
            return Err("unexpected snapshot metadata".into());
        }
        let mut bytes = Vec::with_capacity(size);
        let input = format!("if={path}");
        let block = format!("bs={CHUNK_BYTES}");
        for offset in (0..size).step_by(CHUNK_BYTES) {
            let skip = format!("skip={}", offset / CHUNK_BYTES);
            let chunk = self
                .log_exec(lease, &["dd", &input, &block, &skip, "count=1"])
                .await?;
            if chunk.len() != CHUNK_BYTES.min(size - offset) {
                return Err(format!("incorrect log chunk length at byte {offset}"));
            }
            bytes.extend_from_slice(&chunk);
        }
        if format!("{:x}", Sha256::digest(&bytes)) != digest {
            return Err("log snapshot SHA-256 mismatch".into());
        }
        Ok(bytes)
    }

    async fn log_exec(&self, lease: &str, argv: &[&str]) -> Result<Vec<u8>, String> {
        let response = self.exec_wire(lease, argv).await?;
        if response.truncated {
            return Err(format!(
                "the API truncated the log snapshot {} response",
                argv[0]
            ));
        }
        if response.exit_code != 0 || response.timed_out || response.cancelled {
            return Err(format!(
                "log snapshot {} did not succeed (exit {})",
                argv[0], response.exit_code
            ));
        }
        STANDARD
            .decode(response.stdout)
            .map_err(|_| "invalid log snapshot encoding".into())
    }
}
