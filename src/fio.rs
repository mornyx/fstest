// Thin wrapper around the system fio binary: maps the common best-practice knobs
// onto one fio invocation, runs it with JSON output written to a temp file (fio's
// stdout can interleave non-JSON noise), and repackages the result into the
// standard fstest JSON envelope with a normalized per-job summary plus the full
// raw fio JSON. Expert escape hatches: --job <file> (fstest adds only directory
// and output plumbing) and --fio-arg (verbatim passthrough).

use crate::smallfile::{iso8601, yn_bool};
use log::{info, warn};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(clap::Args)]
pub struct Args {
    /// IO pattern: read, write, randread, randwrite, rw, randrw
    #[arg(long, default_value = "randrw")]
    pub rw: String,

    /// block size, fio syntax (4k, 1m, 4k,1m ...)
    #[arg(long, default_value = "4k")]
    pub bs: String,

    /// per-job file size, fio syntax (256m, 1g ...)
    #[arg(long, default_value = "256m")]
    pub size: String,

    /// number of parallel jobs
    #[arg(long, default_value_t = 4)]
    pub numjobs: u32,

    /// IO queue depth per job
    #[arg(long, default_value_t = 32)]
    pub iodepth: u32,

    /// IO engine (default: libaio on Linux, posixaio on macOS; "auto" lets fio choose)
    #[arg(long)]
    pub ioengine: Option<String>,

    /// O_DIRECT, bypass the page cache (Y/N)
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub direct: bool,

    /// percentage of reads in rw/randrw workloads
    #[arg(long, default_value_t = 75)]
    pub rwmixread: u32,

    /// run time-based for this many seconds instead of one size-based pass
    #[arg(long, default_value_t = 0)]
    pub runtime: u32,

    /// aggregate per-job stats into one report (Y/N)
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, value_parser = yn_bool)]
    pub group_reporting: bool,

    /// verify written data contents; the value is passed verbatim to fio --verify (crc32, md5, sha256, meta, pattern, ...). Unset by default, like fio itself
    #[arg(long)]
    pub verify: Option<String>,

    /// expert mode: use a fio job file; fstest only adds --directory and output plumbing
    #[arg(long)]
    pub job: Option<PathBuf>,

    /// extra argument passed verbatim to fio (repeatable)
    #[arg(long = "fio-arg", allow_hyphen_values = true)]
    pub fio_arg: Vec<String>,

    /// keep fio test files after the run (unlinked by default)
    #[arg(long)]
    pub keep_files: bool,

    /// fio binary to invoke
    #[arg(long, default_value = "fio")]
    pub fio: String,

    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

fn default_ioengine() -> Option<&'static str> {
    if cfg!(target_os = "linux") {
        Some("libaio")
    } else if cfg!(target_os = "macos") {
        Some("posixaio")
    } else {
        None
    }
}

fn fio_version(bin: &str) -> Result<String, String> {
    let out = Command::new(bin)
        .arg("--version")
        .output()
        .map_err(|e| format!("fio binary '{bin}' not usable: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// fio >= 3.16 reports completion latency under clat_ns, older versions under clat;
// both hold ns values keyed like "50.000000"
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
    let mean = side.pointer("/clat_ns/mean").or_else(|| side.pointer("/clat/mean"));
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

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let version = fio_version(&args.fio)?;
    let mut cmd: Vec<String> = vec!["--directory".into(), mountpoint.display().to_string()];
    let mut params = json!({
        "rw": args.rw,
        "bs": args.bs,
        "size": args.size,
        "numjobs": args.numjobs,
        "iodepth": args.iodepth,
        "direct": args.direct,
        "rwmixread": args.rwmixread,
        "runtime_sec": args.runtime,
        "group_reporting": args.group_reporting,
        "verify": args.verify,
        "keep_files": args.keep_files,
        "fio": args.fio,
        "job_file": args.job.as_ref().map(|p| p.display().to_string()),
        "extra_fio_args": args.fio_arg,
    });
    match &args.job {
        Some(job) => {
            if !job.exists() {
                return Err(format!("job file {} does not exist", job.display()));
            }
            cmd.push(job.display().to_string());
        }
        None => {
            cmd.extend([
                "--name=fstest".to_string(),
                format!("--rw={}", args.rw),
                format!("--bs={}", args.bs),
                format!("--size={}", args.size),
                format!("--numjobs={}", args.numjobs),
                format!("--iodepth={}", args.iodepth),
                format!("--direct={}", if args.direct { 1 } else { 0 }),
                format!("--group_reporting={}", if args.group_reporting { 1 } else { 0 }),
            ]);
            if !args.keep_files {
                cmd.push("--unlink=1".into());
            }
            if let Some(method) = &args.verify {
                cmd.push(format!("--verify={method}"));
            }
            if args.rw.ends_with("rw") {
                cmd.push(format!("--rwmixread={}", args.rwmixread));
            }
            if let Some(engine) = args.ioengine.as_deref().filter(|e| *e != "auto") {
                cmd.push(format!("--ioengine={engine}"));
            } else if args.ioengine.is_none() {
                if let Some(engine) = default_ioengine() {
                    cmd.push(format!("--ioengine={engine}"));
                }
            }
            if args.runtime > 0 {
                cmd.push(format!("--runtime={}", args.runtime));
                cmd.push("--time_based".into());
            }
        }
    }
    for extra in &args.fio_arg {
        cmd.push(extra.clone());
    }
    let out_path = std::env::temp_dir().join(format!(
        "fstest-fio-{}.json",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    cmd.push("--output-format=json".into());
    cmd.push(format!("--output={}", out_path.display()));
    params["fioArgs"] = json!(cmd);
    info!(
        "fio {} on {}: {} {} jobs={} iodepth={} direct={}",
        version,
        mountpoint.display(),
        args.rw,
        args.bs,
        args.numjobs,
        args.iodepth,
        args.direct
    );
    debug_cmd(&cmd);
    let output = Command::new(&args.fio)
        .args(&cmd)
        .output()
        .map_err(|e| format!("failed to spawn {}: {e}", args.fio))?;
    let stdout_tail = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr_tail = truncate(&String::from_utf8_lossy(&output.stderr).trim().to_string(), 4000);
    let raw =
        fs::read_to_string(&out_path).map_err(|e| format!("fio produced no output file {}: {e}", out_path.display()));
    let _ = fs::remove_file(&out_path);
    let raw = match raw
        .and_then(|s| serde_json::from_str::<Value>(&s).map_err(|e| format!("fio output is not valid JSON: {e}")))
    {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("{e}");
            None
        }
    };
    let status = if output.status.success() && raw.is_some() {
        "ok"
    } else {
        "failed"
    };
    if !stderr_tail.is_empty() {
        info!("fio stderr (tail): {stderr_tail}");
    }
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    let jobs = normalized_jobs(raw.as_ref().unwrap_or(&Value::Null));
    // with group_reporting the first (and usually only) JSON job entry carries the
    // aggregated group numbers, which is what most consumers want up front
    let summary = jobs.first().cloned().unwrap_or(Value::Null);
    let report = json!({
        "suite": "fio",
        "host": host,
        "date": iso8601(SystemTime::now()),
        "top": mountpoint.display().to_string(),
        "fioVersion": version,
        "params": params,
        "status": status,
        "exitCode": output.status.code(),
        "summary": summary,
        "jobs": jobs,
        "raw": raw.unwrap_or(Value::Null),
        "stderrTail": stderr_tail,
        "stdoutTail": truncate(&stdout_tail, 4000),
    });
    if status == "ok" {
        let r = &report["summary"]["read"];
        let w = &report["summary"]["write"];
        info!(
            "fio done: read iops={:.0} bw={:.1}MiB/s | write iops={:.0} bw={:.1}MiB/s",
            r["iops"].as_f64().unwrap_or(0.0),
            r["bwBytes"].as_f64().unwrap_or(0.0) / 1048576.0,
            w["iops"].as_f64().unwrap_or(0.0),
            w["bwBytes"].as_f64().unwrap_or(0.0) / 1048576.0,
        );
    } else {
        warn!(
            "fio exited with {:?}, see stderrTail in the report",
            output.status.code()
        );
    }
    Ok(report)
}

fn debug_cmd(cmd: &[String]) {
    info!("fio cmdline: {}", cmd.join(" "));
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let start = s.len() - max;
        let s = &s[start..];
        // keep whole UTF-8 characters
        let mut off = 0;
        while !s.is_char_boundary(off) {
            off += 1;
        }
        format!("...{}", &s[off..])
    }
}
