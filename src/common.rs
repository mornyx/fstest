use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn host_name() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string())
}

pub fn iso8601(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn suffixed(p: &Path, s: &str) -> PathBuf {
    let mut os = p.as_os_str().to_os_string();
    os.push(s);
    os.into()
}

// `File::sync_all()` issues `fcntl(F_FULLFSYNC)` on Apple platforms, which is orders of
// magnitude slower than plain POSIX `fsync(2)` used by upstream C/Python suites.
pub fn fsync_raw(f: &File) -> io::Result<()> {
    if unsafe { libc::fsync(f.as_raw_fd()) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Sync only the filesystem backing `dir` (via `syncfs(2)` on Linux, `fsync(dir_fd)` fallback)
/// instead of stalling unrelated disks on the host with global `libc::sync()`.
pub fn sync_filesystem(dir: &Path) {
    let Ok(cpath) = CString::new(dir.as_os_str().as_bytes()) else {
        return;
    };
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let _ = libc::syncfs(fd);
    }
    #[cfg(not(target_os = "linux"))]
    unsafe {
        let _ = libc::fsync(fd);
    }
    unsafe {
        libc::close(fd);
    }
}

/// POSIX `posix_fallocate(fd, offset, len)` semantics across Linux and macOS.
pub fn posix_fallocate_fd(fd: i32, offset: i64, len: i64) -> io::Result<()> {
    if offset < 0 || len <= 0 {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    #[cfg(target_os = "linux")]
    {
        let rc = unsafe { libc::posix_fallocate(fd, offset, len) };
        if rc != 0 {
            Err(io::Error::from_raw_os_error(rc))
        } else {
            Ok(())
        }
    }
    #[cfg(target_os = "macos")]
    {
        let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if fl < 0 {
            return Err(io::Error::last_os_error());
        }
        if (fl & libc::O_ACCMODE) == libc::O_RDONLY {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        let mut sb: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut sb) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let ftype = sb.st_mode & libc::S_IFMT;
        if ftype == libc::S_IFIFO {
            return Err(io::Error::from_raw_os_error(libc::ESPIPE));
        }
        if ftype != libc::S_IFREG {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }
        let end = offset
            .checked_add(len)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFBIG))?;
        let mut st = libc::fstore_t {
            fst_flags: libc::F_ALLOCATEALL,
            fst_posmode: libc::F_PEOFPOSMODE,
            fst_offset: 0,
            fst_length: end.saturating_sub(sb.st_size),
            fst_bytesalloc: 0,
        };
        if st.fst_length > 0 && unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut st) } == -1 {
            let err = io::Error::last_os_error();
            if !matches!(err.raw_os_error(), Some(libc::ENOTSUP | libc::EOPNOTSUPP | libc::EINVAL)) {
                return Err(err);
            }
        }
        if end > sb.st_size && unsafe { libc::ftruncate(fd, end) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (fd, offset, len);
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
    }
}

#[derive(Clone, Copy)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn fill_bytes(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

pub fn tail_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("...{}", &s[start..])
}

pub fn tail_chars(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    let kept: String = s.chars().skip(total - n).collect();
    format!("...{kept}")
}

/// Real user home directory from passwd database, unaffected when a test harness
/// overrides `$HOME` to a temporary sandbox directory.
pub fn passwd_home_dir() -> Option<PathBuf> {
    unsafe {
        let uid = libc::geteuid();
        let pw = libc::getpwuid(uid);
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        let cstr = std::ffi::CStr::from_ptr((*pw).pw_dir);
        let s = cstr.to_str().ok()?;
        if s.is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        }
    }
}

pub fn cache_dir_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push_unique = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Ok(d) = std::env::var("FSTEST_CACHE_DIR")
        && !d.trim().is_empty()
    {
        push_unique(PathBuf::from(d));
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.trim().is_empty()
    {
        push_unique(PathBuf::from(xdg).join("fstest"));
    }
    if let Ok(h) = std::env::var("HOME")
        && !h.trim().is_empty()
    {
        push_unique(PathBuf::from(h).join(".cache/fstest"));
    }
    if let Some(pw) = passwd_home_dir() {
        push_unique(pw.join(".cache/fstest"));
    }
    let uid = unsafe { libc::geteuid() };
    push_unique(std::env::temp_dir().join(format!("fstest-cache-{uid}")));
    out
}

pub fn primary_cache_dir() -> PathBuf {
    if let Ok(d) = std::env::var("FSTEST_CACHE_DIR")
        && !d.trim().is_empty()
    {
        return PathBuf::from(d);
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.trim().is_empty()
    {
        return PathBuf::from(xdg).join("fstest");
    }
    if let Some(pw) = passwd_home_dir() {
        return pw.join(".cache/fstest");
    }
    if let Ok(h) = std::env::var("HOME")
        && !h.trim().is_empty()
    {
        return PathBuf::from(h).join(".cache/fstest");
    }
    let uid = unsafe { libc::geteuid() };
    std::env::temp_dir().join(format!("fstest-cache-{uid}"))
}

pub struct ExecOutput {
    pub code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub combined: String,
    pub spawn_err: Option<String>,
}

/// Execute `cmd` in its own session/process group so a timeout kills the entire
/// descendant tree cleanly without hanging stdout/stderr pipes.
pub fn exec_with_timeout(mut cmd: Command, timeout: Duration) -> ExecOutput {
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return ExecOutput {
                code: None,
                timed_out: false,
                stdout: String::new(),
                stderr: String::new(),
                combined: String::new(),
                spawn_err: Some(e.to_string()),
            };
        }
    };
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let t_so = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let t_se = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        b
    });
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code(),
            Ok(None) => {
                if Instant::now() > deadline {
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break None,
        }
    };
    let so_bytes = t_so.join().unwrap_or_default();
    let se_bytes = t_se.join().unwrap_or_default();
    let stdout = String::from_utf8_lossy(&so_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&se_bytes).into_owned();
    let combined = if stderr.is_empty() {
        stdout.clone()
    } else if stdout.is_empty() {
        stderr.clone()
    } else {
        format!("{stdout}\n{stderr}")
    };
    ExecOutput {
        code,
        timed_out,
        stdout,
        stderr,
        combined,
        spawn_err: None,
    }
}

/// Bounded directory cleanup: never block report emission on a wedged FUSE mount.
pub fn remove_dir_all_bounded(path: &Path, bound: Duration) {
    if !path.exists() {
        return;
    }
    let owned = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("fstest-cleanup".into())
        .spawn(move || {
            let _ = fs::remove_dir_all(&owned);
            let _ = tx.send(());
        })
        .ok();
    if rx.recv_timeout(bound).is_err() {
        warn!(
            "cleanup of {} did not finish in {:?}; leaving it (mount may be wedged)",
            path.display(),
            bound
        );
    }
}

#[derive(Debug, Clone)]
pub struct MountSnapshot {
    pub path: PathBuf,
    pub dev: u64,
    pub fstype: String,
}

#[derive(Debug, Clone)]
pub struct MountCheck {
    pub healthy: bool,
    pub fstype: String,
    pub dev_before: u64,
    pub dev_after: Option<u64>,
    pub disconnect_reason: Option<String>,
}

pub fn detect_fstype(path: &Path) -> String {
    let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
        return "unknown".into();
    };
    #[cfg(target_os = "macos")]
    unsafe {
        let mut sfs: libc::statfs = std::mem::zeroed();
        if libc::statfs(cpath.as_ptr(), &mut sfs) == 0 {
            let name = std::ffi::CStr::from_ptr(sfs.f_fstypename.as_ptr());
            return name.to_string_lossy().into_owned();
        }
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let mut sfs: libc::statfs = std::mem::zeroed();
        if libc::statfs(cpath.as_ptr(), &mut sfs) == 0 {
            let t = sfs.f_type as i64;
            return match t {
                0x65735546 => "fuse".into(),
                0x65735543 => "fusectl".into(),
                0xef53 => "ext4".into(),
                0x58465342 => "xfs".into(),
                0x9123683e => "btrfs".into(),
                0x01021994 => "tmpfs".into(),
                0x6969 => "nfs".into(),
                other => format!("0x{other:x}"),
            };
        }
    }
    "unknown".into()
}

impl MountSnapshot {
    pub fn capture(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        Some(MountSnapshot {
            path: path.to_path_buf(),
            dev: meta.dev(),
            fstype: detect_fstype(path),
        })
    }

    pub fn verify_after(&self) -> MountCheck {
        let p = self.path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("fstest-mount-probe".into())
            .spawn(move || {
                let res = fs::metadata(&p).map(|m| (m.dev(), detect_fstype(&p)));
                let _ = tx.send(res);
            })
            .ok();
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok((dev_after, fstype_after))) => {
                if dev_after != self.dev {
                    MountCheck {
                        healthy: false,
                        fstype: fstype_after,
                        dev_before: self.dev,
                        dev_after: Some(dev_after),
                        disconnect_reason: Some(format!(
                            "mount device id changed during run ({} -> {}): FUSE daemon likely unmounted or crashed",
                            self.dev, dev_after
                        )),
                    }
                } else {
                    MountCheck {
                        healthy: true,
                        fstype: fstype_after,
                        dev_before: self.dev,
                        dev_after: Some(dev_after),
                        disconnect_reason: None,
                    }
                }
            }
            Ok(Err(e)) => {
                let raw = e.raw_os_error().unwrap_or(0);
                let reason = if raw == libc::ENOTCONN {
                    format!("ENOTCONN (Transport endpoint is not connected): FUSE daemon crashed ({e})")
                } else {
                    format!("mountpoint stat failed after run (errno={raw}): {e}")
                };
                MountCheck {
                    healthy: false,
                    fstype: self.fstype.clone(),
                    dev_before: self.dev,
                    dev_after: None,
                    disconnect_reason: Some(reason),
                }
            }
            Err(_) => MountCheck {
                healthy: false,
                fstype: self.fstype.clone(),
                dev_before: self.dev,
                dev_after: None,
                disconnect_reason: Some("mountpoint stat hung >5s after run (FUSE daemon wedged)".into()),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseEntry {
    pub id: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub detail: String,
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_sep = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_sep = false;
        } else if !prev_sep && !out.is_empty() {
            out.push('_');
            prev_sep = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

fn insert_metric(metrics: &mut Map<String, Value>, key: impl Into<String>, val: Option<f64>) {
    if let Some(v) = val
        && v.is_finite()
    {
        metrics.insert(key.into(), json!(v));
    }
}

pub fn enrich_report(report: &mut Value, snap: Option<&MountSnapshot>) {
    let mount_check = snap.map(|s| s.verify_after());
    let suite = report["suite"].as_str().unwrap_or("unknown").to_string();

    if report.get("cases").is_none() {
        let derived = derive_cases(&suite, report);
        report["cases"] = serde_json::to_value(&derived).unwrap_or_else(|_| json!([]));
    }

    if report.get("metrics").is_none() {
        let derived = derive_metrics(&suite, report);
        if !derived.is_empty() {
            report["metrics"] = Value::Object(derived);
        }
    }

    if let Some(mc) = mount_check {
        report["mount"] = json!({
            "healthy": mc.healthy,
            "fstype": mc.fstype,
            "devBefore": mc.dev_before,
            "devAfter": mc.dev_after,
            "disconnectReason": mc.disconnect_reason,
        });
        if !mc.healthy {
            let reason = mc
                .disconnect_reason
                .unwrap_or_else(|| "mountpoint unhealthy after run".into());
            report["status"] = json!("mount_disconnected");
            if let Some(arr) = report["cases"].as_array_mut() {
                arr.insert(
                    0,
                    json!({
                        "id": "mount-liveness",
                        "outcome": "blocked",
                        "detail": reason,
                    }),
                );
            }
        }
    }

    if report.get("schemaVersion").is_none() {
        report["schemaVersion"] = json!(1);
    }
}

fn derive_cases(suite: &str, v: &Value) -> Vec<CaseEntry> {
    let mut out = Vec::new();
    let ok = v["status"].as_str() == Some("ok");
    let mut failed_any = !ok;

    match suite {
        "pjdfstest" => {
            if let Some(files) = v["files"].as_array() {
                for f in files {
                    let name = f["name"].as_str().unwrap_or("?");
                    let id = name.replace('/', "_");
                    if f["skipped"].as_bool().unwrap_or(false) {
                        out.push(CaseEntry {
                            id,
                            outcome: "skip".into(),
                            detail: f["skipReason"].as_str().unwrap_or("skipped").to_string(),
                        });
                        continue;
                    }
                    let failed = f["failed"].as_i64().unwrap_or(0);
                    let tests = f["tests"].as_i64().unwrap_or(0);
                    let bail = f["bail"].as_bool().unwrap_or(false);
                    let timed_out = f["timedOut"].as_bool().unwrap_or(false);
                    if failed > 0 || bail || timed_out {
                        failed_any = true;
                        let first = f["failures"]
                            .as_array()
                            .and_then(|a| a.first())
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        out.push(CaseEntry {
                            id,
                            outcome: "fail".into(),
                            detail: format!(
                                "{failed}/{tests} failed{}{}: {}",
                                if bail { ", BAIL OUT" } else { "" },
                                if timed_out { ", TIMED OUT" } else { "" },
                                tail_chars(first, 200)
                            ),
                        });
                    }
                }
            }
        }
        "ltp" => {
            if let Some(tests) = v["results"]["tests"].as_array() {
                for t in tests {
                    let name = t["name"].as_str().unwrap_or("?");
                    let result = t["result"].as_str().unwrap_or("");
                    if result == "pass" || result == "warn" {
                        continue;
                    }
                    let (outcome, label) = match result {
                        "skip" => ("skip", "TCONF skip"),
                        r => ("fail", r),
                    };
                    if outcome == "fail" {
                        failed_any = true;
                    }
                    out.push(CaseEntry {
                        id: name.to_string(),
                        outcome: outcome.into(),
                        detail: format!(
                            "{label} exit={}{}",
                            t["exitCode"].as_i64().unwrap_or(-1),
                            if t["timedOut"].as_bool().unwrap_or(false) {
                                " TIMED OUT"
                            } else {
                                ""
                            }
                        ),
                    });
                }
            }
        }
        "git" => {
            if let Some(items) = v["results"]["items"].as_array() {
                for t in items {
                    let (Some(name), Some(status)) = (t["name"].as_str(), t["status"].as_str()) else {
                        continue;
                    };
                    if status == "ok" {
                        continue;
                    }
                    if status == "skip" || status == "skipped" {
                        out.push(CaseEntry {
                            id: name.to_string(),
                            outcome: "skip".into(),
                            detail: format!("skipAll: {}", t["skipAll"].as_str().unwrap_or("prereq")),
                        });
                        continue;
                    }
                    failed_any = true;
                    let detail = t["failures"]
                        .as_array()
                        .and_then(|a| a.first())
                        .map(|f| {
                            format!(
                                "{}: {}",
                                f["name"].as_str().unwrap_or("?"),
                                f["detail"].as_str().unwrap_or("")
                            )
                        })
                        .unwrap_or_else(|| tail_chars(t["tail"].as_str().unwrap_or(""), 200));
                    out.push(CaseEntry {
                        id: name.to_string(),
                        outcome: "fail".into(),
                        detail: format!("{status} {detail}"),
                    });
                }
            }
        }
        "sqlite" => {
            if let Some(items) = v["results"]["items"].as_array() {
                for t in items {
                    let (Some(name), Some(status)) = (t["name"].as_str(), t["status"].as_str()) else {
                        continue;
                    };
                    if status == "ok" {
                        continue;
                    }
                    if status == "skip" || status == "skipped" {
                        out.push(CaseEntry {
                            id: name.to_string(),
                            outcome: "skip".into(),
                            detail: t["reason"].as_str().unwrap_or("skipped").to_string(),
                        });
                        continue;
                    }
                    failed_any = true;
                    let detail = t["failures"]
                        .as_array()
                        .and_then(|a| a.first())
                        .map(|f| {
                            if let (Some(c), Some(m)) = (f["client"].as_str(), f["message"].as_str()) {
                                format!("{c}: {m}")
                            } else if let (Some(n), Some(d)) = (f["name"].as_str(), f["detail"].as_str()) {
                                format!("{n}: {d}")
                            } else {
                                f.to_string()
                            }
                        })
                        .or_else(|| t["reason"].as_str().filter(|s| !s.is_empty()).map(String::from))
                        .unwrap_or_else(|| tail_chars(t["tail"].as_str().unwrap_or(""), 200));
                    out.push(CaseEntry {
                        id: name.to_string(),
                        outcome: "fail".into(),
                        detail: format!("{status}: {detail}"),
                    });
                }
            }
        }
        "stdfs" => {
            let mut avail_count = 0usize;
            let mut total_langs = 0usize;
            if let Some(langs) = v["results"]["languages"].as_object() {
                for (lang, lv) in langs {
                    total_langs += 1;
                    let available = lv["available"].as_bool().unwrap_or(false);
                    if !available {
                        out.push(CaseEntry {
                            id: lang.clone(),
                            outcome: "skip".into(),
                            detail: lv["reason"]
                                .as_str()
                                .unwrap_or("toolchain unavailable")
                                .to_string(),
                        });
                        continue;
                    }
                    avail_count += 1;
                    if lv["status"].as_str() == Some("ok") {
                        continue;
                    }
                    failed_any = true;
                    let mut pushed_lang_item = false;
                    if let Some(items) = lv["items"].as_array() {
                        for it in items {
                            let ist = it["status"].as_str().unwrap_or("ok");
                            if ist == "ok" {
                                continue;
                            }
                            pushed_lang_item = true;
                            let iname = it["name"].as_str().unwrap_or("?");
                            let first_fail = it["failures"]
                                .as_array()
                                .and_then(|a| a.first())
                                .map(|f| {
                                    if let Some(s) = f.as_str() {
                                        s.to_string()
                                    } else {
                                        format!(
                                            "{}: {}",
                                            f["name"].as_str().unwrap_or("?"),
                                            tail_chars(f["tail"].as_str().unwrap_or(""), 160)
                                        )
                                    }
                                })
                                .unwrap_or_else(|| {
                                    let detail = it["reason"]
                                        .as_str()
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or_else(|| it["tail"].as_str().unwrap_or(""));
                                    tail_chars(detail, 200)
                                });
                            out.push(CaseEntry {
                                id: format!("{lang}-{}", iname.replace(['/', '.'], "_")),
                                outcome: "fail".into(),
                                detail: format!("{ist}: {first_fail}"),
                            });
                        }
                    }
                    if let Some(fails) = lv["failures"].as_array() {
                        for f in fails {
                            pushed_lang_item = true;
                            let fname = f["name"].as_str().unwrap_or("?");
                            let ftail = f["tail"].as_str().unwrap_or("");
                            out.push(CaseEntry {
                                id: format!("{lang}-{fname}"),
                                outcome: "fail".into(),
                                detail: tail_chars(ftail, 200),
                            });
                        }
                    }
                    if !pushed_lang_item {
                        let reason = lv["reason"]
                            .as_str()
                            .unwrap_or_else(|| lv["tail"].as_str().unwrap_or("failed"));
                        out.push(CaseEntry {
                            id: lang.clone(),
                            outcome: "fail".into(),
                            detail: tail_chars(reason, 200),
                        });
                    }
                }
            }
            if total_langs > 0 && avail_count == 0 {
                out.push(CaseEntry {
                    id: "summary".into(),
                    outcome: "skip".into(),
                    detail: "no requested stdfs language toolchains available".into(),
                });
                return out;
            }
        }
        "fio" => {
            if let Some(phases) = v["phases"].as_array() {
                for p in phases {
                    let pname = p["name"].as_str().unwrap_or("phase");
                    let pst = p["status"].as_str().unwrap_or("ok");
                    if pst == "ok" {
                        let r_bw = p["summary"]["read"]["bwBytes"].as_f64().unwrap_or(0.0) / 1_048_576.0;
                        let w_bw = p["summary"]["write"]["bwBytes"].as_f64().unwrap_or(0.0) / 1_048_576.0;
                        let r_iops = p["summary"]["read"]["iops"].as_f64().unwrap_or(0.0);
                        let w_iops = p["summary"]["write"]["iops"].as_f64().unwrap_or(0.0);
                        out.push(CaseEntry {
                            id: pname.to_string(),
                            outcome: "pass".into(),
                            detail: format!(
                                "read {:.0} iops ({:.1} MiB/s), write {:.0} iops ({:.1} MiB/s)",
                                r_iops, r_bw, w_iops, w_bw
                            ),
                        });
                    } else {
                        failed_any = true;
                        out.push(CaseEntry {
                            id: pname.to_string(),
                            outcome: "fail".into(),
                            detail: tail_chars(p["stderrTail"].as_str().unwrap_or("fio phase failed"), 250),
                        });
                    }
                }
            } else if v["status"].as_str().is_some_and(|s| s != "ok") {
                failed_any = true;
                let det = v["failure"]
                    .as_str()
                    .or_else(|| v["stderrTail"].as_str())
                    .unwrap_or("fio failed");
                out.push(CaseEntry {
                    id: "fio".into(),
                    outcome: "fail".into(),
                    detail: tail_chars(det, 300),
                });
            }
        }
        "smallfile" => {
            if let Some(phases) = v["phases"].as_array() {
                for (idx, p) in phases.iter().enumerate() {
                    let op = p["operation"].as_str().unwrap_or("op");
                    let pst = p["status"].as_str().unwrap_or("ok");
                    let r = &p["results"];
                    let id = format!("{idx:02}-{op}");
                    if pst == "ok" {
                        out.push(CaseEntry {
                            id,
                            outcome: "pass".into(),
                            detail: format!(
                                "{:.1} files/s, {:.1} IOPS, {:.2} MiB/s",
                                r["filesPerSec"].as_f64().unwrap_or(0.0),
                                r["IOPS"].as_f64().unwrap_or(0.0),
                                r["MiBps"].as_f64().unwrap_or(0.0)
                            ),
                        });
                    } else {
                        failed_any = true;
                        let det = p["failure"].as_str().unwrap_or(pst);
                        out.push(CaseEntry {
                            id,
                            outcome: "fail".into(),
                            detail: tail_chars(det, 250),
                        });
                    }
                }
            } else if v["status"].as_str().is_some_and(|s| s != "ok") {
                failed_any = true;
                let op = v["params"]["operation"].as_str().unwrap_or("smallfile");
                let det = v["failure"].as_str().or_else(|| v["status"].as_str()).unwrap_or("failed");
                out.push(CaseEntry {
                    id: op.to_string(),
                    outcome: "fail".into(),
                    detail: tail_chars(det, 250),
                });
            }
        }
        "mdtest" => {
            let io_errs = v["ioErrors"].as_u64().unwrap_or(0);
            let ver_errs = v["verificationErrors"].as_u64().unwrap_or(0);
            if io_errs > 0 || ver_errs > 0 {
                failed_any = true;
                out.push(CaseEntry {
                    id: "errors".into(),
                    outcome: "fail".into(),
                    detail: format!("ioErrors={io_errs} verificationErrors={ver_errs}"),
                });
            }
        }
        "mdworkbench" => {
            if let Some(phases) = v["phases"].as_array() {
                for p in phases {
                    let pname = p["name"].as_str().unwrap_or("phase");
                    let agg = &p["aggregate"];
                    let errs = agg["errors"].as_u64().unwrap_or(0);
                    if errs > 0 {
                        failed_any = true;
                        out.push(CaseEntry {
                            id: pname.to_string(),
                            outcome: "fail".into(),
                            detail: format!("errors={errs}"),
                        });
                    } else {
                        out.push(CaseEntry {
                            id: pname.to_string(),
                            outcome: "pass".into(),
                            detail: format!(
                                "{:.1} obj/s, {:.1} iops, {:.2} MiB/s",
                                agg["rateObjPerSec"].as_f64().unwrap_or(0.0),
                                agg["iops"].as_f64().unwrap_or(0.0),
                                agg["throughputMiBps"].as_f64().unwrap_or(0.0),
                            ),
                        });
                    }
                }
            }
        }
        _ => {
            if v["status"].as_str().is_some_and(|s| s != "ok") {
                failed_any = true;
            }
            if let Some(f) = v["failure"].as_str()
                && !f.is_empty()
            {
                failed_any = true;
                out.push(CaseEntry {
                    id: "failure".into(),
                    outcome: "fail".into(),
                    detail: tail_chars(f, 300),
                });
            }
            if let Some(fails) = v["failures"].as_array()
                && !fails.is_empty()
            {
                failed_any = true;
                for (idx, f) in fails.iter().take(25).enumerate() {
                    out.push(CaseEntry {
                        id: format!("failure-{idx}"),
                        outcome: "fail".into(),
                        detail: tail_chars(&f.to_string(), 300),
                    });
                }
            }
        }
    }

    let detail = if v["summary"].is_object() {
        tail_chars(&v["summary"].to_string(), 300)
    } else if v["results"].is_object() && v["results"]["summary"].is_object() {
        tail_chars(&v["results"]["summary"].to_string(), 300)
    } else if v["results"].is_object() && v["results"]["filesPerSec"].is_number() {
        tail_chars(&v["results"].to_string(), 300)
    } else {
        format!("status={}", v["status"].as_str().unwrap_or("?"))
    };
    out.push(CaseEntry {
        id: "summary".into(),
        outcome: if failed_any { "fail".into() } else { "pass".into() },
        detail,
    });
    out
}

fn derive_metrics(suite: &str, v: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    match suite {
        "fio" => {
            let emit_fio_side = |m: &mut Map<String, Value>, prefix: &str, side_obj: &Value| {
                if !side_obj.is_object() {
                    return;
                }
                let iops = side_obj["iops"].as_f64();
                let bw = side_obj["bwBytes"].as_f64();
                insert_metric(m, format!("{prefix}.iops"), iops);
                insert_metric(m, format!("{prefix}.bw_bytes_per_sec"), bw);
                insert_metric(m, format!("{prefix}.bw_mibps"), bw.map(|b| b / 1_048_576.0));
                for (k, out_k) in [
                    ("mean", "clat_mean_ms"),
                    ("p50", "clat_p50_ms"),
                    ("p95", "clat_p95_ms"),
                    ("p99", "clat_p99_ms"),
                ] {
                    let ns = side_obj["clatNs"][k].as_f64();
                    insert_metric(m, format!("{prefix}.{out_k}"), ns.map(|x| x / 1_000_000.0));
                }
            };
            if let Some(phases) = v["phases"].as_array() {
                for p in phases {
                    let pname = slugify(p["name"].as_str().unwrap_or("phase"));
                    for side in ["read", "write"] {
                        emit_fio_side(&mut m, &format!("fio.{pname}.{side}"), &p["summary"][side]);
                    }
                }
            } else {
                for side in ["read", "write"] {
                    emit_fio_side(&mut m, &format!("fio.{side}"), &v["summary"][side]);
                }
            }
        }
        "fsmark" => {
            let s = &v["summary"];
            insert_metric(&mut m, "fsmark.avg_files_per_sec", s["averageFilesPerSec"].as_f64());
            insert_metric(&mut m, "fsmark.p50_files_per_sec", s["p50FilesPerSec"].as_f64());
            insert_metric(&mut m, "fsmark.p90_files_per_sec", s["p90FilesPerSec"].as_f64());
            insert_metric(&mut m, "fsmark.p99_files_per_sec", s["p99FilesPerSec"].as_f64());
            if let Some(iters) = v["iterations"].as_array()
                && !iters.is_empty()
            {
                let n = iters.len() as f64;
                for (field, name) in [
                    ("creatUsec", "creat_avg_ms"),
                    ("writeUsec", "write_avg_ms"),
                    ("fsyncUsec", "fsync_avg_ms"),
                    ("closeUsec", "close_avg_ms"),
                    ("unlinkUsec", "unlink_avg_ms"),
                ] {
                    let avg_us: f64 = iters
                        .iter()
                        .filter_map(|it| it[field]["avg"].as_f64())
                        .sum::<f64>()
                        / n;
                    insert_metric(&mut m, format!("fsmark.{name}"), Some(avg_us / 1000.0));
                }
            }
        }
        "mdtest" => {
            if let Some(summary) = v["summary"].as_object() {
                for (op_name, sv) in summary {
                    let slug = slugify(op_name);
                    insert_metric(&mut m, format!("mdtest.{slug}.total_ops_per_sec"), sv["totalRate"].as_f64());
                    insert_metric(
                        &mut m,
                        format!("mdtest.{slug}.mean_ops_per_sec"),
                        sv["meanRatePerRank"].as_f64(),
                    );
                }
            }
        }
        "mdworkbench" => {
            if let Some(phases) = v["phases"].as_array() {
                for p in phases {
                    let pname = slugify(p["name"].as_str().unwrap_or("phase"));
                    let agg = &p["aggregate"];
                    insert_metric(&mut m, format!("mdworkbench.{pname}.iops"), agg["iops"].as_f64());
                    insert_metric(
                        &mut m,
                        format!("mdworkbench.{pname}.obj_per_sec"),
                        agg["rateObjPerSec"].as_f64(),
                    );
                    insert_metric(
                        &mut m,
                        format!("mdworkbench.{pname}.bw_mibps"),
                        agg["throughputMiBps"].as_f64(),
                    );
                    insert_metric(
                        &mut m,
                        format!("mdworkbench.{pname}.max_op_ms"),
                        agg["opMaxSec"].as_f64().map(|s| s * 1000.0),
                    );
                    for op in ["create", "read", "stat", "delete"] {
                        if agg["latencySec"][op]["count"].as_u64().unwrap_or(0) > 0 {
                            insert_metric(
                                &mut m,
                                format!("mdworkbench.{pname}.{op}_p99_ms"),
                                agg["latencySec"][op]["p99"].as_f64().map(|s| s * 1000.0),
                            );
                        }
                    }
                }
            }
        }
        "smallfile" => {
            if let Some(phases) = v["phases"].as_array() {
                for p in phases {
                    let op = slugify(p["operation"].as_str().unwrap_or("op"));
                    let r = &p["results"];
                    insert_metric(&mut m, format!("smallfile.{op}.files_per_sec"), r["filesPerSec"].as_f64());
                    insert_metric(&mut m, format!("smallfile.{op}.iops"), r["IOPS"].as_f64());
                    insert_metric(&mut m, format!("smallfile.{op}.bw_mibps"), r["MiBps"].as_f64());
                }
            } else if v["results"].is_object() {
                let op = slugify(v["params"]["operation"].as_str().unwrap_or("op"));
                let r = &v["results"];
                insert_metric(&mut m, format!("smallfile.{op}.files_per_sec"), r["filesPerSec"].as_f64());
                insert_metric(&mut m, format!("smallfile.{op}.iops"), r["IOPS"].as_f64());
                insert_metric(&mut m, format!("smallfile.{op}.bw_mibps"), r["MiBps"].as_f64());
            }
        }
        "sqlite" => {
            if let Some(items) = v["results"]["items"].as_array() {
                for it in items {
                    let slug = slugify(it["name"].as_str().unwrap_or("item"));
                    if let Some(dur_ms) = it["durationMs"].as_f64() {
                        insert_metric(&mut m, format!("sqlite.{slug}.duration_ms"), Some(dur_ms));
                        if let Some(tests) = it["tests"].as_f64()
                            && dur_ms > 0.0
                        {
                            insert_metric(
                                &mut m,
                                format!("sqlite.{slug}.tests_per_sec"),
                                Some(tests * 1000.0 / dur_ms),
                            );
                        }
                    }
                }
            }
        }
        "fsrand" => {
            if let Some(modes) = v["modes"].as_array() {
                for mv in modes {
                    let mode = slugify(mv["mode"].as_str().unwrap_or("mode"));
                    if let Some(dur_ms) = mv["durationMs"].as_f64() {
                        insert_metric(&mut m, format!("fsrand.{mode}.duration_ms"), Some(dur_ms));
                        let units = mv["summary"]["ops"]
                            .as_f64()
                            .or_else(|| mv["summary"]["totalFiles"].as_f64());
                        if let Some(u) = units
                            && dur_ms > 0.0
                        {
                            insert_metric(&mut m, format!("fsrand.{mode}.ops_per_sec"), Some(u * 1000.0 / dur_ms));
                        }
                    }
                }
            } else {
                let mode = slugify(v["params"]["mode"].as_str().unwrap_or("tree"));
                if let Some(dur_ms) = v["durationMs"].as_f64() {
                    insert_metric(&mut m, format!("fsrand.{mode}.duration_ms"), Some(dur_ms));
                    let units = v["summary"]["ops"]
                        .as_f64()
                        .or_else(|| v["summary"]["totalFiles"].as_f64());
                    if let Some(u) = units
                        && dur_ms > 0.0
                    {
                        insert_metric(
                            &mut m,
                            format!("fsrand.{mode}.ops_per_sec"),
                            Some(u * 1000.0 / dur_ms),
                        );
                    }
                }
            }
        }
        "pjdfstest" => {
            if let Some(dur_ms) = v["durationMs"].as_f64() {
                insert_metric(&mut m, "pjdfstest.duration_ms", Some(dur_ms));
                if let Some(tests) = v["summary"]["tests"].as_f64()
                    && dur_ms > 0.0
                {
                    insert_metric(&mut m, "pjdfstest.tests_per_sec", Some(tests * 1000.0 / dur_ms));
                }
            }
        }
        "fsx" => {
            if let Some(dur_ms) = v["durationMs"].as_f64() {
                insert_metric(&mut m, "fsx.duration_ms", Some(dur_ms));
                if let Some(ops) = v["opsCompleted"].as_f64()
                    && dur_ms > 0.0
                {
                    insert_metric(&mut m, "fsx.ops_per_sec", Some(ops * 1000.0 / dur_ms));
                }
            }
        }
        "fsstress" => {
            if let Some(dur_ms) = v["durationMs"].as_f64() {
                insert_metric(&mut m, "fsstress.duration_ms", Some(dur_ms));
                if let Some(ops) = v["totalOps"].as_f64()
                    && dur_ms > 0.0
                {
                    insert_metric(&mut m, "fsstress.ops_per_sec", Some(ops * 1000.0 / dur_ms));
                }
            }
        }
        "errno-diff" => {
            if let Some(dur_ms) = v["durationMs"].as_f64() {
                insert_metric(&mut m, "errno_diff.duration_ms", Some(dur_ms));
            }
        }
        _ => {}
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_utf8_preserves_multibyte_boundaries() {
        let s = "hello世界🙂完结";
        for max in 0..=s.len() + 4 {
            let t = tail_utf8(s, max);
            assert!(!t.is_empty());
        }
    }

    #[test]
    fn posix_fallocate_fd_extends_file_with_offset() {
        let p = std::env::temp_dir().join(format!("fstest-falloc-{}", std::process::id()));
        let _ = fs::remove_file(&p);
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&p)
            .unwrap();
        posix_fallocate_fd(f.as_raw_fd(), 20_000, 3_456).unwrap();
        assert_eq!(f.metadata().unwrap().len(), 23_456);
        assert!(posix_fallocate_fd(f.as_raw_fd(), 0, 0).is_err());
        drop(f);
        let ro = File::open(&p).unwrap();
        assert!(posix_fallocate_fd(ro.as_raw_fd(), 0, 128).is_err());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn enrich_stdfs_all_unavailable_yields_skip_summary() {
        let mut rep = json!({
            "suite": "stdfs",
            "status": "ok",
            "results": {
                "languages": {
                    "node": {"available": false, "reason": "no checkout"}
                }
            }
        });
        enrich_report(&mut rep, None);
        let cases = rep["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0]["id"], "node");
        assert_eq!(cases[0]["outcome"], "skip");
        assert_eq!(cases[1]["id"], "summary");
        assert_eq!(cases[1]["outcome"], "skip");
    }

    #[test]
    fn enrich_fio_populates_normalized_metrics() {
        let mut rep = json!({
            "suite": "fio",
            "status": "ok",
            "summary": {
                "read": {
                    "iops": 12500.0,
                    "bwBytes": 52428800.0,
                    "clatNs": {"mean": 250000.0, "p50": 200000, "p95": 450000, "p99": 900000}
                },
                "write": {
                    "iops": 4000.0,
                    "bwBytes": 16777216.0,
                    "clatNs": {"mean": 600000.0, "p50": 500000, "p95": 1200000, "p99": 2500000}
                }
            }
        });
        enrich_report(&mut rep, None);
        assert_eq!(rep["metrics"]["fio.read.iops"].as_f64(), Some(12500.0));
        assert_eq!(rep["metrics"]["fio.read.bw_mibps"].as_f64(), Some(50.0));
        assert_eq!(rep["metrics"]["fio.write.clat_p99_ms"].as_f64(), Some(2.5));
    }

    #[test]
    fn enrich_fio_matrix_populates_cases_and_phase_metrics() {
        let mut rep = json!({
            "suite": "fio",
            "mode": "matrix",
            "status": "ok",
            "phases": [
                {
                    "name": "seq-write",
                    "status": "ok",
                    "summary": {
                        "read": {"iops": 0.0, "bwBytes": 0.0, "clatNs": {}},
                        "write": {"iops": 800.0, "bwBytes": 838860800.0, "clatNs": {"p99": 4000000}}
                    }
                },
                {
                    "name": "rand-read",
                    "status": "ok",
                    "summary": {
                        "read": {"iops": 25000.0, "bwBytes": 102400000.0, "clatNs": {"p99": 500000}},
                        "write": {"iops": 0.0, "bwBytes": 0.0, "clatNs": {}}
                    }
                }
            ]
        });
        enrich_report(&mut rep, None);
        let cases = rep["cases"].as_array().unwrap();
        assert_eq!(cases[0]["id"], "seq-write");
        assert_eq!(cases[0]["outcome"], "pass");
        assert_eq!(cases[1]["id"], "rand-read");
        assert_eq!(rep["metrics"]["fio.seq_write.write.bw_mibps"].as_f64(), Some(800.0));
        assert_eq!(rep["metrics"]["fio.rand_read.read.iops"].as_f64(), Some(25000.0));
        assert_eq!(rep["metrics"]["fio.rand_read.read.clat_p99_ms"].as_f64(), Some(0.5));
    }

    #[test]
    fn mount_snapshot_detects_live_directory() {
        let tmp = std::env::temp_dir();
        let snap = MountSnapshot::capture(&tmp).expect("capture temp_dir");
        let check = snap.verify_after();
        assert!(check.healthy);
        assert!(check.disconnect_reason.is_none());
    }
}
