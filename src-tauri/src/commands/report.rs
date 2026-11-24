use base64::{Engine as _, engine::general_purpose::STANDARD};
use regex::Regex;
use serde::Serialize;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;
use sysinfo::System;

/// Keep the newest part of the log so the base64 payload stays well under the backend's 1 MB cap.
const MAX_LOG_BYTES: usize = 400_000;
const REPORT_TIMEOUT: Duration = Duration::from_secs(30);
const REPORT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize)]
struct ReportPayload<'a> {
    title: &'a str,
    description: &'a str,
    mm_version: &'a str,
    os: String,
    arch: String,
    cpu: String,
    gpu: String,
    ram: String,
    log_b64: String,
    log_filename: String,
}

static UNIX_ACCOUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(/users/|/home/)[^/\\"\s<]+"#).expect("static regex"));
/// `C:\Users\x`, `C:/Users/x` and the doubled form `C:\\Users\\x` that `{:?}` formatting of a path
/// puts in the log, in any case.
static WINDOWS_ACCOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b([a-z]:(?:\\\\|\\|/)users(?:\\\\|\\|/))[^\\/"\r\n<]+"#)
        .expect("static regex")
});
static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}")
        .expect("static regex")
});
static IPV4: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").expect("static regex"));

/// Remove personal data from a log before it leaves the machine: the home directory becomes `~`,
/// account names in user-profile paths become `<user>`, e-mail addresses `<email>`, IPv4
/// addresses `<ip>`. Idempotent. Deliberately errs on the side of over-redaction (a mod id whose
/// name contains a dot, or a four-part version number, is also replaced).
pub fn redact(text: &str, home: Option<&Path>) -> String {
    let mut out = text.to_string();
    if let Some(home) = home {
        let h = home.to_string_lossy().to_string();
        if h.len() > 3 {
            // The home path as written, with the other separator, and as it appears when a path is
            // Debug-formatted (backslashes doubled). Case-insensitive, and only at a path boundary
            // so `/home/al` does not eat the start of `/home/alice`.
            let mut variants = vec![h.clone(), h.replace('\\', "/"), h.replace('/', "\\")];
            variants.push(variants[2].replace('\\', "\\\\"));
            variants.sort();
            variants.dedup();
            let alt: Vec<String> = variants.iter().map(|v| regex::escape(v)).collect();
            if let Ok(re) = Regex::new(&format!(r#"(?i)(?:{})(?P<next>[/\\"\s]|$)"#, alt.join("|")))
            {
                out = re.replace_all(&out, "~${next}").into_owned();
            }
        }
    }
    let out = UNIX_ACCOUNT.replace_all(&out, "${1}<user>");
    let out = WINDOWS_ACCOUNT.replace_all(&out, "${1}<user>");
    let out = EMAIL.replace_all(&out, "<email>");
    IPV4.replace_all(&out, "<ip>").into_owned()
}

/// Redact first, then keep the newest `max_bytes`, so a secret straddling the cut is never
/// half-visible.
pub fn prepare_log(raw: &str, home: Option<&Path>, max_bytes: usize) -> String {
    tail_str(&redact(raw, home), max_bytes).to_string()
}

struct Collected {
    os: String,
    arch: String,
    cpu: String,
    gpu: String,
    ram: String,
    log_filename: String,
    log: String,
}

fn collect() -> Collected {
    let (cpu, ram) = get_cpu_and_ram();
    let os = std::env::consts::OS.to_string();
    let os_version = detect_os_version().unwrap_or_else(|| "Unknown".to_string());
    let os = if os_version == "Unknown" {
        os
    } else {
        format!("{os} | {os_version}")
    };
    let (log_filename, raw) =
        latest_log().unwrap_or_else(|| ("bmm.log".to_string(), String::from("No logs found.")));
    Collected {
        os,
        arch: std::env::consts::ARCH.to_string(),
        cpu,
        gpu: detect_gpu().unwrap_or_else(|| "Unknown".to_string()),
        ram,
        log_filename,
        log: prepare_log(&raw, dirs::home_dir().as_deref(), MAX_LOG_BYTES),
    }
}

#[derive(Serialize)]
pub struct ReportPreview {
    pub os: String,
    pub arch: String,
    pub cpu: String,
    pub gpu: String,
    pub ram: String,
    pub log_filename: String,
    pub log: String,
    pub log_bytes: usize,
}

/// Exactly what `submit_report` will send (apart from the title, description and app version the
/// user typed), so the UI can show it before the user consents.
#[tauri::command]
pub async fn preview_report() -> Result<ReportPreview, String> {
    let c = collect();
    Ok(ReportPreview {
        log_bytes: c.log.len(),
        os: c.os,
        arch: c.arch,
        cpu: c.cpu,
        gpu: c.gpu,
        ram: c.ram,
        log_filename: c.log_filename,
        log: c.log,
    })
}

/// Submit an issue report to the BMI backend (`POST {BMI_SERVER_URL}/report`).
#[tauri::command]
pub async fn submit_report(
    title: String,
    description: String,
    mm_version: String,
) -> Result<(), String> {
    let c = collect();
    let log_b64 = STANDARD.encode(c.log.as_bytes());

    let payload = ReportPayload {
        title: &title,
        description: &description,
        mm_version: &mm_version,
        os: c.os,
        arch: c.arch,
        cpu: c.cpu,
        gpu: c.gpu,
        ram: c.ram,
        log_b64,
        log_filename: c.log_filename,
    };

    // Send with timeout to avoid hanging indefinitely
    let client = reqwest::Client::builder()
        .timeout(REPORT_TIMEOUT)
        .connect_timeout(REPORT_CONNECT_TIMEOUT)
        .build()
        .map_err(|e| {
            log::error!("Failed to build HTTP client for report: {}", e);
            format!("Failed to initialize report client: {e}")
        })?;

    log::info!("Submitting issue report: {}", title);
    let report_url = crate::bmi::bmi_server_url()?
        .join("report")
        .map_err(|e| format!("Invalid report URL: {e}"))?;
    let resp = client
        .post(report_url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            log::error!("Report request failed: {}", e);
            if e.is_timeout() {
                "Report request timed out. Please check your internet connection and try again."
                    .to_string()
            } else if e.is_connect() {
                "Could not connect to report server. Please check your internet connection."
                    .to_string()
            } else {
                format!("Report request failed: {e}")
            }
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let err_msg = format!(
            "Report server error: HTTP {}{}",
            status,
            if text.is_empty() {
                String::new()
            } else {
                format!(" - {}", text)
            }
        );
        log::error!("{}", err_msg);
        return Err(err_msg);
    }
    log::info!("Issue report submitted successfully");
    Ok(())
}

/// Last `max` bytes of `text`, cut on a char boundary.
fn tail_str(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Returns the latest log's filename and text, if any.
#[tauri::command]
pub async fn get_latest_log() -> Result<(String, String), String> {
    latest_log().ok_or_else(|| "No logs found.".to_string())
}

#[tauri::command]
pub async fn get_logs_folder() -> Result<String, String> {
    let dir = bmm_lib::config_root()
        .ok_or_else(|| "config directory not found".to_string())?
        .join("Balatro")
        .join("logs");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.to_string_lossy().to_string())
}

fn latest_log() -> Option<(String, String)> {
    let dir = bmm_lib::config_root()?.join("Balatro").join("logs");
    let entries = fs::read_dir(&dir).ok()?;
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let p = e.path();
            p.extension().is_some_and(|ext| ext == "log")
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("bmm_"))
        })
        .collect();

    files.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    let path = files.last()?.path();
    let filename = path.file_name()?.to_string_lossy().to_string();
    let text = fs::read_to_string(&path).ok()?;
    Some((filename, text))
}

fn get_cpu_and_ram() -> (String, String) {
    let mut sys = System::new_all();
    sys.refresh_all();
    let cpu = detect_cpu();

    // Try to produce a sane GB value despite sysinfo changes over versions
    let tm = sys.total_memory();
    let gib_from_bytes = (tm as f64) / (1024.0 * 1024.0 * 1024.0);
    let gib_from_kib = (tm as f64) / (1024.0 * 1024.0);

    let gb = if (1.0..=1024.0).contains(&gib_from_bytes) {
        gib_from_bytes
    } else if (1.0..=1024.0).contains(&gib_from_kib) {
        gib_from_kib
    } else {
        // Fallback: prefer bytes path
        gib_from_bytes
    };
    let ram_str = format!("{}GB", (gb.round() as u64).max(1));
    (cpu, ram_str)
}

/// Returns a human-friendly OS version string, if available.
///
/// Examples:
/// - Windows: "Microsoft Windows 11 Pro 10.0.22631 (Build 3880)"
/// - macOS:   "macOS 14.5 (23F79)"
/// - Linux:   distro + kernel if available
// The Windows branches nest `if let` + `if`; clippy only sees them on a Windows host.
#[allow(clippy::collapsible_if)]
fn detect_os_version() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        // Prefer PowerShell CIM query (wmic deprecated on newer Windows)
        let ps = r#"
            $o = Get-CimInstance Win32_OperatingSystem; 
            Write-Output ($o.Caption + ' ' + $o.Version + ' (Build ' + $o.BuildNumber + ')')
        "#;
        if let Ok(out) = Command::new("powershell")
            .args(["-NoProfile", "-Command", ps])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }

        // Fallback: wmic
        if let Ok(out) = Command::new("wmic")
            .args(["os", "get", "Caption,Version,BuildNumber", "/value"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                let mut caption = String::new();
                let mut version = String::new();
                let mut build = String::new();
                for line in s.lines() {
                    let l = line.trim();
                    if let Some(v) = l.strip_prefix("Caption=") {
                        caption = v.trim().to_string();
                    } else if let Some(v) = l.strip_prefix("Version=") {
                        version = v.trim().to_string();
                    } else if let Some(v) = l.strip_prefix("BuildNumber=") {
                        build = v.trim().to_string();
                    }
                }
                if !caption.is_empty() && !version.is_empty() && !build.is_empty() {
                    return Some(format!("{} {} (Build {})", caption, version, build));
                }
            }
        }

        // Last resort: sysinfo long_os_version + kernel
        let mut sys = System::new_all();
        sys.refresh_all();
        let long = System::long_os_version().or_else(System::os_version);
        let kernel = System::kernel_version();
        if let Some(l) = long {
            if let Some(k) = kernel {
                return Some(format!("{} (Kernel {})", l, k));
            }
            return Some(l);
        }
        None
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::Command;

        let name = Command::new("sw_vers").arg("-productName").output().ok();
        let version = Command::new("sw_vers").arg("-productVersion").output().ok();
        let build = Command::new("sw_vers").arg("-buildVersion").output().ok();

        let clean = |o: Option<std::process::Output>| -> Option<String> {
            o.and_then(|out| {
                if out.status.success() {
                    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
                } else {
                    None
                }
            })
        };

        let name = clean(name).unwrap_or_else(|| "macOS".to_string());
        let version = clean(version).unwrap_or_default();
        let build = clean(build);
        if !version.is_empty() {
            return Some(match build {
                Some(b) if !b.is_empty() => format!("{} {} ({})", name, version, b),
                _ => format!("{} {}", name, version),
            });
        }

        // Fallback: sysinfo long_os_version
        let mut sys = System::new_all();
        sys.refresh_all();
        if let Some(l) = System::long_os_version().or_else(System::os_version) {
            return Some(l);
        }
        Some(name)
    }

    #[cfg(target_os = "linux")]
    {
        use std::fs;

        // Try /etc/os-release PRETTY_NAME
        if let Ok(s) = fs::read_to_string("/etc/os-release") {
            let mut pretty = None;
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
                    let v = v.trim_matches('"').to_string();
                    if !v.is_empty() {
                        pretty = Some(v);
                        break;
                    }
                }
            }
            if let Some(p) = pretty {
                return Some(p);
            }
        }

        // Fallback: sysinfo long_os_version + kernel
        let mut sys = System::new_all();
        sys.refresh_all();
        let long = System::long_os_version().or_else(System::os_version);
        let kernel = System::kernel_version();
        match (long, kernel) {
            (Some(l), Some(k)) => Some(format!("{} (Kernel {})", l, k)),
            (Some(l), None) => Some(l),
            (None, Some(k)) => Some(format!("Linux (Kernel {})", k)),
            _ => None,
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let mut sys = System::new_all();
        sys.refresh_all();
        System::long_os_version()
            .or_else(System::os_version)
            .or_else(|| Some(std::env::consts::OS.to_string()))
    }
}

// Best-effort GPU detection; returns None if not supported
#[cfg(target_os = "macos")]
fn detect_gpu() -> Option<String> {
    use std::process::Command;
    let out = Command::new("system_profiler")
        .arg("SPDisplaysDataType")
        .arg("-detailLevel")
        .arg("mini")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        let l = line.trim();
        if l.starts_with("Chipset Model:") || l.starts_with("Chipset:") {
            return Some(
                l.replace("Chipset Model:", "")
                    .replace("Chipset:", "")
                    .trim()
                    .to_string(),
            );
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn detect_gpu() -> Option<String> {
    // Using wmic is deprecated on newer Windows; keep it best-effort
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let out = Command::new("wmic")
        .args(["path", "win32_VideoController", "get", "name"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let mut lines = s.lines().skip(1).filter(|l| !l.trim().is_empty());
    lines.next().map(|l| l.trim().to_string())
}

#[cfg(target_os = "linux")]
fn detect_gpu() -> Option<String> {
    use std::process::Command;
    let out = Command::new("sh")
        .arg("-c")
        .arg("lspci | grep -Ei 'vga|3d|display' | head -n1")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Some(s.trim().to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn detect_gpu() -> Option<String> {
    None
}

// Platform-specific CPU detection (best effort)
// The Windows branches nest `if let` + `if`; clippy only sees them on a Windows host.
#[allow(clippy::collapsible_if)]
fn detect_cpu() -> String {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        if let Ok(out) = Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            && out.status.success()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        if let Ok(out) = Command::new("wmic")
            .args(["cpu", "get", "name"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout);
                if let Some(name) = s.lines().skip(1).find(|l| !l.trim().is_empty()) {
                    return name.trim().to_string();
                }
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        if let Ok(out) = Command::new("sh")
            .arg("-c")
            .arg("lscpu | grep 'Model name' | cut -d: -f2")
            .output()
            && out.status.success()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
        if let Ok(out) = Command::new("sh")
            .arg("-c")
            .arg("grep -m1 'model name' /proc/cpuinfo | cut -d: -f2")
            .output()
            && out.status.success()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "Unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_report_url_is_https() {
        let url = crate::bmi::bmi_server_url()
            .unwrap()
            .join("report")
            .unwrap();
        assert_eq!(url.path(), "/report");
        // BMI_SERVER_URL may legitimately be http://localhost during development.
        if std::env::var("BMI_SERVER_URL").is_err() {
            assert_eq!(url.scheme(), "https");
        }
    }

    #[test]
    fn test_tail_str_keeps_newest_and_respects_char_boundaries() {
        assert_eq!(tail_str("hello", 10), "hello");
        assert_eq!(tail_str("hello world", 5), "world");
        // "é" is 2 bytes; a cut in the middle must advance to the next boundary.
        assert_eq!(tail_str("aéb", 2), "b");
        assert!(
            STANDARD
                .encode(tail_str(&"x".repeat(2_000_000), MAX_LOG_BYTES))
                .len()
                < 1_000_000
        );
    }

    #[test]
    fn test_report_timeout_is_reasonable() {
        assert!(REPORT_TIMEOUT.as_secs() >= 10);
        assert!(REPORT_TIMEOUT.as_secs() <= 120);
    }

    #[test]
    fn test_report_connect_timeout_is_reasonable() {
        assert!(REPORT_CONNECT_TIMEOUT.as_secs() >= 5);
        assert!(REPORT_CONNECT_TIMEOUT.as_secs() <= 30);
    }

    #[test]
    fn test_report_connect_timeout_less_than_total() {
        assert!(REPORT_CONNECT_TIMEOUT < REPORT_TIMEOUT);
    }

    #[test]
    fn test_report_payload_serialize() {
        let payload = ReportPayload {
            title: "Test Bug",
            description: "A test description",
            mm_version: "1.0.0",
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            cpu: "Test CPU".to_string(),
            gpu: "Test GPU".to_string(),
            ram: "16GB".to_string(),
            log_b64: "dGVzdA==".to_string(),
            log_filename: "bmm_test.log".to_string(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("Test Bug"));
        assert!(json.contains("mm_version"));
        assert!(json.contains("log_b64"));
    }

    #[test]
    fn test_report_payload_all_fields_present() {
        let payload = ReportPayload {
            title: "t",
            description: "d",
            mm_version: "v",
            os: "o".to_string(),
            arch: "a".to_string(),
            cpu: "c".to_string(),
            gpu: "g".to_string(),
            ram: "r".to_string(),
            log_b64: "l".to_string(),
            log_filename: "f".to_string(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        // All expected fields should be present
        for field in [
            "title",
            "description",
            "mm_version",
            "os",
            "arch",
            "cpu",
            "gpu",
            "ram",
            "log_b64",
            "log_filename",
        ] {
            assert!(json.contains(field), "Missing field: {}", field);
        }
    }

    #[test]
    fn test_get_cpu_and_ram_returns_valid_format() {
        let (cpu, ram) = get_cpu_and_ram();
        // CPU should be non-empty (might be "Unknown")
        assert!(!cpu.is_empty());
        // RAM should end with "GB"
        assert!(
            ram.ends_with("GB"),
            "RAM format should end with GB: {}",
            ram
        );
    }

    #[test]
    fn test_detect_cpu_returns_string() {
        let cpu = detect_cpu();
        assert!(!cpu.is_empty());
    }

    #[test]
    fn test_detect_os_version_returns_option() {
        // Just verify it doesn't panic
        let _version = detect_os_version();
    }

    #[test]
    fn test_detect_gpu_returns_option() {
        // Just verify it doesn't panic
        let _gpu = detect_gpu();
    }
}

#[cfg(test)]
mod redact_tests {
    use super::*;

    fn r(t: &str) -> String {
        redact(t, None)
    }

    #[test]
    fn account_names_in_profile_paths_are_replaced() {
        assert_eq!(
            r("/Users/kh/Library/Application Support/Balatro/mods"),
            "/Users/<user>/Library/Application Support/Balatro/mods"
        );
        assert_eq!(
            r(r"C:\Users\Jane Doe\AppData\Roaming\Balatro"),
            r"C:\Users\<user>\AppData\Roaming\Balatro"
        );
        assert_eq!(
            r("/home/jane/.config/Balatro"),
            "/home/<user>/.config/Balatro"
        );
    }

    #[test]
    fn debug_formatted_windows_paths_are_redacted() {
        // `{:?}` of a PathBuf doubles the backslashes; this is how the app logs most paths.
        let line = r#"Scanning for manual mods in: "C:\\Users\\Jane Doe\\AppData\\Roaming\\Balatro\\Mods""#;
        for home in [None, Some(Path::new(r"C:\Users\Jane Doe"))] {
            let out = redact(line, home);
            assert!(
                !out.contains("Jane") && !out.contains("Doe"),
                "{home:?}: {out}"
            );
        }
    }

    #[test]
    fn other_windows_spellings_and_case_are_redacted() {
        for t in [
            "c:/users/jane doe/x",
            "C:/Users/Jane Doe/x",
            r"c:\USERS\Jane Doe\x",
            "/USERS/jane/x",
            "/Home/jane/x",
        ] {
            let out = redact(t, None);
            assert!(!out.to_lowercase().contains("jane"), "{t} -> {out}");
        }
    }

    #[test]
    fn home_replacement_respects_path_boundaries() {
        assert_eq!(
            redact("/home/alice/x", Some(Path::new("/home/al"))),
            "/home/<user>/x"
        );
        assert_eq!(redact("/home/al/x", Some(Path::new("/home/al"))), "~/x");
        assert_eq!(
            redact(r"C:\\Users\\Jo\\x", Some(Path::new(r"C:\Users\Jo"))),
            r"~\\x"
        );
    }

    #[test]
    fn home_directory_becomes_tilde() {
        assert_eq!(
            redact("opened /Users/kh/x", Some(Path::new("/Users/kh"))),
            "opened ~/x"
        );
        assert_eq!(
            redact(r"opened C:\Users\kh\x", Some(Path::new(r"C:\Users\kh"))),
            r"opened ~\x"
        );
    }

    #[test]
    fn emails_and_ips_are_replaced() {
        assert_eq!(
            r("contact a.b+c@example.com from 192.168.1.20 and 8.8.8.8"),
            "contact <email> from <ip> and <ip>"
        );
        assert_eq!(r("version 1.2.3.4"), "version <ip>"); // accepted over-redaction
    }

    #[test]
    fn versions_dates_and_ordinary_lines_are_untouched() {
        for t in [
            "0.4.1",
            "2026-11-15",
            "Mod details fetch: title='Foo' id='a@Foo'",
        ] {
            assert_eq!(r(t), t);
        }
        // accepted over-redaction: a mod name containing a dot looks like an e-mail address
        assert_eq!(r("id='x@Mod.Pack'"), "id='<email>'");
    }

    #[test]
    fn redaction_is_idempotent() {
        let t = "/Users/kh/x jane@example.com 10.0.0.1 C:\\Users\\Jo\\y";
        assert_eq!(r(&r(t)), r(t));
    }

    #[test]
    fn username_straddling_the_truncation_boundary_never_survives() {
        let line = "/Users/secretname/Library/x\n";
        let mut raw = "x".repeat(1000);
        raw.push_str(line);
        raw.push_str(&"y".repeat(500));
        for cut in (0..line.len() + 40).step_by(1) {
            let max = 500 + line.len() - cut.min(line.len());
            let out = prepare_log(&raw, None, max.max(1));
            assert!(!out.contains("secretname"), "cut {cut}: {out:?}");
            assert!(!out.contains("secret"), "cut {cut}");
        }
        let big = format!("{}{line}{}", "a".repeat(250_000), "b".repeat(250_000));
        assert!(!prepare_log(&big, None, MAX_LOG_BYTES).contains("secretname"));
    }

    #[test]
    fn tail_str_cuts_on_char_boundaries() {
        assert_eq!(tail_str("héllo", 3), "llo");
        assert_eq!(tail_str("abc", 10), "abc");
    }
}

#[cfg(test)]
mod registration_tests {
    /// The UI calls `preview_report`; a command that is not in `invoke_handler!` fails at runtime
    /// only, so pin the registration in a test.
    #[test]
    fn preview_report_is_registered_in_the_invoke_handler() {
        let lib = include_str!("../lib.rs");
        assert!(lib.contains("commands::report::preview_report,"));
        assert!(lib.contains("commands::report::submit_report,"));
    }
}
