//! Machine ID and system hardware fingerprint for Kiro credential identity.

// ---- Machine ID & System Fingerprint ----

/// Generate a per-credential machine ID using system fingerprint + token.
/// Priority: token hash + region > system hardware ID > random fallback.
pub(super) fn generate_machine_id(refresh_token: Option<&str>, region: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();

    // Primary: refresh token + region
    if let Some(token) = refresh_token {
        token.hash(&mut hasher);
        region.hash(&mut hasher);
    } else {
        // Fallback: system hardware ID
        let hw_id = get_system_fingerprint();
        hw_id.hash(&mut hasher);
    }

    format!("{:016x}", hasher.finish())
}

/// Get a system-level hardware fingerprint for machine identification.
/// macOS: IOPlatformUUID, Linux: /etc/machine-id, Windows: wmic csproduct UUID
fn get_system_fingerprint() -> String {
    #[cfg(target_os = "macos")]
    {
        // Try IOPlatformUUID
        if let Ok(output) = std::process::Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(line) = stdout.lines().find(|l| l.contains("IOPlatformUUID")) {
                if let Some(uuid) = line.split('"').nth(3) {
                    return uuid.to_string();
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(id) = std::fs::read_to_string("/etc/machine-id") {
            return id.trim().to_string();
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(output) = std::process::Command::new("wmic")
            .args(["csproduct", "get", "UUID"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(uuid) = stdout.lines().nth(1) {
                return uuid.trim().to_string();
            }
        }
    }

    // Fallback: username
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}
