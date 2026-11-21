// fio suite: pure pass-through to the compile-time embedded upstream `axboe/fio`
// binary (`bundle/fio/fio`, with `$PATH` fallback only for `--no-default-features`
// / `FSTEST_SKIP_BUNDLE=1` lightweight dev builds).
//
// - `fstest fio [ANY_NATIVE_FIO_ARGS...] <MOUNTPOINT>`: forwards every argument
//   verbatim to embedded `fio` with `--directory=<MOUNTPOINT>` and captures its
//   JSON output into the canonical `fstest` envelope.
// - `fstest fio <MOUNTPOINT>` (zero fio options): runs a standard 5-phase
//   benchmark matrix (`seq-write` 1M, `seq-read` 1M, `rand-write` 4K,
//   `rand-read` 4K, `rand-rw` 4K 75/25) via embedded `fio` inside
//   `<MOUNTPOINT>/.fstest-fio/` and cleans up afterwards.

use crate::common::{host_name, iso8601};
use log::{info, warn};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub enum ParsedInvocation {
    InfoHandled(i32),
    Run {
        mountpoint: PathBuf,
        json_out: Option<PathBuf>,
        fio_args: Vec<String>,
    },
}

fn resolve_fio_bin() -> Result<(PathBuf, &'static str), String> {
    if let Some(bundle) = crate::bundle::ensure_extracted() {
        let bundled = bundle.join("fio/fio");
        if bundled.is_file() {
            return Ok((bundled, "bundled"));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("fio");
            if cand.is_file() {
                return Ok((cand, "system-path"));
            }
        }
    }
    Err("embedded `fio` bundle not present and no `fio` found on PATH (rebuild without FSTEST_SKIP_BUNDLE)".into())
}

fn fio_version(bin: &Path) -> Result<String, String> {
    let out = Command::new(bin)
        .arg("--version")
        .output()
        .map_err(|e| format!("fio binary '{}' not usable: {e}", bin.display()))?;
    if !out.status.success() {
        return Err(format!(
            "fio binary '{}' --version exited with {}",
            bin.display(),
            out.status
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn is_direct_info_flag(s: &str) -> bool {
    matches!(
        s,
        "--version" | "-v" | "--cmdhelp" | "--enghelp" | "--crctest" | "--cpuclock-test" | "--showcmd"
    ) || s.starts_with("--cmdhelp=")
        || s.starts_with("--enghelp=")
        || s.starts_with("--crctest=")
}

fn print_combined_help() -> i32 {
    let (bin, source) = match resolve_fio_bin() {
        Ok(x) => x,
        Err(e) => {
            log::error!("{e}");
            return 1;
        }
    };
    let ver = fio_version(&bin).unwrap_or_else(|_| "fio".into());
    let mut out = std::io::stdout().lock();
    let header = format!(
        "fstest fio — pure pass-through to embedded upstream {ver} ({source})\n\n\
Usage:\n  \
  fstest fio <MOUNTPOINT>                      # run default 5-phase benchmark matrix\n  \
  fstest fio [FIO_OPTIONS...] <MOUNTPOINT>     # pass any native fio options verbatim\n  \
  fstest fio <JOBFILE.fio> <MOUNTPOINT>        # run a native .fio job file on <MOUNTPOINT>\n  \
  fstest fio --cmdhelp=<opt> | --enghelp[=eng] # query upstream fio option / engine docs\n\n\
fstest envelope option:\n  \
  --json <FILE>                                # write enriched fstest JSON report to <FILE>\n"
    );
    if writeln!(out, "{header}").is_err() {
        return 0;
    }
    for (title, arg) in [
        ("--- Upstream `fio --help` ---", "--help"),
        (
            "--- Upstream `fio --cmdhelp=all` (all native job options, pass as `--<name>=<val>`) ---",
            "--cmdhelp=all",
        ),
        ("--- Compiled-in IO Engines (`fio --enghelp`) ---", "--enghelp"),
    ] {
        if let Ok(res) = Command::new(&bin).arg(arg).output()
            && res.status.success()
        {
            let text = String::from_utf8_lossy(&res.stdout);
            if writeln!(out, "{title}\n{}\n", text.trim_end()).is_err() {
                return 0;
            }
        }
    }
    0
}

/// Parse raw tokens after `fstest fio`:
/// - Handles `-h`/`--help` and direct upstream info queries (`--cmdhelp`, `--enghelp`, `--version`, ...).
/// - Extracts `--json <FILE>` (the only `fstest` envelope option) and `<MOUNTPOINT>`, leaving all
///   remaining tokens untouched for direct pass-through to embedded `fio`.
pub fn parse_invocation(raw_argv: &[OsString]) -> Result<ParsedInvocation, String> {
    let tokens: Vec<String> = raw_argv
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();

    if tokens.is_empty() || tokens.iter().any(|t| t == "-h" || t == "--help") {
        return Ok(ParsedInvocation::InfoHandled(print_combined_help()));
    }

    if tokens.iter().any(|t| is_direct_info_flag(t)) {
        let (bin, _) = resolve_fio_bin()?;
        let st = Command::new(&bin)
            .args(&tokens)
            .status()
            .map_err(|e| format!("failed to run {}: {e}", bin.display()))?;
        return Ok(ParsedInvocation::InfoHandled(st.code().unwrap_or(1)));
    }

    // Strip `--json <path>` / `--json=<path>` (and optional `--` separator markers).
    let mut json_out: Option<PathBuf> = None;
    let mut cleaned: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if t == "--" {
            i += 1;
            continue;
        }
        if t == "--json" {
            let Some(v) = tokens.get(i + 1) else {
                return Err("--json requires a file path".into());
            };
            json_out = Some(PathBuf::from(v));
            i += 2;
            continue;
        }
        if let Some(v) = t.strip_prefix("--json=") {
            json_out = Some(PathBuf::from(v));
            i += 1;
            continue;
        }
        cleaned.push(t.clone());
        i += 1;
    }

    // 1. Check if caller already passed native `--directory=<dir>` or `--directory <dir>`.
    let mut explicit_dir: Option<PathBuf> = None;
    let mut without_dir: Vec<String> = Vec::with_capacity(cleaned.len());
    let mut j = 0;
    while j < cleaned.len() {
        let t = &cleaned[j];
        if t == "--directory"
            && let Some(d) = cleaned.get(j + 1)
        {
            explicit_dir = Some(PathBuf::from(d));
            j += 2;
            continue;
        }
        if let Some(d) = t.strip_prefix("--directory=") {
            explicit_dir = Some(PathBuf::from(d));
            j += 1;
            continue;
        }
        without_dir.push(t.clone());
        j += 1;
    }
    if let Some(mountpoint) = explicit_dir {
        return Ok(ParsedInvocation::Run {
            mountpoint,
            json_out,
            fio_args: without_dir,
        });
    }

    // 2. Locate `<MOUNTPOINT>`: scan from the end for an existing directory positional token
    //    (not preceded by `--opendir`), falling back to the last non-flag, non-`.fio` token
    //    so non-existent mountpoints report a clean error.
    let mut mnt_idx: Option<usize> = None;
    for idx in (0..cleaned.len()).rev() {
        let tok = &cleaned[idx];
        if tok.starts_with('-') {
            continue;
        }
        if idx > 0 && cleaned[idx - 1] == "--opendir" {
            continue;
        }
        if Path::new(tok).is_dir() {
            mnt_idx = Some(idx);
            break;
        }
    }
    if mnt_idx.is_none() {
        for idx in (0..cleaned.len()).rev() {
            let tok = &cleaned[idx];
            let preceded_by_bare_flag =
                idx > 0 && cleaned[idx - 1].starts_with('-') && !cleaned[idx - 1].contains('=');
            if !tok.starts_with('-') && !tok.ends_with(".fio") && !preceded_by_bare_flag {
                mnt_idx = Some(idx);
                break;
            }
        }
    }
    if mnt_idx.is_none() {
        for idx in (0..cleaned.len()).rev() {
            let tok = &cleaned[idx];
            if !tok.starts_with('-') && !tok.ends_with(".fio") {
                mnt_idx = Some(idx);
                break;
            }
        }
    }

    let Some(midx) = mnt_idx else {
        return Err("missing <MOUNTPOINT> directory (see `fstest fio -h`)".into());
    };
    let mountpoint = PathBuf::from(&cleaned[midx]);
    cleaned.remove(midx);

    Ok(ParsedInvocation::Run {
        mountpoint,
        json_out,
        fio_args: cleaned,
    })
}

/// Probe whether `dir` supports 4 KiB-aligned `O_DIRECT` (Linux) / `F_NOCACHE` (macOS) writes
/// (used only by the zero-argument default benchmark matrix).
fn probe_direct_io(dir: &Path) -> bool {
    let probe = dir.join(format!(".fstest-fio-direct-probe-{}", std::process::id()));
    let Ok(cpath) = std::ffi::CString::new(probe.as_os_str().as_bytes()) else {
        return false;
    };
    #[cfg(target_os = "linux")]
    let flags = libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC | libc::O_DIRECT;
    #[cfg(not(target_os = "linux"))]
    let flags = libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC;
    let fd = unsafe { libc::open(cpath.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        if unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1) } != 0 {
            unsafe {
                libc::close(fd);
                libc::unlink(cpath.as_ptr());
            }
            return false;
        }
    }
    let mut ptr: *mut libc::c_void = std::ptr::null_mut();
    let ok = if unsafe { libc::posix_memalign(&mut ptr, 4096, 4096) } == 0 && !ptr.is_null() {
        unsafe {
            std::ptr::write_bytes(ptr as *mut u8, 0xa5, 4096);
        }
        let n = unsafe { libc::pwrite(fd, ptr, 4096, 0) };
        unsafe {
            libc::free(ptr);
        }
        n == 4096
    } else {
        false
    };
    unsafe {
        libc::close(fd);
        libc::unlink(cpath.as_ptr());
    }
    ok
}

fn default_matrix_ioengine() -> &'static str {
    if cfg!(target_os = "linux") {
        "libaio"
    } else {
        "posixaio"
    }
}

fn clat_percentiles(side: &Value, names: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for n in names {
        let key = format!("{n}.000000");
        let v = side
            .pointer(&format!("/clat_ns/percentile/{key}"))
            .or_else(|| side.pointer(&format!("/clat/percentile/{key}")));
        if let Some(v) = v {
            out.insert(format!("p{n}"), v.clone());
        }
    }
    let mean = side
        .pointer("/clat_ns/mean")
        .or_else(|| side.pointer("/clat/mean"));
    if let Some(m) = mean {
        out.insert("mean".into(), m.clone());
    }
    Value::Object(out)
}

fn side_json(side: &Value) -> Value {
    json!({
        "iops": side.get("iops").cloned().unwrap_or(json!(null)),
        "bwBytes": side.get("bw_bytes").cloned().unwrap_or(json!(null)),
        "ioBytes": side.get("io_bytes").cloned().unwrap_or(json!(null)),
        "clatNs": clat_percentiles(side, &["50", "95", "99"]),
    })
}

fn aggregate_side(jobs: &[Value], side: &str) -> Value {
    let mut iops = 0.0;
    let mut bw_bytes: u64 = 0;
    let mut io_bytes: u64 = 0;
    let mut weighted_mean_sum = 0.0;
    let mut weight_total = 0.0;
    let mut max_pct: [Option<u64>; 3] = [None, None, None];
    let pct_keys = ["p50", "p95", "p99"];

    for j in jobs {
        let s = &j[side];
        let j_iops = s["iops"].as_f64().unwrap_or(0.0);
        iops += j_iops;
        bw_bytes += s["bwBytes"].as_u64().unwrap_or(0);
        io_bytes += s["ioBytes"].as_u64().unwrap_or(0);
        if let Some(m) = s["clatNs"]["mean"].as_f64() {
            let w = if j_iops > 0.0 { j_iops } else { 1.0 };
            weighted_mean_sum += m * w;
            weight_total += w;
        }
        for (idx, pk) in pct_keys.iter().enumerate() {
            if let Some(v) = s["clatNs"][*pk].as_u64() {
                max_pct[idx] = Some(max_pct[idx].map_or(v, |cur| cur.max(v)));
            }
        }
    }

    let mut clat = serde_json::Map::new();
    for (idx, pk) in pct_keys.iter().enumerate() {
        if let Some(v) = max_pct[idx] {
            clat.insert((*pk).into(), json!(v));
        }
    }
    if weight_total > 0.0 {
        clat.insert("mean".into(), json!(weighted_mean_sum / weight_total));
    }
    json!({
        "iops": iops,
        "bwBytes": bw_bytes,
        "ioBytes": io_bytes,
        "clatNs": Value::Object(clat),
    })
}

fn summarize_jobs(jobs: &[Value]) -> Value {
    match jobs {
        [] => Value::Null,
        [single] => single.clone(),
        many => {
            let runtime_ms = many
                .iter()
                .filter_map(|j| j["runtimeMs"].as_u64())
                .max();
            json!({
                "jobname": many[0].get("jobname").cloned().unwrap_or(json!("all")),
                "runtimeMs": runtime_ms,
                "read": aggregate_side(many, "read"),
                "write": aggregate_side(many, "write"),
            })
        }
    }
}

fn normalized_jobs(raw: &Value) -> Vec<Value> {
    raw.get("jobs")
        .and_then(|j| j.as_array())
        .map(|jobs| {
            jobs.iter()
                .map(|j| {
                    json!({
                        "jobname": j.get("jobname").cloned().unwrap_or(json!(null)),
                        "runtimeMs": j.get("job_runtime").cloned().unwrap_or(json!(null)),
                        "read": j.get("read").map(side_json).unwrap_or(Value::Null),
                        "write": j.get("write").map(side_json).unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Upstream `fio --output-format=json --output=...` may prepend plain-text `fio: ...`
/// diagnostic lines before the root `{` JSON object. Split those out cleanly.
fn extract_fio_json_slice(s: &str) -> (&str, &str) {
    let trimmed = s.trim_start();
    if trimmed.starts_with('{') {
        return ("", trimmed);
    }
    if let Some(pos) = s.find("\n{") {
        return (s[..pos].trim(), &s[pos + 1..]);
    }
    if let Some(pos) = s.find("\r{") {
        return (s[..pos].trim(), &s[pos + 1..]);
    }
    if let Some(pos) = s.find('{') {
        return (s[..pos].trim(), &s[pos..]);
    }
    ("", s)
}

struct FioExecResult {
    ok: bool,
    exit_code: Option<i32>,
    raw: Value,
    jobs: Vec<Value>,
    summary: Value,
    stderr_tail: String,
    stdout_tail: String,
    failure: Option<String>,
    cmdline: Vec<String>,
}

fn invoke_fio(bin: &Path, mut cmd: Vec<String>) -> Result<FioExecResult, String> {
    let out_path = std::env::temp_dir().join(format!(
        "fstest-fio-{}-{}.json",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    cmd.push("--output-format=json".into());
    cmd.push(format!("--output={}", out_path.display()));
    info!("fio cmdline: {} {}", bin.display(), cmd.join(" "));

    let output = Command::new(bin)
        .args(&cmd)
        .output()
        .map_err(|e| format!("failed to spawn {}: {e}", bin.display()))?;
    let stdout_tail = crate::common::tail_utf8(String::from_utf8_lossy(&output.stdout).trim(), 4000);
    let mut stderr_tail = crate::common::tail_utf8(String::from_utf8_lossy(&output.stderr).trim(), 4000);

    let raw_res = fs::read_to_string(&out_path)
        .map_err(|e| format!("fio produced no JSON output {}: {e}", out_path.display()))
        .and_then(|s| {
            let (preamble, json_str) = extract_fio_json_slice(&s);
            if !preamble.is_empty() {
                info!("fio output preamble: {preamble}");
                if stderr_tail.is_empty() {
                    stderr_tail = crate::common::tail_utf8(preamble, 4000);
                }
            }
            serde_json::from_str::<Value>(json_str)
                .map_err(|e| format!("fio output is not valid JSON: {e}"))
        });
    let _ = fs::remove_file(&out_path);
    if !stderr_tail.is_empty() {
        info!("fio stderr (tail): {stderr_tail}");
    }

    let (raw, parse_err) = match raw_res {
        Ok(v) => (v, None),
        Err(e) => {
            warn!("{e}");
            (Value::Null, Some(e))
        }
    };
    let ok = output.status.success() && parse_err.is_none();
    let jobs = normalized_jobs(&raw);
    let summary = summarize_jobs(&jobs);
    let failure = if ok {
        None
    } else if !stderr_tail.is_empty() {
        Some(stderr_tail.clone())
    } else if let Some(pe) = parse_err {
        Some(pe)
    } else {
        Some(format!("fio exited with {:?}", output.status.code()))
    };

    Ok(FioExecResult {
        ok,
        exit_code: output.status.code(),
        raw,
        jobs,
        summary,
        stderr_tail,
        stdout_tail,
        failure,
        cmdline: cmd,
    })
}

fn has_name_or_jobfile(fio_args: &[String]) -> bool {
    let mut i = 0;
    while i < fio_args.len() {
        let a = &fio_args[i];
        if a == "--name" || a.starts_with("--name=") {
            return true;
        }
        if !a.starts_with('-') {
            // Check if this non-flag token is a standalone positional jobfile (not a value of a previous `--opt val`).
            if i == 0 || fio_args[i - 1].contains('=') || a.ends_with(".fio") || Path::new(a).is_file() {
                return true;
            }
        }
        i += 1;
    }
    false
}

pub fn run(mountpoint: &Path, fio_args: &[String]) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint)
        .map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let (fio_bin, fio_source) = resolve_fio_bin()?;
    let version = fio_version(&fio_bin)?;

    if fio_args.is_empty() {
        run_default_matrix(mountpoint, &fio_bin, fio_source, version)
    } else {
        run_passthrough(mountpoint, fio_args, &fio_bin, fio_source, version)
    }
}

fn run_passthrough(
    mountpoint: &Path,
    fio_args: &[String],
    fio_bin: &Path,
    fio_source: &'static str,
    version: String,
) -> Result<Value, String> {
    let mut cmd: Vec<String> = vec![format!("--directory={}", mountpoint.display())];
    if !has_name_or_jobfile(fio_args) {
        cmd.push("--name=fstest".into());
    }
    cmd.extend(fio_args.iter().cloned());

    let ex = invoke_fio(fio_bin, cmd)?;
    let status = if ex.ok { "ok" } else { "failed" };
    let mut report = json!({
        "suite": "fio",
        "mode": "passthrough",
        "host": host_name(),
        "date": iso8601(SystemTime::now()),
        "top": mountpoint.display().to_string(),
        "fioVersion": version,
        "params": {
            "fio_source": fio_source,
            "fio_bin": fio_bin.display().to_string(),
            "fioArgs": ex.cmdline,
        },
        "status": status,
        "exitCode": ex.exit_code,
        "summary": ex.summary,
        "jobs": ex.jobs,
        "raw": ex.raw,
        "stderrTail": ex.stderr_tail,
        "stdoutTail": ex.stdout_tail,
    });
    if let Some(f) = &ex.failure {
        report["failure"] = json!(f);
    }
    log_phase_summary("fio", &report["summary"], ex.ok, ex.exit_code);
    Ok(report)
}

fn run_default_matrix(
    mountpoint: &Path,
    fio_bin: &Path,
    fio_source: &'static str,
    version: String,
) -> Result<Value, String> {
    let work_dir = mountpoint.join(".fstest-fio");
    fs::create_dir_all(&work_dir).map_err(|e| format!("mkdir {}: {e}", work_dir.display()))?;
    let direct = if probe_direct_io(&work_dir) { 1 } else { 0 };
    let engine = default_matrix_ioengine();

    // (name, prefix, rw, bs, size, iodepth, runtime, time_based, readonly, end_fsync)
    let specs: &[(&str, &str, &str, &str, &str, u32, u32, bool, bool, bool)] = &[
        ("seq-write", "seq", "write", "1m", "256m", 16, 30, false, false, true),
        ("seq-read", "seq", "read", "1m", "256m", 16, 30, false, true, false),
        ("rand-write", "rand", "randwrite", "4k", "64m", 32, 10, true, false, true),
        ("rand-read", "rand", "randread", "4k", "64m", 32, 10, true, true, false),
        ("rand-rw", "rand", "randrw", "4k", "64m", 32, 10, true, false, true),
    ];

    info!(
        "fio {} ({}) default matrix on {}: 5 phases (direct={direct} ioengine={engine})",
        version,
        fio_source,
        mountpoint.display()
    );

    let mut phases = Vec::with_capacity(specs.len());
    let mut any_failed = false;
    let mut first_failure: Option<String> = None;

    for &(name, prefix, rw, bs, size, qd, rt, time_based, readonly, end_fsync) in specs {
        let mut cmd: Vec<String> = vec![
            format!("--directory={}", work_dir.display()),
            format!("--name={name}"),
            format!("--filename_format={prefix}.$jobnum"),
            format!("--rw={rw}"),
            format!("--bs={bs}"),
            format!("--size={size}"),
            "--numjobs=4".into(),
            format!("--iodepth={qd}"),
            format!("--direct={direct}"),
            format!("--ioengine={engine}"),
            "--group_reporting=1".into(),
            "--unlink=0".into(),
            "--invalidate=1".into(),
            format!("--runtime={rt}"),
        ];
        if time_based {
            cmd.push("--time_based".into());
        }
        if readonly {
            cmd.push("--readonly".into());
            cmd.push("--allow_file_create=0".into());
        }
        if end_fsync {
            cmd.push("--end_fsync=1".into());
        }
        if rw == "randrw" {
            cmd.push("--rwmixread=75".into());
        }

        let ex = invoke_fio(fio_bin, cmd)?;
        log_phase_summary(&format!("fio[{name}]"), &ex.summary, ex.ok, ex.exit_code);
        if !ex.ok {
            any_failed = true;
            if first_failure.is_none() {
                first_failure = ex.failure.clone();
            }
        }
        phases.push(json!({
            "name": name,
            "rw": rw,
            "bs": bs,
            "size": size,
            "iodepth": qd,
            "status": if ex.ok { "ok" } else { "failed" },
            "exitCode": ex.exit_code,
            "summary": ex.summary,
            "jobs": ex.jobs,
            "stderrTail": ex.stderr_tail,
        }));

        if name == "seq-read" {
            for j in 0..4 {
                let _ = fs::remove_file(work_dir.join(format!("seq.{j}")));
            }
        }
        if !ex.ok && (name == "seq-write" || name == "rand-write") {
            break;
        }
    }

    let _ = fs::remove_dir_all(&work_dir);

    let mut summary_map = serde_json::Map::new();
    for p in &phases {
        if let Some(name) = p["name"].as_str() {
            summary_map.insert(
                name.to_string(),
                json!({
                    "status": p["status"],
                    "read": p["summary"]["read"],
                    "write": p["summary"]["write"],
                }),
            );
        }
    }

    let mut report = json!({
        "suite": "fio",
        "mode": "matrix",
        "host": host_name(),
        "date": iso8601(SystemTime::now()),
        "top": mountpoint.display().to_string(),
        "fioVersion": version,
        "params": {
            "mode": "matrix",
            "fio_source": fio_source,
            "fio_bin": fio_bin.display().to_string(),
            "direct": direct == 1,
            "ioengine": engine,
        },
        "status": if any_failed { "failed" } else { "ok" },
        "summary": Value::Object(summary_map),
        "phases": phases,
    });
    if let Some(f) = first_failure {
        report["failure"] = json!(f);
    }
    Ok(report)
}

fn log_phase_summary(label: &str, summary: &Value, ok: bool, code: Option<i32>) {
    if ok {
        let r = &summary["read"];
        let w = &summary["write"];
        info!(
            "{label} done: read iops={:.0} bw={:.1}MiB/s | write iops={:.0} bw={:.1}MiB/s",
            r["iops"].as_f64().unwrap_or(0.0),
            r["bwBytes"].as_f64().unwrap_or(0.0) / 1048576.0,
            w["iops"].as_f64().unwrap_or(0.0),
            w["bwBytes"].as_f64().unwrap_or(0.0) / 1048576.0,
        );
    } else {
        warn!("{label} exited with {code:?}, see stderrTail in the report");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_invocation_preserves_native_fio_args_verbatim() {
        let tmp = std::env::temp_dir();
        let argv: Vec<OsString> = [
            "--rw",
            "randrw",
            "--bs=4k",
            "--nrfiles=4",
            "--end_fsync=1",
            "--json",
            "/tmp/out.json",
            tmp.to_str().unwrap(),
        ]
        .into_iter()
        .map(OsString::from)
        .collect();

        let ParsedInvocation::Run {
            mountpoint,
            json_out,
            fio_args,
        } = parse_invocation(&argv).expect("parse")
        else {
            panic!("expected Run");
        };
        assert_eq!(mountpoint, tmp);
        assert_eq!(json_out, Some(PathBuf::from("/tmp/out.json")));
        assert_eq!(
            fio_args,
            vec!["--rw", "randrw", "--bs=4k", "--nrfiles=4", "--end_fsync=1"]
        );
    }

    #[test]
    fn parse_invocation_zero_args_selects_matrix() {
        let tmp = std::env::temp_dir();
        let argv = vec![OsString::from(tmp.to_str().unwrap())];
        let ParsedInvocation::Run {
            mountpoint,
            fio_args,
            ..
        } = parse_invocation(&argv).expect("parse")
        else {
            panic!("expected Run");
        };
        assert_eq!(mountpoint, tmp);
        assert!(fio_args.is_empty());
    }

    #[test]
    fn extract_fio_json_strips_leading_preamble_lines() {
        let raw = "Jobs: 1 (f=1)\rfio: verification note\n{\n  \"fio version\": \"fio-3.38\"\n}\n";
        let (pre, js) = extract_fio_json_slice(raw);
        assert!(pre.contains("fio: verification note"));
        let v: Value = serde_json::from_str(js).expect("valid json");
        assert_eq!(v["fio version"], json!("fio-3.38"));
    }

    #[test]
    fn summarize_jobs_aggregates_ungrouped_multi_job_runs() {
        let jobs = vec![
            json!({
                "jobname": "fstest",
                "runtimeMs": 1000,
                "read": {"iops": 500.0, "bwBytes": 2048000u64, "ioBytes": 2048000u64, "clatNs": {"mean": 1000.0, "p99": 2000u64}},
                "write": {"iops": 0.0, "bwBytes": 0u64, "ioBytes": 0u64, "clatNs": {}}
            }),
            json!({
                "jobname": "fstest",
                "runtimeMs": 1010,
                "read": {"iops": 700.0, "bwBytes": 2867200u64, "ioBytes": 2867200u64, "clatNs": {"mean": 2000.0, "p99": 3500u64}},
                "write": {"iops": 0.0, "bwBytes": 0u64, "ioBytes": 0u64, "clatNs": {}}
            }),
        ];
        let s = summarize_jobs(&jobs);
        assert_eq!(s["read"]["iops"].as_f64(), Some(1200.0));
        assert_eq!(s["read"]["bwBytes"].as_u64(), Some(4_915_200));
        assert_eq!(s["read"]["clatNs"]["p99"].as_u64(), Some(3500));
        assert_eq!(s["runtimeMs"].as_u64(), Some(1010));
    }
}
