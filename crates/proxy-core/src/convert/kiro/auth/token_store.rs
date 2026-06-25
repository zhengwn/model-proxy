//! Persistence of refreshed Kiro tokens (JSON cache + write-back to source).

use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

use super::credential::{CredentialSource, KiroCredential};

// ---- Token persistence ----

/// Serializable token record for file persistence.
#[derive(Serialize, Deserialize)]
pub(super) struct TokenRecord {
    pub(super) access_token: String,
    pub(super) refresh_token: Option<String>,
    pub(super) expires_at: Option<String>,
    pub(super) profile_arn: Option<String>,
    pub(super) region: String,
}

/// Save token to a JSON file for persistence across restarts.
pub(super) fn persist_token(cred: &KiroCredential) {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    let path = format!(
        "{}/.config/model-proxy/kiro-token-{}.json",
        home,
        cred.region
    );

    let record = TokenRecord {
        access_token: cred.access_token.clone().unwrap_or_default(),
        refresh_token: cred.refresh_token.clone(),
        expires_at: cred.expires_at.map(|dt| dt.to_rfc3339()),
        profile_arn: cred.profile_arn.clone(),
        region: cred.region.clone(),
    };

    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
        restrict_dir_permissions(parent);
    }

    match serde_json::to_string_pretty(&record) {
        Ok(json) => {
            if let Err(e) = write_secret_file(&path, json.as_bytes()) {
                tracing::warn!(path = path.as_str(), error = %e, "保存 Kiro token 失败");
            } else {
                tracing::debug!(path = path.as_str(), "Kiro token 已保存");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "序列化 Kiro token 失败");
        }
    }
}

/// Write a file containing secrets with owner-only permissions (0600 on Unix).
///
/// On Unix the file is created with mode 0600 before any data is written, so the
/// secret is never briefly world-readable. On other platforms it falls back to a
/// regular write (NTFS ACLs already restrict to the user profile directory).
fn write_secret_file(path: &str, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // Ensure mode is 0600 even if the file already existed with looser bits.
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = file.set_permissions(perms);
        file.write_all(contents)
    }

    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

/// Restrict a directory holding secrets to owner-only access (0700 on Unix).
fn restrict_dir_permissions(dir: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

/// Write refreshed token back to the original credential source.
/// This ensures other clients (kiro-cli, kiro IDE) can use the updated token.
pub(super) fn write_back_to_source(cred: &KiroCredential, original_refresh_token: &str) {
    // Only write back if the refresh token actually changed
    let new_token = match &cred.refresh_token {
        Some(t) if t != original_refresh_token => t,
        _ => return,
    };

    match cred.source {
        CredentialSource::KiroCliSqlite => {
            write_back_to_sqlite(cred, new_token);
        }
        CredentialSource::AwsSsoCache => {
            // SSO cache tokens are managed by AWS SSO, don't write back
            tracing::debug!("SSO cache token refreshed, not writing back (managed by AWS SSO)");
        }
        CredentialSource::Config | CredentialSource::PersistedFile | CredentialSource::Unknown => {
            // Already handled by persist_token() which writes to model-proxy's cache
            tracing::debug!(
                source = ?cred.source,
                "Token persisted to model-proxy cache"
            );
        }
    }
}

/// Escape a string for safe embedding as a single-quoted SQLite string literal.
///
/// Within a SQLite single-quoted string literal the *only* metacharacter is the
/// single quote, which is escaped by doubling it (`''`). Backslashes and double
/// quotes have **no** special meaning in SQLite string literals and must NOT be
/// pre-escaped — doing so would corrupt the stored value. This is the same
/// transformation a prepared-statement text binding performs internally, so it
/// is injection-safe.
fn sqlite_quote_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// Try to write refreshed token back to kiro-cli SQLite database.
fn write_back_to_sqlite(cred: &KiroCredential, new_refresh_token: &str) {
    use std::io::Write;

    let home = match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(h) => h,
        Err(_) => return,
    };

    #[cfg(target_os = "macos")]
    let db_path = format!("{}/.local/share/kiro-cli/data.sqlite3", home);
    #[cfg(target_os = "linux")]
    let db_path = format!("{}/.local/share/kiro-cli/data.sqlite3", home);
    #[cfg(target_os = "windows")]
    let db_path = format!("{}\\AppData\\Local\\kiro-cli\\data.sqlite3", home);

    if !std::path::Path::new(&db_path).exists() {
        return;
    }

    // Defense in depth: treat the refresh token as untrusted (it comes from a
    // network response). A legitimate OAuth refresh token never contains NUL or
    // other control characters and is not pathologically long. Refuse to write
    // back anything that looks malformed, so a compromised upstream cannot use
    // the token to tamper with the user's local kiro-cli database.
    if new_refresh_token.is_empty()
        || new_refresh_token.len() > 8192
        || new_refresh_token.chars().any(|c| c.is_control())
    {
        tracing::warn!("refresh token 含非法字符或长度异常，跳过 SQLite 回写");
        return;
    }

    // Validate and prepare additional fields to write back atomically.
    let access_token_literal = cred.access_token.as_ref()
        .filter(|t| !t.is_empty() && t.len() <= 8192 && !t.chars().any(|c| c.is_control()))
        .map(|t| sqlite_quote_literal(t));

    let profile_arn_literal = cred.profile_arn.as_ref()
        .filter(|a| !a.is_empty() && a.len() <= 2048 && !a.chars().any(|c| c.is_control()))
        .map(|a| sqlite_quote_literal(a));

    // Build a chained json_set() SQL to update refreshToken + accessToken + profileArn atomically.
    let token_literal = sqlite_quote_literal(new_refresh_token);

    // SQLite json_set() supports multiple path-value pairs in a single call:
    //   json_set(value, '$.refreshToken', '<rt>', '$.accessToken', '<at>', ...)
    let mut paths = format!("'$.refreshToken', '{}'", token_literal);
    if let Some(ref at) = access_token_literal {
        paths.push_str(&format!(", '$.accessToken', '{}'", at));
    }
    if let Some(ref arn) = profile_arn_literal {
        paths.push_str(&format!(", '$.profileArn', '{}'", arn));
    }

    // Note: key_prefix values below are hardcoded constants (not user input),
    // so they are safe to embed directly.
    for key_prefix in &["kirocli:social:token", "kirocli:oidc:token"] {
        let sql = format!(
            "UPDATE auth_kv SET value = json_set(value, {}) WHERE key = '{}';",
            paths, key_prefix
        );

        // Feed the SQL (which contains the secret) via stdin rather than argv so
        // the refresh token does not appear in the process argument list, which
        // is readable by other local users (e.g. via `ps`).
        let mut child = match std::process::Command::new("sqlite3")
            .arg(&db_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "sqlite3 CLI 不可用，跳过 kiro-cli token 回写"
                );
                return;
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(sql.as_bytes()) {
                tracing::debug!(error = %e, "写入 sqlite3 stdin 失败");
                let _ = child.kill();
                let _ = child.wait();
                continue;
            }
            // stdin dropped here → EOF signalled to sqlite3
        }

        match child.wait_with_output() {
            Ok(output) if output.status.success() => {
                tracing::info!(
                    db_path = db_path.as_str(),
                    key = key_prefix,
                    "已将刷新后的 token 回写到 kiro-cli SQLite"
                );
                return;
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::debug!(
                    key = key_prefix,
                    stderr = %stderr,
                    "SQLite 回写失败 (尝试下一个 key)"
                );
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "等待 sqlite3 进程失败"
                );
            }
        }
    }
}

/// Try to load a persisted token from file.
pub(super) fn load_persisted_token(region: &str) -> Option<TokenRecord> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    let path = format!(
        "{}/.config/model-proxy/kiro-token-{}.json",
        home,
        region
    );
    let data = std::fs::read_to_string(&path).ok()?;
    let record: TokenRecord = serde_json::from_str(&data).ok()?;
    // Check if not expired
    if let Some(ref expires_str) = record.expires_at {
        if let Ok(expires) = chrono::DateTime::parse_from_rfc3339(expires_str) {
            if expires < Utc::now() + Duration::minutes(5) {
                return None; // Expired
            }
        }
    }
    Some(record)
}

#[cfg(test)]
mod tests {
    use super::sqlite_quote_literal;

    #[test]
    fn sqlite_quote_literal_doubles_single_quotes() {
        // Single quotes are the only metacharacter; they are doubled.
        assert_eq!(sqlite_quote_literal("abc"), "abc");
        assert_eq!(sqlite_quote_literal("a'b"), "a''b");
        assert_eq!(sqlite_quote_literal("''"), "''''");
    }

    #[test]
    fn sqlite_quote_literal_leaves_backslash_and_quotes_untouched() {
        // Backslashes and double quotes have no special meaning in SQLite
        // string literals and must NOT be escaped (escaping would corrupt them).
        assert_eq!(sqlite_quote_literal("a\\b"), "a\\b");
        assert_eq!(sqlite_quote_literal("a\"b"), "a\"b");
    }

    #[test]
    fn sqlite_quote_literal_neutralizes_injection_attempt() {
        // A malicious token attempting to break out of the string literal is
        // rendered inert: every single quote is doubled, so when embedded in
        // '{}' the value cannot terminate the literal early.
        let malicious = "x'); DROP TABLE auth_kv;--";
        let quoted = sqlite_quote_literal(malicious);
        assert_eq!(quoted, "x''); DROP TABLE auth_kv;--");

        // Security invariant: in the escaped output every single quote belongs
        // to a doubled pair, i.e. there is no lone quote that could close the
        // surrounding literal. Verify by confirming each run of consecutive
        // quotes has even length.
        let mut run = 0usize;
        for ch in quoted.chars() {
            if ch == '\'' {
                run += 1;
            } else {
                assert_eq!(run % 2, 0, "found an odd run of single quotes");
                run = 0;
            }
        }
        assert_eq!(run % 2, 0, "trailing odd run of single quotes");
    }
}
