// LTP adapter: runs the Linux Test Project suites against a mounted filesystem
// the same way JuiceFS's official compatibility run does — the LTP test
// binaries come from the user's LTP installation, suite selection uses LTP's
// own runtest files, and JuiceFS's published syscall removal list is applied by
// default so results are directly comparable with their published numbers.
//
// fstest executes each filtered runtest line itself with a per-test timeout and
// maps LTP exit codes to results.

use crate::smallfile::iso8601;
use log::{info, warn};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

const JUICEFS_REMOVE_LIST: &str = include_str!("../vendor/ltp/rm_syscalls_juicefs.txt");

// LTP ships no prebuilt binaries (source-only tarball, no distro package), so a
// fresh machine must compile it once. scripts/prepare-ltp.sh is the single
// source of truth and is embedded here so that a fresh machine can bootstrap
// from the fstest binary alone: `fstest ltp --prepare-script | sh`.
pub const PREPARE_SCRIPT: &str = include_str!("../scripts/prepare-ltp.sh");

// Comma-separated to match the `value_delimiter = ','` on --suite: with the
// default carried as one space-separated string, clap treats the whole thing as
// a single suite name and the default run dies with "cannot read runfile
// .../runtest/syscalls fs_bind fs_perms_simple smoketest fcntl-locktests".
const DEFAULT_SUITES: &str = "syscalls,fs_bind,fs_perms_simple,smoketest,fcntl-locktests";

// LTP exit codes
const TPASS: i32 = 0;
const TFAIL: i32 = 1;
const TBROK: i32 = 2;
const TCONF: i32 = 32;

#[derive(clap::Args)]
pub struct Args {
    /// suites to run (runtest file names under <ltp-dir>/runtest)
    #[arg(long, value_delimiter = ',', default_value = DEFAULT_SUITES)]
    pub suite: Vec<String>,

    /// LTP installation directory (contains runtest/ and testcases/bin)
    #[arg(long)]
    pub ltp_dir: Option<PathBuf>,

    /// syscall removal list (defaults to JuiceFS's published list; empty = keep all)
    #[arg(long)]
    pub remove_list: Option<PathBuf>,

    /// keep every test (ignore any removal list)
    #[arg(long)]
    pub no_remove: bool,

    /// per-test timeout in seconds
    #[arg(long, default_value_t = 120)]
    pub timeout: u64,

    /// only run tests whose name contains this substring (repeatable)
    #[arg(long = "filter")]
    pub filter: Vec<String>,

    /// print the LTP prepare script (download + build + install) and exit;
    /// run it on a fresh machine with: fstest ltp --prepare-script | sh
    #[arg(long)]
    pub prepare_script: bool,

    /// cap the number of tests (smoke runs)
    #[arg(long)]
    pub max_tests: Option<usize>,

    /// working directory name under the mount point
    #[arg(long, default_value = ".fstest-ltp")]
    pub work_dir: String,

    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

fn default_ltp_dir_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(d) = std::env::var("LTP_DIR") {
        v.push(PathBuf::from(d));
    }
    v.push(PathBuf::from("/opt/ltp"));
    v.push(PathBuf::from("/usr/lib/ltp"));
    v.push(PathBuf::from("/usr/local/ltp"));
    v
}

fn removal_set(args: &Args) -> HashSet<String> {
    if args.no_remove {
        return HashSet::new();
    }
    let text = match &args.remove_list {
        Some(p) => fs::read_to_string(p).unwrap_or_else(|e| {
            warn!(
                "cannot read remove list {}: {e}; using built-in JuiceFS list",
                p.display()
            );
            JUICEFS_REMOVE_LIST.to_string()
        }),
        None => JUICEFS_REMOVE_LIST.to_string(),
    };
    text.split_whitespace().map(|w| w.to_string()).collect()
}

fn parse_runfile(text: &str, remove: &HashSet<String>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(name) = it.next() else { continue };
        if remove.contains(name) {
            continue;
        }
        if !it.clone().any(|_| true) {
            continue;
        }
        let cmd = it.collect::<Vec<_>>().join(" ");
        out.push((name.to_string(), cmd));
    }
    out
}

fn result_of(code: Option<i32>, timed_out: bool) -> &'static str {
    if timed_out {
        return "timedout";
    }
    match code {
        Some(c) if c == TPASS => "pass",
        Some(c) if c == TFAIL => "fail",
        Some(c) if c == TBROK => "broken",
        Some(c) if c == TCONF => "skip",
        Some(_) => "warn",
        None => "broken",
    }
}

fn run_direct(mountpoint: &Path, args: &Args, ltp_dir: &Path, tests: Vec<(String, String)>) -> Value {
    let bin_dir = ltp_dir.join("testcases/bin");
    let work = mountpoint.join(&args.work_dir);
    let tmp = work.join("tmp");
    let _ = fs::create_dir_all(&tmp);
    let _ = fs::create_dir_all(work.join("cwd"));
    // LTP's own runltp exports LTPROOT and prepends testcases/bin to PATH;
    // tests rely on both (shell tests `. fs_bind_lib.sh` / `. tst_test.sh`
    // resolve via PATH, and tst_test's resource copy plus *_child helper
    // binaries are located relative to LTPROOT). Without them every shell and
    // child-spawning test reports TBROK ("... not found" / "Failed to copy
    // resource '..._child'") regardless of the filesystem under test.
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
    let start = Instant::now();
    for (i, (name, cmd)) in tests.iter().enumerate() {
        let mut it = cmd.split_whitespace();
        let binname = it.next().unwrap_or("");
        let binargs: Vec<&str> = it.collect();
        let binpath = bin_dir.join(binname);
        let mut cmdc = Command::new(binpath.clone());
        cmdc.args(&binargs)
            .current_dir(work.join("cwd"))
            .env("LTPROOT", ltp_dir)
            .env("PATH", &child_path)
            .env("TMPDIR", &tmp)
            .env("LTP_TMPDIR", &tmp);
        unsafe {
            cmdc.pre_exec(|| {
                // own process group so timeout kills test children too
                libc::setsid();
                Ok(())
            });
        }
        let mut child = match cmdc
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                // binary missing from this install: count as broken
                broken += 1;
                results.push(json!({
                    "name": name, "result": "broken", "exitCode": Value::Null,
                    "error": format!("spawn {}: {e}", binpath.display()),
                }));
                continue;
            }
        };
        let deadline = Duration::from_secs(args.timeout.max(1));
        let t0 = Instant::now();
        let mut timed_out = false;
        let code = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st.code(),
                Ok(None) => {
                    if t0.elapsed() > deadline {
                        // kill the whole process group: shell wrappers leave
                        // children holding our stdout pipe otherwise
                        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                        let _ = child.kill();
                        let _ = child.wait();
                        timed_out = true;
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break None,
            }
        };
        let output = child.wait_with_output().ok();
        let out_tail = {
            let s = output
                .as_ref()
                .map(|o| {
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    )
                })
                .unwrap_or_default();
            if s.len() > 2000 {
                format!("...{}", &s[s.len() - 2000..])
            } else {
                s
            }
        };
        let result = result_of(code, timed_out);
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
        results.push(json!({
            "name": name, "result": result, "exitCode": code, "timedOut": timed_out,
            "outputTail": out_tail,
        }));
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
    json!({
        "status": if fail == 0 && broken == 0 && tout == 0 { "ok" } else { "failed" },
        "summary": {
            "total": results.len(), "passed": pass, "failed": fail,
            "broken": broken, "skipped": skip, "warned": warnn, "timedout": tout,
        },
        "tests": results,
    })
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
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
    let remove = removal_set(args);
    let mut tests: Vec<(String, String)> = Vec::new();
    for suite in &args.suite {
        let runfile = ltp_dir.join("runtest").join(suite);
        let text =
            fs::read_to_string(&runfile).map_err(|e| format!("cannot read runfile {}: {e}", runfile.display()))?;
        let n0 = tests.len();
        tests.extend(parse_runfile(&text, &remove));
        info!(
            "suite {suite}: {} tests ({} removed by list)",
            tests.len() - n0,
            remove.len()
        );
    }
    if !args.filter.is_empty() {
        tests.retain(|(name, _)| args.filter.iter().any(|f| name.contains(f)));
    }
    if let Some(cap) = args.max_tests {
        tests.truncate(cap);
    }
    if tests.is_empty() {
        return Err("no tests selected after filtering/removal".into());
    }
    info!(
        "ltp: {} tests selected from {} on {} (removal list: {} entries)",
        tests.len(),
        args.suite.join(","),
        ltp_dir.display(),
        if args.no_remove { 0 } else { remove.len() }
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
            "remove_list": if args.no_remove { Value::Null } else {
                json!(args.remove_list.clone().unwrap_or_else(|| PathBuf::from("<built-in juicefs list>")).display().to_string())
            },
            "timeout_sec": args.timeout,
        },
        "status": body["status"].clone(),
        "results": body,
    });
    if let Some(path) = &args.json {
        if let Err(e) = fs::write(path, serde_json::to_vec_pretty(&report).unwrap()) {
            warn!("failed to write {}: {e}", path.display());
        }
    }
    Ok(report)
}
