// LTP adapter: runs the Linux Test Project suites against a mounted filesystem
// aligned with JuiceFS's official POSIX compatibility & CI methodology:
// - Suites: `syscalls` (`syscalls-jfs`), `fs` (`fs-jfs`), `fs_perms_simple`,
//   and `fcntl-locktests` by default (`fs_bind` and `smoketest` remain opt-in
//   via `--suite` since they test kernel mount namespaces / network / process
//   plumbing rather than the filesystem under test).
// - Filtering: applies JuiceFS's published `rm_syscalls` and `rm_fs` removal
//   lists by default (plus `listmount04` on Linux kernels < 6.11, matching
//   JuiceFS's `ltpsyscalls.yml` skip file), with `--skip` / `--skip-file` /
//   `--no-remove` overrides.
// - Execution: runs each runtest command via `sh -c` with `cwd = <ltp>/testcases/bin`
//   (matching LTP's `kirk` runner so `$TMPDIR`, pipes, and compound commands
//   expand properly while keeping `cwd` off the FUSE mount), `TMPDIR` and
//   `LTP_TMPDIR` pointed at a `01777` directory on the mount under test, and
//   `fcntl-locktests` rewritten from hardcoded `/tmp/...` onto `$TMPDIR/...`.
// - Exit codes & counts: matches `kirk`'s bitmask exit-code semantics
//   (`0=pass`, `4=warn`, `32=skip`, `TFAIL` or unknown non-zero => `fail`,
//   `TBROK` or signal => `broken`) and reports both per-command counts and
//   `kirk`-style sub-assertion totals (`summary.assertions`).

use crate::common::iso8601;
use crate::common::{remove_dir_all_bounded, tail_utf8};
use log::{info, warn};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::io::Read as _;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

const JUICEFS_SYSCALLS_REMOVE_LIST: &str = include_str!("../vendor/ltp/rm_syscalls_juicefs.txt");
const JUICEFS_FS_REMOVE_LIST: &str = include_str!("../vendor/ltp/rm_fs_juicefs.txt");

const DEFAULT_SUITES: &str = "syscalls,fs,fs_perms_simple,fcntl-locktests";

// LTP bitmask exit codes (`include/tst_res_flags.h`)
const TPASS: i32 = 0;
const TFAIL: i32 = 1;
const TBROK: i32 = 2;
const TWARN: i32 = 4;
const TCONF: i32 = 32;

#[derive(clap::Args)]
pub struct Args {
    /// suites to run (runtest file names under <ltp-dir>/runtest; `syscalls-jfs` and `fs-jfs` aliases supported)
    #[arg(long, value_delimiter = ',', default_value = DEFAULT_SUITES)]
    pub suite: Vec<String>,

    /// LTP installation directory (contains runtest/ and testcases/bin)
    #[arg(long)]
    pub ltp_dir: Option<PathBuf>,

    /// custom removal list file overriding the built-in JuiceFS `rm_syscalls` / `rm_fs` lists
    #[arg(long)]
    pub remove_list: Option<PathBuf>,

    /// additional test names to skip (comma-separated or repeatable, like `kirk --skip-tests`)
    #[arg(long, value_delimiter = ',')]
    pub skip: Vec<String>,

    /// additional skip file (one test name per line, `#` comments allowed, like `kirk --skip-file`)
    #[arg(long)]
    pub skip_file: Option<PathBuf>,

    /// keep every test (ignore built-in and custom removal lists)
    #[arg(long)]
    pub no_remove: bool,

    /// per-test timeout in seconds
    #[arg(long, default_value_t = 120)]
    pub timeout: u64,

    /// only run tests whose name contains this substring (repeatable)
    #[arg(long = "filter")]
    pub filter: Vec<String>,

    /// cap the number of tests (smoke runs)
    #[arg(long)]
    pub max_tests: Option<usize>,

    /// working directory name under the mount point
    #[arg(long, default_value = ".fstest-ltp")]
    pub work_dir: String,

    /// keep the working directory under the mount point after the run
    #[arg(long)]
    pub keep: bool,

    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

fn default_ltp_dir_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(d) = std::env::var("LTP_DIR")
        && !d.trim().is_empty()
    {
        v.push(PathBuf::from(d));
    }
    if let Some(bundle) = crate::bundle::ensure_extracted() {
        let p = bundle.join("ltp");
        if p.is_dir() {
            v.push(p);
        }
    }
    for base in crate::common::cache_dir_candidates() {
        let ltp_cache = base.join("ltp");
        if let Ok(rd) = fs::read_dir(&ltp_cache) {
            let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            dirs.sort();
            dirs.reverse();
            for d in dirs {
                if d.join("runtest").is_dir() && d.join("testcases/bin").is_dir() {
                    v.push(d);
                } else if let Ok(sub_rd) = fs::read_dir(&d) {
                    let mut subs: Vec<PathBuf> = sub_rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
                    subs.sort();
                    subs.reverse();
                    v.extend(subs);
                }
            }
        }
    }
    v.push(PathBuf::from("/opt/ltp"));
    v.push(PathBuf::from("/usr/lib/ltp"));
    v.push(PathBuf::from("/usr/local/ltp"));
    v
}

/// Extract the test name (first whitespace-delimited token) from each non-empty,
/// non-comment line. Works identically on JuiceFS `rm_syscalls` / `rm_fs` files
/// (`<name> <cmd...>`) and `kirk` `--skip-file` lists (`<name>`), without
/// accidentally treating command arguments like `-I r` / `-I l` as test prefixes.
fn parse_removal_names(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.split_whitespace().next() {
            out.insert(name.to_string());
        }
    }
    out
}

fn linux_kernel_version() -> Option<(u32, u32)> {
    #[cfg(target_os = "linux")]
    unsafe {
        let mut uts: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut uts) == 0 {
            let rel = std::ffi::CStr::from_ptr(uts.release.as_ptr()).to_string_lossy();
            let mut it = rel.split(['.', '-']);
            let major: u32 = it.next()?.parse().ok()?;
            let minor: u32 = it.next()?.parse().ok()?;
            return Some((major, minor));
        }
    }
    None
}

fn extra_user_skips(args: &Args) -> Result<HashSet<String>, String> {
    let mut out = HashSet::new();
    for s in &args.skip {
        let t = s.trim();
        if !t.is_empty() {
            out.insert(t.to_string());
        }
    }
    if let Some(path) = &args.skip_file {
        let text = fs::read_to_string(path).map_err(|e| format!("cannot read --skip-file {}: {e}", path.display()))?;
        out.extend(parse_removal_names(&text));
    }
    Ok(out)
}

fn removal_set_for_suite(suite: &str, args: &Args, user_skips: &HashSet<String>) -> HashSet<String> {
    if args.no_remove {
        return user_skips.clone();
    }
    let mut set = if let Some(p) = &args.remove_list {
        match fs::read_to_string(p) {
            Ok(text) => parse_removal_names(&text),
            Err(e) => {
                warn!(
                    "cannot read remove list {}: {e}; falling back to built-in JuiceFS lists",
                    p.display()
                );
                builtin_removal_for_suite(suite)
            }
        }
    } else {
        builtin_removal_for_suite(suite)
    };
    set.extend(user_skips.iter().cloned());
    set
}

fn builtin_removal_for_suite(suite: &str) -> HashSet<String> {
    match suite {
        "syscalls" | "syscalls-jfs" => {
            let mut set = parse_removal_names(JUICEFS_SYSCALLS_REMOVE_LIST);
            // Mirror JuiceFS's `.github/workflows/ltpsyscalls.yml` skip rule:
            // LTP 20260529 `listmount04` expects invalid mount-id validation added
            // in Linux 6.11 without `.min_kver = "6.11"`, causing false failures on 6.8/6.1 kernels.
            if let Some((maj, min)) = linux_kernel_version()
                && (maj < 6 || (maj == 6 && min < 11))
            {
                info!("ltp: skipping listmount04 on kernel {maj}.{min} (< 6.11, per JuiceFS ltpsyscalls.yml)");
                set.insert("listmount04".to_string());
            }
            set
        }
        "fs" | "fs-jfs" => parse_removal_names(JUICEFS_FS_REMOVE_LIST),
        _ => HashSet::new(),
    }
}

fn resolve_runfile_path(ltp_dir: &Path, suite: &str) -> PathBuf {
    let direct = ltp_dir.join("runtest").join(suite);
    if direct.is_file() {
        return direct;
    }
    match suite {
        "syscalls-jfs" => ltp_dir.join("runtest/syscalls"),
        "fs-jfs" => ltp_dir.join("runtest/fs"),
        _ => direct,
    }
}

/// Rewrite upstream runtest commands that hardcode `/tmp/...` instead of `$TMPDIR/...`
/// so they actually exercise the mounted filesystem under test rather than host `/tmp`.
fn rewrite_mount_bound_cmd(name: &str, cmd: &str) -> String {
    if name.eq_ignore_ascii_case("FCNTL_LOCKTESTS") || cmd.contains("/tmp/fcntl_locktest_testfile") {
        return cmd.replace("/tmp/fcntl_locktest_testfile", "\"$TMPDIR/fcntl_locktest_testfile\"");
    }
    cmd.to_string()
}

fn parse_runfile(text: &str, remove: &HashSet<String>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let cmd = rest.trim();
        if cmd.is_empty() || remove.contains(name) {
            continue;
        }
        out.push((name.to_string(), rewrite_mount_bound_cmd(name, cmd)));
    }
    out
}

/// Classify LTP process exit status matching `kirk` (`libkirk/ltp.py`):
/// - `0` (`TPASS`) -> `"pass"`
/// - `4` (`TWARN`) -> `"warn"`
/// - `32` (`TCONF`) -> `"skip"`
/// - Any bitmask carrying `TFAIL` (`1`, `3`, `5`, `33`, ...) -> `"fail"`
/// - Any bitmask carrying `TBROK` without `TFAIL` (`2`, `6`, `34`, ...) or signal kill -> `"broken"`
/// - Any other non-zero exit status (e.g. `127` command not found, `255`) -> `"fail"`
fn result_of(code: Option<i32>, timed_out: bool) -> &'static str {
    if timed_out {
        return "timedout";
    }
    match code {
        Some(TPASS) => "pass",
        Some(TWARN) => "warn",
        Some(TCONF) => "skip",
        Some(c) if (c & TFAIL) != 0 => "fail",
        Some(c) if (c & TBROK) != 0 => "broken",
        Some(_) => "fail",
        None => "broken",
    }
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct AssertionCounts {
    passed: u64,
    failed: u64,
    broken: u64,
    skipped: u64,
    warnings: u64,
}

impl AssertionCounts {
    fn total(&self) -> u64 {
        self.passed + self.failed + self.broken + self.skipped + self.warnings
    }

    fn add_assign(&mut self, rhs: Self) {
        self.passed += rhs.passed;
        self.failed += rhs.failed;
        self.broken += rhs.broken;
        self.skipped += rhs.skipped;
        self.warnings += rhs.warnings;
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b';') {
                i += 1;
            }
            if i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                i += 1;
            }
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Parse sub-assertion counts from LTP stdout/stderr the same way `kirk` (`libkirk/ltp.py:read_result`)
/// does so `summary.assertions` is directly comparable with JuiceFS's published `1479 / 1454` table.
fn parse_assertions(clean_out: &str, result: &str) -> AssertionCounts {
    let lines: Vec<&str> = clean_out.lines().collect();
    for idx in (0..lines.len()).rev() {
        if lines[idx].trim() == "Summary:" && idx + 5 < lines.len() {
            let parse_row = |line: &str, key: &str| -> Option<u64> {
                let rest = line.trim().strip_prefix(key)?;
                rest.trim().parse::<u64>().ok()
            };
            if let (Some(passed), Some(failed), Some(broken), Some(skipped), Some(warnings)) = (
                parse_row(lines[idx + 1], "passed"),
                parse_row(lines[idx + 2], "failed"),
                parse_row(lines[idx + 3], "broken"),
                parse_row(lines[idx + 4], "skipped"),
                parse_row(lines[idx + 5], "warnings"),
            ) {
                return AssertionCounts {
                    passed,
                    failed,
                    broken,
                    skipped,
                    warnings,
                };
            }
        }
    }

    let passed = clean_out.matches("TPASS").count() as u64;
    let failed = clean_out.matches("TFAIL").count() as u64;
    let broken = clean_out.matches("TBROK").count() as u64;
    let skipped = (clean_out.matches("TSKIP").count() + clean_out.matches("TCONF").count()) as u64;
    let warnings = clean_out.matches("TWARN").count() as u64;
    if passed + failed + broken + skipped + warnings > 0 {
        return AssertionCounts {
            passed,
            failed,
            broken,
            skipped,
            warnings,
        };
    }

    match result {
        "pass" => AssertionCounts {
            passed: 1,
            ..Default::default()
        },
        "warn" => AssertionCounts {
            warnings: 1,
            ..Default::default()
        },
        "skip" => AssertionCounts {
            skipped: 1,
            ..Default::default()
        },
        "broken" | "timedout" => AssertionCounts {
            broken: 1,
            ..Default::default()
        },
        _ => AssertionCounts {
            failed: 1,
            ..Default::default()
        },
    }
}

fn exec_shell_test(
    cmd_str: &str,
    bin_dir: &Path,
    ltp_dir: &Path,
    child_path: &std::ffi::OsStr,
    tmp_dir: &Path,
    timeout: Duration,
) -> (Option<i32>, bool, String, Option<String>) {
    let mut child_opt = None;
    let mut spawn_err = None;

    for attempt in 0..3 {
        let mut cmdc = Command::new("sh");
        cmdc.arg("-c")
            .arg(cmd_str)
            // Match `kirk` (`cwd = <ltp_dir>/testcases/bin`): keeps cwd off the
            // FUSE mount under test while resolving relative helper lookups.
            .current_dir(bin_dir)
            .env("LTPROOT", ltp_dir)
            .env("PATH", child_path)
            .env("TMPDIR", tmp_dir)
            .env("LTP_TMPDIR", tmp_dir)
            .env("LTP_COLORIZE_OUTPUT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            cmdc.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        match cmdc.spawn() {
            Ok(c) => {
                child_opt = Some(c);
                break;
            }
            Err(e) => {
                spawn_err = Some(e.to_string());
                if attempt < 2 {
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }

    let Some(mut child) = child_opt else {
        return (None, false, String::new(), spawn_err);
    };

    // Drain stdout and stderr concurrently so a verbose test (>64 KiB output)
    // never blocks in pipe `write()` waiting for us while `try_wait()` loops.
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

    let t0 = Instant::now();
    let mut timed_out = false;
    let mut poll_ms = 1u64;
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code(),
            Ok(None) => {
                if t0.elapsed() > timeout {
                    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(poll_ms));
                poll_ms = (poll_ms * 2).min(10);
            }
            Err(_) => break None,
        }
    };
    // Ensure any lingering background children in the session release stdout/stderr pipes
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };

    let so_bytes = t_so.join().unwrap_or_default();
    let se_bytes = t_se.join().unwrap_or_default();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&so_bytes),
        String::from_utf8_lossy(&se_bytes)
    );
    (code, timed_out, combined, None)
}

fn run_direct(mountpoint: &Path, args: &Args, ltp_dir: &Path, tests: Vec<(String, String, String)>) -> Value {
    let bin_dir = ltp_dir.join("testcases/bin");
    let work = mountpoint.join(&args.work_dir);
    let tmp = work.join("tmp");
    let _ = fs::create_dir_all(&tmp);
    // LTP's tst_tmpdir creates LTP_* directly under TMPDIR and several tests
    // (nftw01, symlink03, dirtyc0w …) drop privileges to nobody before doing so.
    // Match /tmp's sticky world-writable 01777 mode so dropped-privilege tests
    // can create scratch dirs.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o1777));
    }
    let child_path = match std::env::var_os("PATH") {
        Some(p) => {
            let mut joined = std::ffi::OsString::from(&bin_dir);
            joined.push(":");
            joined.push(p);
            joined
        }
        None => bin_dir.as_os_str().to_os_string(),
    };

    let mut results = Vec::new();
    let (mut pass, mut fail, mut broken, mut skip, mut warnn, mut tout) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut assertions = AssertionCounts::default();
    let start = Instant::now();
    let deadline = Duration::from_secs(args.timeout.max(1));

    for (i, (suite_name, name, cmd)) in tests.iter().enumerate() {
        let t0 = Instant::now();
        let (code, timed_out, raw_out, spawn_err) =
            exec_shell_test(cmd, &bin_dir, ltp_dir, &child_path, &tmp, deadline);
        if let Some(e) = spawn_err {
            broken += 1;
            assertions.broken += 1;
            results.push(json!({
                "suite": suite_name,
                "name": name,
                "result": "broken",
                "exitCode": Value::Null,
                "error": format!("spawn sh -c {cmd:?}: {e}"),
            }));
            continue;
        }

        let clean_out = strip_ansi(&raw_out);
        let result = result_of(code, timed_out);
        let ac = parse_assertions(&clean_out, result);
        assertions.add_assign(ac);

        match result {
            "pass" | "warn" => pass += 1,
            "fail" => fail += 1,
            "broken" => broken += 1,
            "skip" => skip += 1,
            _ => tout += 1,
        }
        if result == "warn" {
            warnn += 1;
        }
        let out_tail = tail_utf8(&clean_out, 2000);
        results.push(json!({
            "suite": suite_name,
            "name": name,
            "result": result,
            "exitCode": code,
            "timedOut": timed_out,
            "assertions": {
                "passed": ac.passed,
                "failed": ac.failed,
                "broken": ac.broken,
                "skipped": ac.skipped,
                "warnings": ac.warnings,
            },
            "outputTail": out_tail,
        }));
        info!(
            "ltp: [{}] {} -> {} ({}m{}.{:03}s)",
            suite_name,
            name,
            result,
            t0.elapsed().as_secs() / 60,
            t0.elapsed().as_secs() % 60,
            t0.elapsed().subsec_millis()
        );
        if (i + 1) % 100 == 0 {
            info!(
                "ltp: {}/{} tests executed ({}m{}s elapsed)",
                i + 1,
                tests.len(),
                start.elapsed().as_secs() / 60,
                start.elapsed().as_secs() % 60
            );
        }
    }

    if !args.keep {
        remove_dir_all_bounded(&work, Duration::from_secs(20));
    }

    json!({
        "status": if fail == 0 && broken == 0 && tout == 0 { "ok" } else { "failed" },
        "summary": {
            "total": results.len(),
            "passed": pass,
            "failed": fail,
            "broken": broken,
            "skipped": skip,
            "warned": warnn,
            "timedout": tout,
            "assertions": {
                "total": assertions.total(),
                "passed": assertions.passed,
                "failed": assertions.failed,
                "broken": assertions.broken,
                "skipped": assertions.skipped,
                "warnings": assertions.warnings,
            },
        },
        "tests": results,
    })
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    if !cfg!(target_os = "linux") && args.ltp_dir.is_none() {
        let reason = format!("LTP targets Linux only (current OS: {})", std::env::consts::OS);
        info!("ltp: skipping ({reason})");
        let host = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "localhost".to_string());
        let report = json!({
            "suite": "ltp",
            "host": host,
            "date": iso8601(SystemTime::now()),
            "top": mountpoint.display().to_string(),
            "status": "ok",
            "skipped": true,
            "reason": reason,
            "cases": [{
                "id": "ltp-platform",
                "outcome": "skip",
                "detail": reason,
            }],
            "results": {
                "status": "ok",
                "summary": {
                    "total": 0,
                    "passed": 0,
                    "failed": 0,
                    "broken": 0,
                    "skipped": 0,
                    "warned": 0,
                    "timedout": 0,
                    "assertions": {
                        "total": 0,
                        "passed": 0,
                        "failed": 0,
                        "broken": 0,
                        "skipped": 0,
                        "warnings": 0,
                    },
                },
                "tests": [],
            },
        });
        return Ok(report);
    }

    let ltp_dir = match &args.ltp_dir {
        Some(d) => d.clone(),
        None => default_ltp_dir_candidates()
            .into_iter()
            .find(|d| d.join("runtest").is_dir() && d.join("testcases/bin").is_dir())
            .ok_or_else(|| {
                "LTP installation not found; pass --ltp-dir (dir containing runtest/ and testcases/bin)".to_string()
            })?,
    };
    if !cfg!(target_os = "linux") {
        warn!("LTP targets Linux; on this platform only the plumbing is exercised");
    }

    let user_skips = extra_user_skips(args)?;
    let mut tests: Vec<(String, String, String)> = Vec::new();
    for suite in &args.suite {
        let runfile = resolve_runfile_path(&ltp_dir, suite);
        let text =
            fs::read_to_string(&runfile).map_err(|e| format!("cannot read runfile {}: {e}", runfile.display()))?;
        let remove = removal_set_for_suite(suite, args, &user_skips);
        let parsed = parse_runfile(&text, &remove);
        info!(
            "suite {suite}: {} tests selected (remove list: {} entries)",
            parsed.len(),
            remove.len()
        );
        for (name, cmd) in parsed {
            tests.push((suite.clone(), name, cmd));
        }
    }
    if !args.filter.is_empty() {
        tests.retain(|(_, name, _)| args.filter.iter().any(|f| name.contains(f)));
    }
    if let Some(cap) = args.max_tests {
        tests.truncate(cap);
    }
    if tests.is_empty() {
        return Err("no tests selected after filtering/removal".into());
    }
    info!(
        "ltp: {} tests selected across [{}] using {}",
        tests.len(),
        args.suite.join(","),
        ltp_dir.display(),
    );
    let started = SystemTime::now();
    let body = run_direct(mountpoint, args, &ltp_dir, tests);
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    let report = json!({
        "suite": "ltp",
        "host": host,
        "date": iso8601(started),
        "top": mountpoint.display().to_string(),
        "params": {
            "ltp_dir": ltp_dir.display().to_string(),
            "suites": args.suite,
            "remove_list": if args.no_remove {
                Value::Null
            } else {
                json!(
                    args.remove_list
                        .clone()
                        .unwrap_or_else(|| PathBuf::from("<built-in juicefs rm_syscalls + rm_fs>"))
                        .display()
                        .to_string()
                )
            },
            "timeout_sec": args.timeout,
        },
        "status": body["status"].clone(),
        "results": body,
    });
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_lists_extract_only_test_names() {
        let syscalls_rm = parse_removal_names(JUICEFS_SYSCALLS_REMOVE_LIST);
        assert!(syscalls_rm.contains("alarm02"));
        assert!(syscalls_rm.contains("fork06"));
        assert!(syscalls_rm.contains("unlink09"));
        assert!(!syscalls_rm.contains("-n"));
        assert!(!syscalls_rm.contains("1000"));

        let fs_rm = parse_removal_names(JUICEFS_FS_REMOVE_LIST);
        assert_eq!(fs_rm.len(), 39);
        assert!(fs_rm.contains("gf01"));
        assert!(fs_rm.contains("rwtest05"));
        assert!(fs_rm.contains("fs_fill"));
        // Guard against JuiceFS `rm_list.sh`'s whitespace-split bug (`-I r` / `-I l` / `-I p`)
        assert!(!fs_rm.contains("r"));
        assert!(!fs_rm.contains("l"));
        assert!(!fs_rm.contains("p"));
        assert!(!fs_rm.contains("linker01"));
        assert!(!fs_rm.contains("lftest01"));
        assert!(!fs_rm.contains("proc01"));
    }

    #[test]
    fn fcntl_locktests_rewrites_hardcoded_tmp_onto_tmpdir() {
        let parsed = parse_runfile(
            "FCNTL_LOCKTESTS locktests -n 100 -f /tmp/fcntl_locktest_testfile\n",
            &HashSet::new(),
        );
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "FCNTL_LOCKTESTS");
        assert_eq!(parsed[0].1, "locktests -n 100 -f \"$TMPDIR/fcntl_locktest_testfile\"");
    }

    #[test]
    fn exit_code_bitmask_matches_kirk_semantics() {
        assert_eq!(result_of(Some(0), false), "pass");
        assert_eq!(result_of(Some(4), false), "warn");
        assert_eq!(result_of(Some(32), false), "skip");
        assert_eq!(result_of(Some(1), false), "fail");
        assert_eq!(result_of(Some(3), false), "fail"); // TFAIL | TBROK
        assert_eq!(result_of(Some(5), false), "fail"); // TFAIL | TWARN
        assert_eq!(result_of(Some(33), false), "fail"); // TFAIL | TCONF
        assert_eq!(result_of(Some(127), false), "fail"); // shell command not found
        assert_eq!(result_of(Some(255), false), "fail");
        assert_eq!(result_of(Some(2), false), "broken"); // TBROK
        assert_eq!(result_of(Some(6), false), "broken"); // TBROK | TWARN
        assert_eq!(result_of(None, false), "broken"); // killed by signal
        assert_eq!(result_of(Some(0), true), "timedout");
    }

    #[test]
    fn assertion_parser_handles_new_summary_and_legacy_output() {
        let new_api =
            "\x1b[1;32mSummary:\x1b[0m\nSummary:\npassed   12\nfailed   0\nbroken   0\nskipped  2\nwarnings 1\n";
        let clean = strip_ansi(new_api);
        assert_eq!(
            parse_assertions(&clean, "pass"),
            AssertionCounts {
                passed: 12,
                failed: 0,
                broken: 0,
                skipped: 2,
                warnings: 1,
            }
        );

        let legacy = "fs_perms 1 TPASS : r a 004 file\n";
        assert_eq!(
            parse_assertions(legacy, "pass"),
            AssertionCounts {
                passed: 1,
                failed: 0,
                broken: 0,
                skipped: 0,
                warnings: 0,
            }
        );

        assert_eq!(
            parse_assertions("", "skip"),
            AssertionCounts {
                passed: 0,
                failed: 0,
                broken: 0,
                skipped: 1,
                warnings: 0,
            }
        );
    }
}
