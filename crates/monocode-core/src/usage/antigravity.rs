//! Antigravity (`agy` CLI) subscription usage probe via interactive `/usage`.
use super::{now_ms, FetchError, Snapshot, Window};
use crate::provider::Kind;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Overall time budget for one probe, from spawn to a parsed `/usage` screen.
#[cfg(unix)]
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);

#[cfg(unix)]
const READY_MARKER: &str = "for shortcuts";
/// Prompt-specific phrases of sign-in and trust screens. Bare words such as "trust" or
/// "authenticate" would also match banners of a signed-in session ("Authenticated as ...").
#[cfg(unix)]
const SETUP_PHRASES: [&str; 8] = [
    "do you trust",
    "trust this folder",
    "trust the files",
    "sign in to",
    "sign in with",
    "log in to",
    "log in with",
    "to authenticate",
];

pub(super) fn fetch() -> Result<Snapshot, FetchError> {
    let resolved = crate::provider::resolve(Kind::Antigravity);
    // An explicit override (settings or env) is honored as is, even when it is the legacy server.
    let overridden = crate::cli::configured(Kind::Antigravity).is_some()
        || std::env::var_os("MONOCODE_ANTIGRAVITY_BINARY").is_some_and(|v| !v.is_empty());
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&std::ffi::OsString::from(
        crate::scm::shell::gui_search_path(),
    ))
    .collect();
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        dirs.push(PathBuf::from(home).join(".local/bin"));
    }
    fetch_from(pick_binary(resolved, overridden, &dirs))
}

/// `resolve` prefers the legacy `agy_acp_server.par` over the `agy` alias. The legacy server
/// has no usage command, so when an automatic lookup lands on it, look for an installed `agy`
/// next to it and then in `search_dirs`. The legacy path is kept when none is found.
fn pick_binary(
    resolved: Option<PathBuf>,
    overridden: bool,
    search_dirs: &[PathBuf],
) -> Option<PathBuf> {
    let found = resolved?;
    if overridden || !is_legacy_server(&found) {
        return Some(found);
    }
    let sibling = found.parent().map(Path::to_path_buf);
    let agy = sibling
        .iter()
        .chain(search_dirs.iter())
        .map(|dir| dir.join("agy"))
        .find(|candidate| is_executable_file(candidate));
    Some(agy.unwrap_or(found))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn fetch_from(binary: Option<PathBuf>) -> Result<Snapshot, FetchError> {
    let Some(binary) = binary else {
        return Err(FetchError::unavailable(
            "Install the agy CLI to check Antigravity usage.",
        ));
    };
    if is_legacy_server(&binary) {
        return Err(FetchError::unavailable(
            "The legacy Antigravity ACP server has no usage command. Install the agy CLI to check usage.",
        ));
    }
    let raw = run_agy_usage(&binary)?;
    let clean = strip_ansi(&raw);
    parse_usage(&clean, now_ms())
        .ok_or_else(|| FetchError::failed("Antigravity did not report usage."))
}

/// Mirrors `provider::acp::is_legacy_antigravity_server`: the legacy server is a `.par` file.
fn is_legacy_server(binary: &Path) -> bool {
    binary
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("par"))
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum Screen {
    /// The prompt is up and accepts slash commands.
    Ready,
    /// A sign-in, trust or other setup screen is blocking the prompt.
    NeedsSetup,
    Waiting,
}

/// Classifies the ANSI-stripped terminal text printed so far, before `/usage` is sent.
/// `NeedsSetup` is only trustworthy once the output has gone quiet; see the read loop.
#[cfg(unix)]
fn classify_screen(clean: &str) -> Screen {
    if clean.contains(READY_MARKER) {
        return Screen::Ready;
    }
    let lower = clean.to_lowercase();
    if SETUP_PHRASES.iter().any(|phrase| lower.contains(phrase)) {
        return Screen::NeedsSetup;
    }
    Screen::Waiting
}

#[cfg(unix)]
fn needs_setup_error() -> FetchError {
    FetchError::unavailable(
        "Antigravity is not signed in or needs setup. Open agy in a terminal once.",
    )
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

/// `/usage` can list several model groups (for example Gemini and Claude), each with its own
/// Weekly and Five Hour sections. The tightest group is what blocks the user, so for each
/// window kind keep the occurrence with the lowest remaining percentage (with its own reset).
fn parse_window(text: &str, header: &str, minutes: u64, now: i64) -> Option<Window> {
    let mut best: Option<Window> = None;
    for (at, _) in text.match_indices(header) {
        let section_start = at + header.len();
        let rest = &text[section_start..];
        // A section ends where the next header of any kind begins.
        let end = rest.find("Limit Remaining").unwrap_or(rest.len());
        let Some(window) = parse_section(&rest[..end], minutes, now) else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|current| window.used_percent > current.used_percent)
        {
            best = Some(window);
        }
    }
    best
}

fn parse_section(section: &str, minutes: u64, now: i64) -> Option<Window> {
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
struct PtyChild {
    child: std::process::Child,
    pid: libc::pid_t,
}

#[cfg(unix)]
impl PtyChild {
    /// The child is a session leader (`setsid`), so its pid is the process group id. Stop the
    /// whole group so agy's helpers do not outlive the probe, then reap the child.
    fn terminate(&mut self) {
        use std::time::{Duration, Instant};
        unsafe { libc::kill(-self.pid, libc::SIGTERM) };
        let grace = Instant::now();
        while grace.elapsed() < Duration::from_millis(300) {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe { libc::kill(-self.pid, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
impl Drop for PtyChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Known, accepted race: another thread could fork between `openpty` and this call and inherit
/// the PTY fds until its own exec. The window is tiny and only leaks a PTY end to a child.
#[cfg(unix)]
fn set_cloexec(fd: libc::c_int) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Moves `fd` above the stdio range (as close-on-exec) so CLOEXEC cannot break fds 0-2 of the child.
#[cfg(unix)]
fn move_above_stdio(fd: libc::c_int) -> std::io::Result<libc::c_int> {
    if fd > 2 {
        return Ok(fd);
    }
    let moved = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if moved < 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe { libc::close(fd) };
    Ok(moved)
}

#[cfg(unix)]
fn run_agy_usage(binary: &Path) -> Result<String, FetchError> {
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
    // `libc::openpty` takes `*mut winsize` on macOS and `*const winsize` on Linux.
    #[allow(clippy::unnecessary_mut_passed)]
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
    let master = match move_above_stdio(master) {
        Ok(fd) => fd,
        Err(e) => {
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
            return Err(FetchError::failed(format!(
                "Failed to set up PTY for agy: {e}"
            )));
        }
    };
    let slave = match move_above_stdio(slave) {
        Ok(fd) => fd,
        Err(e) => {
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
            return Err(FetchError::failed(format!(
                "Failed to set up PTY for agy: {e}"
            )));
        }
    };
    // Owns the master from here on, so every early return closes it.
    let mut file = unsafe { std::fs::File::from_raw_fd(master) };
    // Keep concurrently spawned processes from inheriting the PTY. The `dup2` onto 0/1/2 in the
    // child clears the flag on the copies agy actually uses.
    if let Err(e) = set_cloexec(master).and_then(|()| set_cloexec(slave)) {
        unsafe { libc::close(slave) };
        return Err(FetchError::failed(format!(
            "Failed to set up PTY for agy: {e}"
        )));
    }

    let mut cmd = Command::new(binary);
    crate::scm::shell::apply_gui_env(&mut cmd);
    cmd.env("TERM", "xterm-256color");
    // A neutral directory, so agy does not register or ask to trust the app's own cwd.
    cmd.current_dir(
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir),
    );
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in 0..3 {
                if libc::dup2(slave, fd) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            // `master` and `slave` close on exec (FD_CLOEXEC).
            Ok(())
        });
    }

    let spawned = cmd.spawn();
    unsafe { libc::close(slave) };
    let child = match spawned {
        Ok(c) => c,
        Err(e) => {
            return Err(if e.kind() == std::io::ErrorKind::NotFound {
                FetchError::unavailable("Install the agy CLI to check Antigravity usage.")
            } else {
                FetchError::failed(format!("Failed to launch agy: {e}"))
            });
        }
    };
    let pid = child.id() as libc::pid_t;
    // Dropping this on any return kills the process group and reaps the child.
    let _guard = PtyChild { child, pid };

    // Set master non-blocking so we can poll with a deadline.
    unsafe {
        let flags = libc::fcntl(master, libc::F_GETFL);
        if flags < 0 || libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(FetchError::failed(format!(
                "Failed to set up PTY for agy: {}",
                std::io::Error::last_os_error()
            )));
        }
    }

    let start = Instant::now();
    let mut buf = Vec::new();
    let mut sent_usage = false;
    let mut saw_usage_at: Option<Instant> = None;
    let mut last_data = Instant::now();

    while start.elapsed() < DEADLINE {
        let mut chunk = [0u8; 4096];
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                last_data = Instant::now();
                let clean = strip_ansi(&String::from_utf8_lossy(&buf));
                if !sent_usage && classify_screen(&clean) == Screen::Ready {
                    let _ = file.write_all(b"/usage\r");
                    let _ = file.flush();
                    sent_usage = true;
                }
                if sent_usage
                    && saw_usage_at.is_none()
                    && clean.contains("Five Hour Limit Remaining")
                    && clean.contains("Refreshes in")
                {
                    saw_usage_at = Some(Instant::now());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // A setup screen waits for input. Banner text of a normal start-up may still
                // be followed by the ready footer, so only judge once the output is quiet.
                if !sent_usage
                    && last_data.elapsed() >= Duration::from_millis(500)
                    && classify_screen(&strip_ansi(&String::from_utf8_lossy(&buf)))
                        == Screen::NeedsSetup
                {
                    return Err(needs_setup_error());
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

    if !sent_usage {
        return Err(FetchError::failed(
            "Antigravity did not become ready in time.",
        ));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(not(unix))]
fn run_agy_usage(_binary: &Path) -> Result<String, FetchError> {
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

    const TWO_GROUPS: &str = "\
GEMINI MODELS
  Weekly Limit Remaining
[#####] 80.00%
Refreshes in 3d 1h

  Five Hour Limit Remaining
[#####] 20.00%
Refreshes in 1h 10m

CLAUDE MODELS
  Weekly Limit Remaining
[#####] 40.00%
Refreshes in 5d

  Five Hour Limit Remaining
[#####] 90.00%
Refreshes in 4h
";

    #[test]
    fn multiple_groups_keep_lowest_remaining_per_window() {
        let snap = parse_usage(TWO_GROUPS, 0).expect("snapshot");
        let five_h = &snap.windows[0];
        assert!((five_h.used_percent - 80.0).abs() < 0.01);
        assert_eq!(five_h.resets_at, Some((3_600 + 10 * 60) * 1_000));
        let weekly = &snap.windows[1];
        assert!((weekly.used_percent - 60.0).abs() < 0.01);
        assert_eq!(weekly.resets_at, Some(5 * 86_400 * 1_000));
    }

    #[test]
    fn missing_refresh_line_gives_no_reset() {
        let snap =
            parse_usage("Five Hour Limit Remaining\n[##] 50.00%\n", 1_000).expect("snapshot");
        assert_eq!(snap.windows[0].resets_at, None);
        assert!((snap.windows[0].used_percent - 50.0).abs() < 0.01);
    }

    #[cfg(unix)]
    #[test]
    fn classifies_screens() {
        assert_eq!(classify_screen("? for shortcuts"), Screen::Ready);
        assert_eq!(
            classify_screen("Please sign in to continue"),
            Screen::NeedsSetup
        );
        assert_eq!(
            classify_screen("Do you trust this folder?"),
            Screen::NeedsSetup
        );
        assert_eq!(
            classify_screen("Open the link to authenticate with Google"),
            Screen::NeedsSetup
        );
        assert_eq!(classify_screen("Starting..."), Screen::Waiting);
        assert_eq!(classify_screen(""), Screen::Waiting);
    }

    #[cfg(unix)]
    #[test]
    fn banner_words_are_not_a_setup_screen() {
        let banner = "Authenticated as user@example.com\nTip: use /login to switch accounts\nTrusted workspace";
        assert_eq!(classify_screen(banner), Screen::Waiting);
        let ready = format!("{banner}\n? for shortcuts");
        assert_eq!(classify_screen(&ready), Screen::Ready);
    }

    #[cfg(unix)]
    #[test]
    fn legacy_server_yields_to_installed_agy() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let par = dir.path().join("agy_acp_server.par");
        std::fs::write(&par, "").unwrap();
        // No agy anywhere: keep the legacy path.
        assert_eq!(
            pick_binary(Some(par.clone()), false, &[other.path().to_path_buf()]),
            Some(par.clone())
        );
        // A non-executable agy is ignored.
        let agy = other.path().join("agy");
        std::fs::write(&agy, "").unwrap();
        assert_eq!(
            pick_binary(Some(par.clone()), false, &[other.path().to_path_buf()]),
            Some(par.clone())
        );
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Found through the search dirs.
        assert_eq!(
            pick_binary(Some(par.clone()), false, &[other.path().to_path_buf()]),
            Some(agy.clone())
        );
        // A user override stays, even when it is the legacy server.
        assert_eq!(
            pick_binary(Some(par.clone()), true, &[other.path().to_path_buf()]),
            Some(par.clone())
        );
        // The sibling wins over the search dirs.
        let sibling = dir.path().join("agy");
        std::fs::write(&sibling, "").unwrap();
        std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            pick_binary(Some(par), false, &[other.path().to_path_buf()]),
            Some(sibling)
        );
        assert_eq!(pick_binary(None, false, &[]), None);
    }

    #[test]
    fn missing_or_legacy_binary_is_unavailable() {
        let missing = fetch_from(None).unwrap_err();
        assert!(missing.unavailable);
        assert!(missing.message.contains("Install the agy CLI"));
        let legacy = fetch_from(Some(PathBuf::from("/x/agy_acp_server.par"))).unwrap_err();
        assert!(legacy.unavailable);
        assert!(legacy.message.contains("legacy"));
    }

    #[test]
    fn fetch_with_dispatches_to_antigravity() {
        // Unique to Antigravity, so no other test reads or writes this variable.
        let key = "MONOCODE_ANTIGRAVITY_BINARY";
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, "/nonexistent/agy_acp_server.par") };
        let result = crate::usage::fetch_with(Kind::Antigravity, &Default::default(), None, false);
        unsafe {
            match previous {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        let error = result.unwrap_err();
        assert!(error.unavailable);
        assert!(error.message.contains("legacy"), "{}", error.message);
    }
}
