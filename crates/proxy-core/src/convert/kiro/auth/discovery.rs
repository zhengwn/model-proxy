//! Auto-discovery of Kiro credentials from IDE logs, kiro-cli SQLite, and AWS SSO cache.

/// Try to auto-discover Profile ARN from Kiro IDE log files.
/// Scans ~/Library/Application Support/Kiro/logs/ (macOS) or equivalent.
pub fn discover_profile_arn() -> Option<String> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()?;

    // macOS Kiro log path
    #[cfg(target_os = "macos")]
    let log_dir = format!("{}/Library/Application Support/Kiro/logs", home);
    #[cfg(target_os = "linux")]
    let log_dir = format!("{}/.config/kiro/logs", home);
    #[cfg(target_os = "windows")]
    let log_dir = format!("{}\\AppData\\Roaming\\Kiro\\logs", home);

    let entries = std::fs::read_dir(&log_dir).ok()?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "log").unwrap_or(false) {
            if let Ok(content) = std::fs::read_to_string(&path) {
                // Look for profileArn in log content
                for line in content.lines() {
                    if line.contains("profileArn") || line.contains("profile_arn") {
                        // Extract ARN pattern
                        if let Some(start) = line.find("arn:aws:") {
                            let arn: String = line[start..]
                                .chars()
                                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
                                .collect();
                            if arn.starts_with("arn:aws:codewhisperer:") {
                                return Some(arn);
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// Try to load credentials from kiro-cli's SQLite database.
/// Returns (refresh_token, region) if found.
pub fn load_from_sqlite() -> Option<(String, String)> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok()?;

    // Platform-specific SQLite path
    #[cfg(target_os = "macos")]
    let db_path = format!("{}/.local/share/kiro-cli/data.sqlite3", home);
    #[cfg(target_os = "linux")]
    let db_path = format!("{}/.local/share/kiro-cli/data.sqlite3", home);
    #[cfg(target_os = "windows")]
    let db_path = format!("{}\\AppData\\Local\\kiro-cli\\data.sqlite3", home);

    // Try to read the SQLite file
    let data = std::fs::read(&db_path).ok()?;

    // Simple SQLite table scan for auth_kv entries
    // SQLite format: look for 'refreshToken' in the raw data
    let data_str = String::from_utf8_lossy(&data);

    // Look for social token key
    for key_prefix in &["kirocli:social:token", "kirocli:oidc:token", "codewhisperer:oidc:token"] {
        if let Some(pos) = data_str.find(key_prefix) {
            // Try to find a JSON object nearby containing refreshToken
            let nearby = &data_str[pos..std::cmp::min(pos + 2000, data_str.len())];
            if let Some(rt_start) = nearby.find("refreshToken") {
                let json_area = &nearby[rt_start..];
                // Simple extraction: find "refreshToken":"value"
                if let Some(q1) = json_area.find('"') {
                    let after_key = &json_area[q1 + 1..]; // skip first quote of key
                    if let Some(q2) = after_key.find('"') {
                        let after_colon = &after_key[q2 + 1..]; // skip closing quote of key
                        if let Some(q3) = after_colon.find('"') {
                            let value_start = q3 + 1;
                            if let Some(q4) = after_colon[value_start..].find('"') {
                                let token = &after_colon[value_start..value_start + q4];
                                if !token.is_empty() {
                                    tracing::info!(
                                        key = key_prefix,
                                        "从 kiro-cli SQLite 数据库加载 token"
                                    );
                                    return Some((token.to_string(), "us-east-1".to_string()));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

/// Scan ~/.aws/sso/cache/ for Kiro credential JSON files.
pub fn scan_sso_cache() -> Vec<(String, String)> {
    let home = match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(h) => h,
        Err(_) => return vec![],
    };

    let cache_dir = format!("{}/.aws/sso/cache", home);
    let entries = match std::fs::read_dir(&cache_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let mut results = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            if let Ok(content) = std::fs::read_to_string(&path) {
                // Look for refreshToken field
                if content.contains("refreshToken") {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(token) = json.get("refreshToken").and_then(|v| v.as_str()) {
                            let region = json.get("region")
                                .or_else(|| json.get("ssoRegion"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("us-east-1")
                                .to_string();
                            results.push((token.to_string(), region));
                            tracing::info!(
                                path = %path.display(),
                                "从 SSO 缓存发现 Kiro 凭证"
                            );
                        }
                    }
                }
            }
        }
    }

    results
}
