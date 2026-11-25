//! Antigravity (`agy` CLI) subscription usage probe via interactive `/usage`.
use super::{now_ms, FetchError, Snapshot, Window};
use regex::Regex;
use std::sync::OnceLock;

pub(super) fn fetch() -> Result<Snapshot, FetchError> {
    let raw = run_agy_usage()?;
    let clean = strip_ansi(&raw);
    parse_usage(&clean, now_ms())
        .ok_or_else(|| FetchError::failed("Antigravity did not report usage."))
}

fn strip_ansi(input: &str) -> String {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    let re = ANSI.get_or_init(|| {
        Regex::new(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]").unwrap()
    });
    re.replace_all(input, "").into_owned()
}

/// Parses `agy /usage` plain-text output into a `Snapshot`.
pub fn parse_usage(text: &str, now: i64) -> Option<Snapshot> {
    let mut windows = Vec::new();
    for (header, minutes) in [
        ("Five Hour Limit Remaining", 300),
        ("Weekly Limit Remaining", 10_080),
    ] {
        if let Some(window) = parse_window(text, header, minutes, now) {
            windows.push(window);
        }
    }
    if windows.is_empty() {
        return None;
    }
    Some(Snapshot {
        windows,
        updated_at: now,
        ..Default::default()
    })
}

fn parse_window(text: &str, header: &str, minutes: u64, now: i64) -> Option<Window> {
    let start = text.find(header)?;
    let section = &text[start + header.len()..];
    static PCT: OnceLock<Regex> = OnceLock::new();
    let pct_re = PCT.get_or_init(|| Regex::new(r"(\d+(?:\.\d+)?)%").unwrap());
    let remaining: f64 = pct_re
        .captures(section)?
        .get(1)?
        .as_str()
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite())?;

    static REFRESH: OnceLock<Regex> = OnceLock::new();
    let refresh_re = REFRESH.get_or_init(|| Regex::new(r"Refreshes in\s+([^\r\n]+)").unwrap());
    let resets_at = refresh_re
        .captures(section)
        .and_then(|c| parse_duration_ms(c.get(1)?.as_str()))
        .map(|ms| now.saturating_add(ms));

    Some(Window {
        used_percent: (100.0 - remaining).clamp(0.0, 100.0),
        minutes,
        resets_at,
    })
}

fn parse_duration_ms(s: &str) -> Option<i64> {
    static PART: OnceLock<Regex> = OnceLock::new();
    let re = PART.get_or_init(|| Regex::new(r"(\d+)\s*([dhms])").unwrap());
    let mut total_ms: i64 = 0;
    let mut matched = false;
    for cap in re.captures_iter(s) {
        let val: i64 = cap.get(1)?.as_str().parse().ok()?;
        let unit_ms = match cap.get(2)?.as_str() {
            "d" => 86_400_000,
            "h" => 3_600_000,
            "m" => 60_000,
            "s" => 1_000,
            _ => continue,
        };
        total_ms = total_ms.saturating_add(val.saturating_mul(unit_ms));
        matched = true;
    }
    matched.then_some(total_ms)
}

#[cfg(unix)]
fn run_agy_usage() -> Result<String, FetchError> {
    use std::io::{Read, Write};
    use std::os::unix::io::FromRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::time::{Duration, Instant};

    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut ws = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    } != 0
    {
        return Err(FetchError::failed("Failed to open PTY for agy."));
    }

    let mut cmd = Command::new("agy");
    unsafe {
        cmd.pre_exec(move || {
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            if master > 2 {
                libc::close(master);
            }
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
            return Err(if e.kind() == std::io::ErrorKind::NotFound {
                FetchError::unavailable("Install agy CLI to check Antigravity usage.")
            } else {
                FetchError::failed(format!("Failed to launch agy: {e}"))
            });
        }
    };
    unsafe { libc::close(slave) };

    // Set master non-blocking so we can poll with a deadline.
    unsafe {
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(master) };

    let start = Instant::now();
    let deadline = Duration::from_secs(10);
    let mut buf = Vec::new();
    let mut sent_usage = false;
    let mut saw_usage_at: Option<Instant> = None;

    while start.elapsed() < deadline {
        let mut chunk = [0u8; 4096];
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if !sent_usage
                    && (text.contains("for shortcuts") || start.elapsed() > Duration::from_secs(2))
                {
                    let _ = file.write_all(b"/usage\r");
                    let _ = file.flush();
                    sent_usage = true;
                }
                if sent_usage
                    && saw_usage_at.is_none()
                    && text.contains("Five Hour Limit Remaining")
                    && text.contains("Refreshes in")
                {
                    saw_usage_at = Some(Instant::now());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if !sent_usage && start.elapsed() > Duration::from_secs(2) {
                    let _ = file.write_all(b"/usage\r");
                    let _ = file.flush();
                    sent_usage = true;
                }
                if let Some(at) = saw_usage_at {
                    if at.elapsed() > Duration::from_millis(300) {
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(not(unix))]
fn run_agy_usage() -> Result<String, FetchError> {
    Err(FetchError::unavailable(
        "Antigravity usage is only supported on Unix.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agy_usage_output() {
        let sample = "\
└ Models & Quota

  Account: user@example.com

GEMINI MODELS
  Models within this group: Gemini 4 Hydrogen, Gemini 3.8 Flash

  Weekly Limit Remaining
[████████████████████████████████████████████████░░] 96.24%
Refreshes in 6d 16h 26m

  Five Hour Limit Remaining
[███████████████████████████████████████████████░░░] 94.03%
Refreshes in 2h 26m
";
        let now = 1_700_000_000_000;
        let snap = parse_usage(sample, now).expect("snapshot");
        assert_eq!(snap.windows.len(), 2);

        let five_h = &snap.windows[0];
        assert_eq!(five_h.minutes, 300);
        assert!((five_h.used_percent - 5.97).abs() < 0.01);
        assert_eq!(five_h.resets_at, Some(now + (2 * 3_600 + 26 * 60) * 1_000));

        let weekly = &snap.windows[1];
        assert_eq!(weekly.minutes, 10_080);
        assert!((weekly.used_percent - 3.76).abs() < 0.01);
        assert_eq!(
            weekly.resets_at,
            Some(now + (6 * 86_400 + 16 * 3_600 + 26 * 60) * 1_000)
        );
    }

    #[test]
    fn returns_none_when_no_windows_present() {
        assert!(parse_usage("Welcome to Antigravity CLI", 1_000).is_none());
    }

    #[test]
    fn strips_ansi_sequences() {
        let raw = "\x1b[32mFive Hour Limit Remaining\x1b[0m\n94.03%\nRefreshes in 1h 5m\n";
        let clean = strip_ansi(raw);
        let snap = parse_usage(&clean, 0).expect("snapshot");
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(snap.windows[0].resets_at, Some((3_600 + 5 * 60) * 1_000));
    }
}
