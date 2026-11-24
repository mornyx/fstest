// stdfs: runs the filesystem-relevant portions of the Go, Python, Node.js and
// Rust standard-library test suites against a mounted filesystem. By default,
// all four runtimes and test suites are embedded at compile time per target
// platform (with optional CLI overrides for custom installations):
//   go:     precompiled `go_<pkg>.test` binaries + source/testdata fixtures
//           (`os`, `path/filepath`, `io/fs`), run with cwd and TMPDIR on mount.
//   python: standalone CPython 3.12 + matched `Lib/test` over 13 FS modules
//           (`os`, `posix`, `posixpath`, `genericpath`, `pathlib`, `shutil`,
//           `tarfile`, `stat`, `fileio`, `glob`, `tempfile`, `mmap`, `fcntl`).
//   node:   Node.js v22 `node --test` over curated `test/parallel/test-fs-*.js`.
//   rust:   `library/std/src/fs/tests.rs` transformed at build time and linked
//           directly into `fstest __stdfs-rust` (parallel execution on mount).
// Languages whose toolchain or source dir cannot be resolved are reported as
// unavailable and don't fail the run; real test or module failures do.

use crate::common::iso8601;
use log::{info, warn};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

// tests using std-internal APIs that cannot compile outside the std tree
const RUST_EXCLUDE: &[(&str, &str)] = &[
    ("test_dir_smoke_test", "uses unstable std-internal fs::Dir (dirfd)"),
    ("test_dir_read_file", "uses unstable std-internal fs::Dir (dirfd)"),
    ("file_test_io_smoke_test", "uses unstable read_buf/BorrowedBuf APIs"),
    (
        "file_test_io_non_positional_read",
        "uses unstable read_buf/BorrowedBuf APIs",
    ),
    ("test_read_buf_at", "uses unstable read_buf/BorrowedBuf APIs"),
    ("test_read_buf_exact_at", "uses unstable read_buf/BorrowedBuf APIs"),
    ("test_seek_read_buf", "uses unstable read_buf/BorrowedBuf APIs"),
    ("file_test_read_buf", "uses unstable read_buf/BorrowedBuf APIs"),
    ("test_fs_set_times", "uses unstable free-fn fs::set_times"),
    ("test_fs_set_times_nofollow", "uses unstable free-fn fs::set_times"),
    (
        "test_fs_set_times_follows_symlink",
        "uses unstable free-fn fs::set_times",
    ),
    ("test_fs_set_times_on_dir", "uses unstable free-fn fs::set_times"),
    ("metadata_access_times", "uses unstable io ErrorKind::Uncategorized"),
    ("dir_entry_debug", "touches std-internal DirEntry field"),
];

// `TestVariousDeadlines*` tests anonymous kernel-memory `os.Pipe()` sleep loops
// (~18s of non-FS sleeps); pass `--go-skip ""` to include them.
const GO_SKIP_DEFAULT: &str = "^TestVariousDeadlines(1Proc|4Proc)?$";

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 8))
        .unwrap_or(4)
}

#[derive(clap::Args)]
pub struct Args {
    /// languages to run, comma list (go,python,node,rust)
    #[arg(long, value_delimiter = ',', default_value = "go,python,node,rust")]
    pub lang: Vec<String>,

    /// Go: GOROOT override (uses <dir>/bin/go; default: bundled prebuilt, else `go` on PATH)
    #[arg(long)]
    pub go_dir: Option<PathBuf>,
    /// Go: packages to test
    #[arg(long, value_delimiter = ',', default_value = "os,path/filepath,io/fs")]
    pub go_packages: Vec<String>,
    /// Go: -test.skip regex override (default: none)
    #[arg(long)]
    pub go_skip: Option<String>,

    /// Python: directory whose `test/` package to run (a CPython Lib/ dir)
    #[arg(long)]
    pub python_dir: Option<PathBuf>,
    /// Python: interpreter override (default: bundled python3, else python3 on PATH)
    #[arg(long)]
    pub python_exe: Option<PathBuf>,
    /// Python: test modules (test_ prefix added as needed)
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "os,posix,posixpath,genericpath,pathlib,shutil,tarfile,stat,fileio,glob,tempfile,mmap,fcntl"
    )]
    pub python_modules: Vec<String>,

    /// Python: extra regrtest ignore patterns (test ids like test.test_os.ForkTests.test_fork)
    #[arg(long, value_delimiter = ',')]
    pub python_ignore: Vec<String>,

    /// Node: nodejs/node checkout containing test/parallel (default: bundled Node + test-fs-*.js)
    #[arg(long)]
    pub node_dir: Option<PathBuf>,
    /// Node: only run test files whose name contains this substring
    #[arg(long)]
    pub node_filter: Option<String>,
    /// Node: force-enable event-driven `fs.watch` / `fsPromises.watch` tests (default: on on Linux, off on macOS)
    #[arg(long, conflicts_with = "no_node_watch")]
    pub node_watch: bool,
    /// Node: disable event-driven `fs.watch` / `fsPromises.watch` tests (`stat`-polling `fs.watchFile` stays enabled)
    #[arg(long)]
    pub no_node_watch: bool,

    /// Rust: rust `library/` source dir (default: embedded at compile time)
    #[arg(long)]
    pub rust_src_dir: Option<PathBuf>,
    /// Rust: skip tests whose name contains this substring (repeatable); they
    /// are reported as `skipped` rather than failing the run
    #[arg(long)]
    pub rust_skip: Vec<String>,
    /// Rust: per-test timeout in seconds inside the generated runner
    /// (env STDFS_TEST_TIMEOUT overrides); a wedged test is reported as
    /// `timedout` and the run continues
    #[arg(long, default_value_t = 120)]
    pub rust_test_timeout: u64,

    /// per-item timeout in seconds
    #[arg(long, default_value_t = 600)]
    pub timeout: u64,
    /// concurrent items within a language
    #[arg(long, default_value_t = default_jobs())]
    pub jobs: usize,
    /// working directory name under the mount point
    #[arg(long, default_value = ".fstest-stdfs")]
    pub work_dir: String,
    /// keep the working directory on the mount (and host scratch dir) when done
    #[arg(long)]
    pub keep: bool,
    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    find_all_in_path(name).into_iter().next()
}

fn find_all_in_path(name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(path) = std::env::var("PATH") else {
        return out;
    };
    for dir in path.split(':') {
        let p = Path::new(dir).join(name);
        if p.is_file() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

fn cmd_stdout(mut cmd: Command) -> Option<String> {
    let out = cmd.stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn tail(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut start = s.len() - cap;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("...{}", &s[start..])
}

struct Exec {
    code: Option<i32>,
    timed_out: bool,
    out: String,
    spawn_err: Option<String>,
}

// runs a command in its own process group so a timeout can take the whole
// tree down (children holding our stdout pipe would otherwise hang the drain)
fn exec_with_timeout(mut cmd: Command, timeout: Duration) -> Exec {
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
            return Exec {
                code: None,
                timed_out: false,
                out: String::new(),
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
    let mut out = String::from_utf8_lossy(&so_bytes).into_owned();
    let err = String::from_utf8_lossy(&se_bytes);
    if !err.is_empty() {
        out.push('\n');
        out.push_str(&err);
    }
    Exec {
        code,
        timed_out,
        out,
        spawn_err: None,
    }
}

fn run_parallel<R: Send>(n: usize, jobs: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..n).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= n {
                        break;
                    }
                    let r = f(i);
                    out.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    out.into_inner()
        .unwrap()
        .into_iter()
        .map(|x| x.expect("worker result"))
        .collect()
}

fn unavailable(reason: String) -> Value {
    json!({"available": false, "reason": reason})
}

// remove_dir_all in a detached thread with a bounded wait: recursive removal
// on a wedged network/FUSE mount can block forever, and the report must still
// be emitted. On timeout the thread is simply abandoned (the process exits
// shortly after, discarding it) and a warning records the leftover path.
fn remove_dir_all_bounded(path: &Path, bound: Duration) {
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
            "stdfs: cleanup of {} did not finish in {:?}; leaving it (the mount may be wedged)",
            path.display(),
            bound
        );
    }
}

// ---------- go ----------

struct GoParsed {
    pass: u64,
    fail: u64,
    skip: u64,
    failures: Vec<(String, String)>,
}

fn parse_go_v(out: &str) -> GoParsed {
    let mut p = GoParsed {
        pass: 0,
        fail: 0,
        skip: 0,
        failures: Vec::new(),
    };
    // Track per-test log buffers keyed by top-level test name so `-test.parallel=N`
    // (`=== RUN`, `=== PAUSE`, `=== CONT`) never drops or mixes failure details.
    let mut current_top = String::new();
    let mut logs_by_test: HashMap<String, String> = HashMap::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("=== ") {
            if let Some(name) = rest
                .strip_prefix("RUN")
                .or_else(|| rest.strip_prefix("CONT"))
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let top = name.split('/').next().unwrap_or(name);
                current_top = top.to_string();
            } else if rest.starts_with("PAUSE") {
                current_top.clear();
            }
            continue;
        }
        if line.starts_with("--- ") {
            let rest = &line[4..];
            let Some((verb, tailpart)) = rest.split_once(' ') else {
                continue;
            };
            let name = tailpart.split(" (").next().unwrap_or(tailpart).trim();
            let detail = logs_by_test.remove(name).unwrap_or_default();
            match verb {
                "PASS:" => p.pass += 1,
                "SKIP:" => p.skip += 1,
                "FAIL:" => {
                    p.fail += 1;
                    if p.failures.len() < 10 {
                        p.failures.push((name.to_string(), tail(&detail, 800)));
                    }
                }
                _ => {}
            }
            if current_top == name {
                current_top.clear();
            }
            continue;
        }
        if line.starts_with(char::is_whitespace) && !line.trim().is_empty() {
            if !current_top.is_empty() {
                let buf = logs_by_test.entry(current_top.clone()).or_default();
                if buf.len() < 1200 {
                    buf.push_str(line.trim_start());
                    buf.push('\n');
                }
            }
            continue;
        }
    }
    p
}

fn copy_dir_tree(src: &Path, dst: &Path) {
    let _ = fs::create_dir_all(dst);
    let Ok(rd) = fs::read_dir(src) else { return };
    for ent in rd.flatten() {
        let p = ent.path();
        let name = ent.file_name();
        let out = dst.join(&name);
        if p.is_dir() {
            copy_dir_tree(&p, &out);
        } else if p.is_file() {
            let _ = fs::copy(&p, &out);
        }
    }
}

fn populate_go_pkg_fixtures(fixture_src: &Path, pkg: &str, wd: &Path) {
    let _ = fs::create_dir_all(wd);
    if let Ok(rd) = fs::read_dir(fixture_src) {
        for ent in rd.flatten() {
            let p = ent.path();
            let fname = ent.file_name().to_string_lossy().into_owned();
            if p.is_file() && fname.ends_with(".go") {
                let _ = fs::copy(&p, wd.join(&fname));
            } else if p.is_dir() && (fname == "testdata" || fname == "exec") {
                copy_dir_tree(&p, &wd.join(&fname));
            }
        }
    }
    if pkg == "os" {
        let _ = fs::create_dir_all(wd.join("exec"));
    }
}

fn run_go_test_bin(
    bin: &Path,
    pkg: &str,
    fixture_src: Option<&Path>,
    skip: &str,
    jobs: usize,
    timeout: u64,
    work_root: &Path,
) -> Value {
    // Preserve nested `<pkg>` path (`go/os`, `go/path/filepath`, `go/io/fs`) so
    // `filepath.TestGlob` (`../*/match.go`) and `io/fs.TestGlob` (`*/glob.go`) match on the mount.
    let wd = work_root.join("go").join(pkg);
    let _ = std::fs::create_dir_all(&wd);
    if let Some(fsrc) = fixture_src {
        populate_go_pkg_fixtures(fsrc, pkg, &wd);
    }
    let tmp = work_root.join("go/.tmp").join(pkg.replace(['/', '.'], "_"));
    let _ = std::fs::create_dir_all(&tmp);
    let par = jobs.clamp(1, 4);
    let mut rc = Command::new(bin);
    rc.arg("-test.v")
        .arg(format!("-test.parallel={par}"))
        .arg(format!("-test.timeout={timeout}s"))
        .current_dir(&wd)
        .env("TMPDIR", &tmp);
    if !skip.is_empty() {
        rc.arg(format!("-test.skip={skip}"));
    }
    let t0 = Instant::now();
    let ex = exec_with_timeout(rc, Duration::from_secs(timeout.max(1)));
    let p = parse_go_v(&ex.out);
    let ok = ex.code == Some(0) && !ex.timed_out && p.fail == 0;
    if !ok {
        warn!(
            "stdfs[go]: {} FAILED ({} pass {} fail {} skip)",
            pkg, p.pass, p.fail, p.skip
        );
    }
    info!(
        "stdfs[go]: {} {} pass={} fail={} skip={} ({}s)",
        pkg,
        if ok { "ok" } else { "fail" },
        p.pass,
        p.fail,
        p.skip,
        t0.elapsed().as_secs()
    );
    let status = if ex.timed_out {
        "timedout"
    } else if ok {
        "ok"
    } else {
        "fail"
    };
    json!({
        "name": pkg, "status": status,
        "pass": p.pass, "fail": p.fail, "skip": p.skip,
        "failures": p.failures.iter().map(|(n, t)| json!({"name": n, "tail": t})).collect::<Vec<_>>(),
        "durationMs": t0.elapsed().as_millis() as u64,
        "exitCode": ex.code,
        "tail": tail(&ex.out, 2000),
    })
}

fn run_go(_mountpoint: &Path, args: &Args, work_root: &Path, hosttmp: &Path) -> Value {
    let skip = args.go_skip.clone().unwrap_or_else(|| GO_SKIP_DEFAULT.to_string());
    // Prefer precompiled Go stdlib test binaries from an attached bundle when --go-dir is unset
    if args.go_dir.is_none()
        && let Some(bundle) = crate::bundle::ensure_extracted()
    {
        let prebuilt_dir = bundle.join("go");
        let all_present = args.go_packages.iter().all(|pkg| {
            let safe = pkg.replace(['/', '.'], "_");
            prebuilt_dir.join(format!("go_{safe}.test")).is_file()
        });
        if all_present {
            info!(
                "stdfs[go]: using precompiled test binaries from bundle {} ({} packages)",
                prebuilt_dir.display(),
                args.go_packages.len()
            );
            let items = run_parallel(args.go_packages.len(), args.jobs, |i| {
                let pkg = &args.go_packages[i];
                let safe = pkg.replace(['/', '.'], "_");
                let bin = prebuilt_dir.join(format!("go_{safe}.test"));
                let fix = prebuilt_dir.join("fixtures").join(pkg);
                run_go_test_bin(
                    &bin,
                    pkg,
                    fix.is_dir().then_some(fix.as_path()),
                    &skip,
                    args.jobs,
                    args.timeout,
                    work_root,
                )
            });
            let ok = items.iter().all(|it| it["status"] == json!("ok"));
            return json!({
                "available": true,
                "status": if ok { "ok" } else { "failed" },
                "go": "bundled-prebuilt",
                "goroot": prebuilt_dir.display().to_string(),
                "skipFilter": skip,
                "summary": {
                    "packages": items.len(),
                    "failed": items.iter().filter(|it| it["status"] != json!("ok")).count(),
                },
                "items": items,
            });
        }
    }

    let go = match &args.go_dir {
        Some(d) => {
            let g = d.join("bin/go");
            if !g.is_file() {
                return unavailable(format!(
                    "{} not found; --go-dir must be a GOROOT containing bin/go",
                    g.display()
                ));
            }
            g
        }
        None => match find_in_path("go") {
            Some(p) => p,
            None => return unavailable("no `go` on PATH; install Go or pass --go-dir (GOROOT)".into()),
        },
    };
    let Some(goroot) = cmd_stdout({
        let mut c = Command::new(&go);
        c.arg("env").arg("GOROOT");
        c
    }) else {
        return unavailable(format!("`{} env GOROOT` failed", go.display()));
    };
    let goroot_path = PathBuf::from(&goroot);
    if !goroot_path.join("src/os").is_dir() {
        return unavailable(format!("GOROOT {} has no src/os", goroot));
    }
    info!(
        "stdfs[go]: GOROOT {} ({} packages, skip filter: {:?})",
        goroot,
        args.go_packages.len(),
        skip
    );
    let items = run_parallel(args.go_packages.len(), args.jobs, |i| {
        let pkg = &args.go_packages[i];
        let safe = pkg.replace(['/', '.'], "_");
        let bin = hosttmp.join(format!("go_{safe}.test"));
        let mut cc = Command::new(&go);
        cc.args(["test", "-c", "-o"]).arg(&bin).arg(pkg).current_dir(hosttmp);
        let ex = exec_with_timeout(cc, Duration::from_secs(args.timeout.max(1)));
        if ex.spawn_err.is_some() || ex.code != Some(0) {
            return json!({
                "name": pkg, "status": "compile_error",
                "tail": tail(&ex.out, 2000), "error": ex.spawn_err,
            });
        }
        let pkg_src = goroot_path.join("src").join(pkg);
        run_go_test_bin(
            &bin,
            pkg,
            pkg_src.is_dir().then_some(pkg_src.as_path()),
            &skip,
            args.jobs,
            args.timeout,
            work_root,
        )
    });
    let ok = items.iter().all(|it| it["status"] == json!("ok"));
    json!({
        "available": true,
        "status": if ok { "ok" } else { "failed" },
        "go": go.display().to_string(),
        "goroot": goroot,
        "skipFilter": skip,
        "summary": {
            "packages": items.len(),
            "failed": items.iter().filter(|it| it["status"] != json!("ok")).count(),
        },
        "items": items,
    })
}

// ---------- python ----------

struct PyParsed {
    pass: u64,
    fail: u64,
    skip: u64,
    failures: Vec<String>,
}

fn parse_python_v(out: &str) -> PyParsed {
    let mut p = PyParsed {
        pass: 0,
        fail: 0,
        skip: 0,
        failures: Vec::new(),
    };
    for line in out.lines() {
        let Some(pos) = line.find(" ... ") else { continue };
        let id = line[..pos].trim();
        let st = line[pos + 5..].trim();
        if st.starts_with("ok") {
            p.pass += 1;
        } else if st == "FAIL" || st == "ERROR" {
            p.fail += 1;
            if p.failures.len() < 100 {
                p.failures.push(id.to_string());
            }
        } else if st.starts_with("skipped") {
            p.skip += 1;
        }
    }
    p
}

fn python_exe_for_dir(dir: &Path) -> Option<PathBuf> {
    // an installed tree: <prefix>/lib/python3.X -> <prefix>/bin/python3*;
    // a framework tree: .../Versions/3.Y/lib/python3.Y -> .../Versions/3.Y/bin/python3
    for up in ["../bin", "../../bin"] {
        let bin = dir.join(up);
        let Ok(rd) = std::fs::read_dir(&bin) else { continue };
        let mut cands: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("python3"))
                    .unwrap_or(false)
            })
            .collect();
        cands.sort();
        if let Some(p) = cands.pop() {
            return Some(p);
        }
    }
    None
}

// harness artifacts:
// - `ForkTests.test_fork`: child python re-execs with `-E` (ignoring PYTHONPATH)
// - `TestExtractionFilters.test_modes`: expects `chmod(0o6777)` to preserve `S_ISGID`,
//   which BSD/Darwin kernels strip for unprivileged users when parent dir has gid=0
// - `PosixTester.test_pipe2_c_limits`: raw `import _testcapi` for non-FS `pipe2()` C-int
//   overflow check (`_testcapi` is omitted in stripped standalone/distro CPython builds)
const PYTHON_IGNORE_DEFAULT: &[&str] = &[
    "test.test_os.ForkTests.test_fork",
    "test.test_tarfile.TestExtractionFilters.test_modes",
    "test.test_posix.PosixTester.test_pipe2_c_limits",
];

// CPython C-API overflow / pthread-warning tests that require the internal
// `_testcapi` C extension (omitted by `python-build-standalone` install_only_stripped
// and minimal distro builds) and do not exercise filesystem syscalls.
const PYTHON_IGNORE_NO_TESTCAPI: &[&str] = &[
    "test.test_fcntl.TestFcntl.test_fcntl_bad_file_overflow",
    "test.test_fcntl.TestFcntl.test_flock_overflow",
    "test.test_fileio.COtherFileTests.testInvalidFd_overflow",
    "test.test_fileio.PyOtherFileTests.testInvalidFd_overflow",
    "test.test_os.ForkTests.test_fork_warns_when_non_python_thread_exists",
];

fn run_python(_mountpoint: &Path, args: &Args, work_root: &Path) -> Value {
    let mut chosen: Option<(PathBuf, PathBuf)> = None;
    if args.python_exe.is_none()
        && args.python_dir.is_none()
        && let Some(bundle) = crate::bundle::ensure_extracted()
    {
        let b_py = bundle.join("python");
        let b_exe = ["bin/python3", "bin/python3.12"]
            .iter()
            .map(|rel| b_py.join(rel))
            .find(|p| p.is_file());
        let b_lib = ["lib/python3.12", "Lib"]
            .iter()
            .map(|rel| b_py.join(rel))
            .find(|p| p.join("test").is_dir());
        if let (Some(exe), Some(lib)) = (b_exe, b_lib) {
            chosen = Some((exe, lib));
        }
    }

    if chosen.is_none() {
        let candidates: Vec<PathBuf> = if let Some(p) = &args.python_exe {
            vec![p.clone()]
        } else if let Some(d) = &args.python_dir {
            python_exe_for_dir(d)
                .into_iter()
                .chain(find_all_in_path("python3"))
                .collect()
        } else {
            find_all_in_path("python3")
        };
        let Some(first_exe) = candidates.first().cloned() else {
            return unavailable("no python3 on PATH; pass --python-exe".into());
        };
        if let Some(d) = &args.python_dir {
            chosen = Some((first_exe.clone(), d.clone()));
        } else {
            for cand in &candidates {
                if let Some(stdlib) = cmd_stdout({
                    let mut c = Command::new(cand);
                    c.args(["-c", "import sysconfig;print(sysconfig.get_path('stdlib'))"]);
                    c
                }) {
                    let p = PathBuf::from(&stdlib);
                    if p.join("test").is_dir() {
                        chosen = Some((cand.clone(), p));
                        break;
                    }
                }
            }
            if chosen.is_none()
                && let Some(bundle_lib) = crate::bundle::ensure_extracted()
                    .map(|b| b.join("python/Lib"))
                    .filter(|p| p.join("test").is_dir())
            {
                chosen = Some((first_exe.clone(), bundle_lib));
            }
        }
        if chosen.is_none() {
            return unavailable(format!(
                "python3 ({}) ships without the bundled `test` package; pass --python-dir (a CPython Lib/ dir containing test/)",
                first_exe.display()
            ));
        }
    }
    let (exe, test_lib) = chosen.unwrap();
    if !test_lib.join("test").is_dir() {
        return unavailable(format!("{} contains no test/ package", test_lib.display()));
    }
    let modules: Vec<String> = args
        .python_modules
        .iter()
        .map(|m| {
            if m.starts_with("test_") {
                m.clone()
            } else {
                format!("test_{m}")
            }
        })
        .collect();
    let mut pyenv = test_lib.display().to_string();
    if let Ok(existing) = std::env::var("PYTHONPATH") {
        if !existing.is_empty() {
            pyenv.push(':');
            pyenv.push_str(&existing);
        }
    }
    // regrtest gained --ignore-list in 3.12, earlier versions call it --ignorefile
    let help = cmd_stdout({
        let mut c = Command::new(&exe);
        c.args(["-m", "test", "--help"]).env("PYTHONPATH", &pyenv);
        c
    })
    .unwrap_or_default();
    let ignore_flag = if help.contains("--ignore-list") {
        "--ignore-list"
    } else if help.contains("--ignorefile") {
        "--ignorefile"
    } else {
        ""
    };
    let has_testcapi = Command::new(&exe)
        .args(["-c", "import _testcapi"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let mut patterns: Vec<String> = PYTHON_IGNORE_DEFAULT.iter().map(|s| s.to_string()).collect();
    if !has_testcapi {
        patterns.extend(PYTHON_IGNORE_NO_TESTCAPI.iter().map(|s| s.to_string()));
    }
    patterns.extend(args.python_ignore.iter().cloned());
    let ignore_file = work_root.join(".python-ignore.txt");
    if !ignore_flag.is_empty() && !patterns.is_empty() {
        let _ = fs::write(&ignore_file, patterns.join("\n"));
    }
    info!(
        "stdfs[python]: {} with {} from {} ({} modules)",
        exe.display(),
        test_lib.join("test").display(),
        test_lib.display(),
        modules.len()
    );
    let items = run_parallel(modules.len(), args.jobs, |i| {
        let m = &modules[i];
        let mod_exists = test_lib.join("test").join(format!("{m}.py")).is_file()
            || test_lib.join("test").join(m).join("__init__.py").is_file();
        if !mod_exists {
            warn!(
                "stdfs[python]: module {m} missing in {}",
                test_lib.join("test").display()
            );
            return json!({
                "name": m,
                "status": "missing",
                "failures": [format!("module {m} not found in {}", test_lib.join("test").display())],
                "tail": format!("module {m} not found in {}", test_lib.join("test").display()),
            });
        }
        let wd = work_root.join("python").join(m);
        let _ = std::fs::create_dir_all(&wd);
        let mut c = Command::new(&exe);
        c.args(["-B", "-u", "-m", "test", "-v", m])
            .current_dir(&wd)
            .env("TMPDIR", &wd)
            .env("PYTHONPATH", &pyenv);
        if !ignore_flag.is_empty() && !patterns.is_empty() {
            c.arg(ignore_flag).arg(&ignore_file);
        }
        let t0 = Instant::now();
        let ex = exec_with_timeout(c, Duration::from_secs(args.timeout.max(1)));
        let p = parse_python_v(&ex.out);
        let ok = ex.code == Some(0) && !ex.timed_out && p.fail == 0;
        if !ok {
            warn!(
                "stdfs[python]: {} FAILED ({} pass {} fail {} skip)",
                m, p.pass, p.fail, p.skip
            );
        }
        info!(
            "stdfs[python]: {} {} pass={} fail={} skip={} ({}s)",
            m,
            if ok { "ok" } else { "fail" },
            p.pass,
            p.fail,
            p.skip,
            t0.elapsed().as_secs()
        );
        let status = if ex.timed_out {
            "timedout"
        } else if ex.code != Some(0) {
            "fail"
        } else if ok {
            "ok"
        } else {
            "fail"
        };
        json!({
            "name": m, "status": status,
            "pass": p.pass, "fail": p.fail, "skip": p.skip,
            "failures": p.failures,
            "durationMs": t0.elapsed().as_millis() as u64,
            "exitCode": ex.code,
            "tail": tail(&ex.out, 2000),
        })
    });
    let ok = items.iter().all(|it| it["status"] == json!("ok"));
    json!({
        "available": true,
        "status": if ok { "ok" } else { "failed" },
        "exe": exe.display().to_string(),
        "testLib": test_lib.display().to_string(),
        "summary": {
            "modules": items.len(),
            "missing": items.iter().filter(|it| it["status"] == json!("missing")).count(),
            "failed": items.iter().filter(|it| it["status"] != json!("ok")).count(),
        },
        "items": items,
    })
}

// ---------- node ----------

struct TapParsed {
    pass: u64,
    fail: u64,
    failures: Vec<(String, String)>,
}

fn parse_tap(out: &str) -> TapParsed {
    let mut p = TapParsed {
        pass: 0,
        fail: 0,
        failures: Vec::new(),
    };
    let lines: Vec<&str> = out.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("ok ") {
            p.pass += 1;
        } else if line.starts_with("not ok ") {
            p.fail += 1;
            let name = line
                .trim_start_matches("not ok ")
                .split_once(" - ")
                .map(|(_, n)| n.to_string())
                .unwrap_or_else(|| line.trim_start_matches("not ok ").to_string());
            if p.failures.len() < 10 {
                let mut det = String::new();
                let mut j = i + 1;
                while j < lines.len() && (lines[j].starts_with(' ') || lines[j].starts_with('\t')) {
                    det.push_str(lines[j]);
                    det.push('\n');
                    if det.len() > 800 {
                        break;
                    }
                    j += 1;
                }
                p.failures.push((name, tail(&det, 800)));
            } else {
                p.failures.push((name, String::new()));
            }
        }
        i += 1;
    }
    p
}

// Node test files excluded by default: checkout-relative tests, host `/proc` readers,
// and pure JS argument-type-validation files (`ERR_INVALID_ARG_TYPE`) that perform
// zero filesystem syscalls.
const NODE_EXCLUDE_DEFAULT: &[&str] = &[
    "test-fs-realpath-native.js",
    "test-fs-assert-encoding-error.js",
    "test-fs-buffertype-writesync.js",
    "test-fs-chown-type-check.js",
    "test-fs-constants.js",
    "test-fs-fchmod.js",
    "test-fs-fchown.js",
    "test-fs-null-bytes.js",
    "test-fs-promises-exists.js",
    "test-fs-read-file-assert-encoding.js",
    "test-fs-read-file-sync-hostname.js",
    "test-fs-read-stream-patch-open.js",
    "test-fs-readlink-type-check.js",
    "test-fs-rename-type-check.js",
    "test-fs-rmdir-type-check.js",
    "test-fs-stream-options.js",
    "test-fs-sync-fd-leak.js",
    "test-fs-timestamp-parsing-error.js",
    "test-fs-unlink-type-check.js",
    "test-fs-util-validateoffsetlength.js",
    "test-fs-watch-recursive-validate.js",
    "test-fs-write-no-fd.js",
    "test-fs-write-stream-throw-type-error.js",
];

fn is_node_stat_watchfile_test(name: &str) -> bool {
    name.starts_with("test-fs-watchfile") || name == "test-fs-watch-file-enoent-after-deletion.js"
}

fn is_node_event_watch_test(name: &str) -> bool {
    name.contains("watch") && !is_node_stat_watchfile_test(name)
}

impl Args {
    pub fn include_node_event_watch(&self) -> bool {
        if self.node_watch {
            true
        } else if self.no_node_watch {
            false
        } else {
            cfg!(target_os = "linux")
        }
    }
}

fn run_node(_mountpoint: &Path, args: &Args, work_root: &Path) -> Value {
    let bundled_node_root = crate::bundle::ensure_extracted().map(|b| b.join("node"));
    let node_cand = bundled_node_root
        .as_ref()
        .map(|r| r.join("bin/node"))
        .filter(|p| p.is_file())
        .or_else(|| find_in_path("node"));
    let Some(node) = node_cand else {
        return unavailable("no `node` on PATH; install Node.js".into());
    };
    if let Some(v) = cmd_stdout({
        let mut c = Command::new(&node);
        c.arg("--version");
        c
    }) {
        let major: usize = v
            .trim_start_matches('v')
            .split('.')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if major < 18 {
            return unavailable(format!("node {v} lacks the --test runner (need >= 18)"));
        }
    }
    // installed node binaries never ship the test suite; check --node-dir, then
    // an attached bundle, then a from-source tree (node/out/Release/node)
    let dir: PathBuf = match &args.node_dir {
        Some(d) => d.clone(),
        None => {
            let from_bundle =
                bundled_node_root.filter(|r| r.join("test/parallel").is_dir() && r.join("test/common").is_dir());
            let cand = node
                .parent()
                .unwrap_or(Path::new("/"))
                .ancestors()
                .nth(2)
                .map(|p| p.join("test"));
            let from_tree = cand
                .filter(|c| c.join("parallel").is_dir())
                .and_then(|c| c.parent().map(Path::to_path_buf));
            match from_bundle.or(from_tree) {
                Some(c) => c,
                None => {
                    return unavailable(
                        "node binary distributions don't ship the test suite; pass --node-dir pointing at a nodejs/node checkout (containing test/parallel)".into(),
                    )
                }
            }
        }
    };
    if !dir.join("test/parallel").is_dir() || !dir.join("test/common").is_dir() {
        return unavailable(format!(
            "{} is not a nodejs/node checkout (needs test/parallel and test/common)",
            dir.display()
        ));
    }
    let include_event_watch = args.include_node_event_watch();
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join("test/parallel"))
        .map_err(|e| format!("{}: {e}", dir.join("test/parallel").display()))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p
                .file_name()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_default();
            n.starts_with("test-fs-")
                && n.ends_with(".js")
                && (include_event_watch || !is_node_event_watch_test(&n))
                && !NODE_EXCLUDE_DEFAULT.iter().any(|x| n.contains(x))
        })
        .collect();
    files.sort();
    if let Some(f) = &args.node_filter {
        files.retain(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().contains(f.as_str()))
                .unwrap_or(false)
        });
    }
    if files.is_empty() {
        return unavailable("no test-fs-*.js files matched in test/parallel".into());
    }
    info!(
        "stdfs[node]: {} {} fs test files (watchFile=on, fs.watch={})",
        node.display(),
        files.len(),
        if include_event_watch { "on" } else { "off" }
    );
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    let items = run_parallel(files.len(), args.jobs, |i| {
        let f = &files[i];
        let stem = names[i].trim_end_matches(".js").to_string();
        let wd = work_root.join("node").join(&stem);
        let _ = std::fs::create_dir_all(&wd);
        let mut c = Command::new(&node);
        c.args(["--test", "--test-reporter=tap"])
            .arg(f)
            .current_dir(&wd)
            .env("NODE_TEST_DIR", &wd)
            .env("TMPDIR", &wd);
        let t0 = Instant::now();
        let ex = exec_with_timeout(c, Duration::from_secs(args.timeout.max(1)));
        let p = parse_tap(&ex.out);
        let ok = ex.code == Some(0) && !ex.timed_out && p.fail == 0;
        if !ok {
            warn!("stdfs[node]: {} FAILED ({} pass {} fail)", names[i], p.pass, p.fail);
        }
        info!(
            "stdfs[node]: {} {} pass={} fail={} ({}s)",
            names[i],
            if ok { "ok" } else { "fail" },
            p.pass,
            p.fail,
            t0.elapsed().as_secs()
        );
        let status = if ex.timed_out {
            "timedout"
        } else if ok {
            "ok"
        } else {
            "fail"
        };
        json!({
            "name": names[i], "status": status,
            "pass": p.pass, "fail": p.fail,
            "failures": p.failures.iter().map(|(n, t)| json!({"name": n, "tail": t})).collect::<Vec<_>>(),
            "durationMs": t0.elapsed().as_millis() as u64,
            "exitCode": ex.code,
            "tail": tail(&ex.out, 2000),
        })
    });
    let ok = items.iter().all(|it| it["status"] == json!("ok"));
    json!({
        "available": true,
        "status": if ok { "ok" } else { "failed" },
        "node": node.display().to_string(),
        "checkout": dir.display().to_string(),
        "watchFile": true,
        "eventWatch": include_event_watch,
        "summary": {
            "files": items.len(),
            "failed": items.iter().filter(|it| it["status"] != json!("ok")).count(),
        },
        "items": items,
    })
}

// ---------- rust ----------

// reads one attribute starting at lines[j] (may span lines), returning its
// text joined onto one line and the index of the first line after it
fn read_attr(lines: &[&str], mut j: usize) -> (String, usize) {
    let mut text = String::new();
    let mut depth: i32 = 0;
    while j < lines.len() {
        for ch in lines[j].chars() {
            match ch {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(lines[j].trim());
        j += 1;
        if depth <= 0 {
            break;
        }
    }
    (text, j)
}

// rewrites std's `crate::` paths onto std, drops the tests that use
// std-internal APIs, and returns the body plus each test fn with its cfg attrs
fn transform_rust_tests(src: &str) -> (String, Vec<(Vec<String>, String)>, Vec<String>) {
    let lines: Vec<&str> = src.lines().collect();
    // pass 1: locate every #[test], its attrs (cfg attrs may sit before or
    // after the #[test] line and may span lines), fn name and body extent
    let mut skip: Vec<(usize, usize)> = Vec::new();
    let mut drops: Vec<usize> = Vec::new();
    let mut pubs: Vec<usize> = Vec::new();
    let mut tests: Vec<(Vec<String>, String)> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "#[test]" {
            i += 1;
            continue;
        }
        // attrs directly above #[test] (blank lines don't detach them)
        let mut back = i;
        while back > 0 {
            let t = lines[back - 1].trim();
            if t.is_empty() || t.starts_with("#[") || t.starts_with("//") {
                back -= 1;
            } else {
                break;
            }
        }
        let mut back_attrs: Vec<String> = lines[back..i]
            .iter()
            .map(|l| l.trim().to_string())
            .filter(|l| l.starts_with("#["))
            .collect();
        let mut attrs: Vec<String> = Vec::new();
        let mut j = i + 1;
        let mut name = String::new();
        while j < lines.len() {
            let t = lines[j].trim();
            if t.starts_with("#[") {
                let (text, nj) = read_attr(&lines, j);
                attrs.push(text);
                j = nj;
                continue;
            }
            if t.starts_with("//") || t.starts_with("/*") || t.is_empty() {
                j += 1;
                continue;
            }
            if t.starts_with("fn ") || t.starts_with("pub fn ") || t.starts_with("async fn ") {
                name = t
                    .split("fn ")
                    .nth(1)
                    .unwrap_or("")
                    .split(['(', '<', ':'])
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
            }
            break;
        }
        if name.is_empty() {
            // unrecognized declaration shape: leave this fn untouched and
            // unregistered rather than emitting a broken table entry
            warn!("stdfs[rust]: could not parse test fn after #[test] at line {}", i + 1);
            i += 1;
            continue;
        }
        back_attrs.extend(attrs);
        // compute the fn block extent first so exclusion can inspect the body
        // (braces inside format strings come in balanced pairs, so they cancel
        // out in the net brace depth)
        let mut depth: i32 = 0;
        let mut seen_open = false;
        let mut k = j;
        while k < lines.len() {
            for ch in lines[k].chars() {
                if ch == '{' {
                    depth += 1;
                    seen_open = true;
                } else if ch == '}' {
                    depth -= 1;
                }
            }
            k += 1;
            if seen_open && depth <= 0 {
                break;
            }
        }
        // named exclusions plus a body scan: any test touching `Dir::` uses
        // std's internal dirfd type, which keeps growing new users across
        // releases (test_dir_metadata joined in 1.98) — a body rule beats
        // enumerating names
        let uses_internal_dir = lines[j..k]
            .iter()
            .any(|l| l.contains("Dir::") || l.contains("set_permissions_nofollow"));
        if RUST_EXCLUDE.iter().any(|(n, _)| *n == name) || uses_internal_dir {
            excluded.push(name.clone());
            skip.push((back, k - 1));
            i = k;
            continue;
        }
        drops.push(i);
        pubs.push(j);
        tests.push((back_attrs, name));
        i = k;
        continue;
    }
    // pass 2: emit, dropping excluded blocks and the #[test] lines, making
    // each test fn visible to the generated TESTS table
    let mut out = String::new();
    for (idx, line) in lines.iter().enumerate() {
        if skip.iter().any(|(a, b)| idx >= *a && idx <= *b) || drops.contains(&idx) {
            continue;
        }
        let mut l = line.to_string();
        if pubs.contains(&idx) && l.trim_start().starts_with("fn ") {
            if let Some(p) = l.find("fn ") {
                l.insert_str(p, "pub ");
            }
        }
        if l.trim() == "use super::Dir;" {
            continue;
        }
        l = l.replace("crate::", "std::");
        l = l.replace("std::test_helpers", "crate::shim::test_helpers");
        l = l.replace("use std::io::{BorrowedBuf, ", "use std::io::{");
        if l.trim() == "use rand::RngCore;" {
            l = "use crate::shim::rand::RngCore;".to_string();
        }
        if l.trim() == "use std::{assert_matches, env, io, str, thread};" {
            l = "use crate::shim::assert_matches;\nuse std::{env, io, str, thread};".to_string();
        }
        out.push_str(&l);
        out.push('\n');
    }
    (out, tests, excluded)
}

macro_rules! emit_and_str_shim {
    ($($body:tt)*) => {
        const SHIM: &str = stringify!($($body)*);

        #[allow(warnings, unsafe_op_in_unsafe_fn, dead_code, unused_imports, non_snake_case)]
        mod embedded_rust {
            $($body)*
            include!(concat!(env!("OUT_DIR"), "/stdfs_rust_embedded.rs"));
        }
    };
}

emit_and_str_shim! {
    pub mod shim {
        macro_rules! assert_matches {
            ($left:expr, $($pattern:pat_param)|+ $(if $guard:expr)?) => {
                match $left {
                    $($pattern)|+ $(if $guard)? => {}
                    left => panic!(
                        "assertion failed: `{:?}` does not match any of the expected patterns",
                        left
                    ),
                }
            };
            ($left:expr, $($pattern:pat_param)|+ $(if $guard:expr)?, $($arg:tt)+) => {
                match $left {
                    $($pattern)|+ $(if $guard)? => {}
                    left => panic!(
                        "assertion failed: `{:?}` does not match any of the expected patterns: {}",
                        left,
                        format_args!($($arg)+)
                    ),
                }
            };
        }
        pub(crate) use assert_matches;
        pub mod rand {
            use std::sync::atomic::{AtomicU64, Ordering};
            pub trait RngCore {
                fn fill_bytes(&mut self, dest: &mut [u8]);
            }
            pub struct TestRng(u64);
            impl TestRng {
                fn next_u64(&mut self) -> u64 {
                    let mut x = self.0;
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    self.0 = x;
                    x
                }
            }
            impl RngCore for TestRng {
                fn fill_bytes(&mut self, dest: &mut [u8]) {
                    for chunk in dest.chunks_mut(8) {
                        let b = self.next_u64().to_le_bytes();
                        chunk.copy_from_slice(&b[..chunk.len()]);
                    }
                }
            }
            static RNG_SEQ: AtomicU64 = AtomicU64::new(1);
            pub fn test_rng() -> TestRng {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0x9E37_79B9_7F4A_7C15);
                let salt = RNG_SEQ.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
                TestRng((nanos ^ salt) | 1)
            }
        }
        pub mod test_helpers {
            use std::ops::Deref;
            use std::path::{Path, PathBuf};
            use std::sync::atomic::{AtomicU64, Ordering};
            pub use super::rand::test_rng;
            pub struct TempDir(PathBuf);
            impl TempDir {
                pub fn path(&self) -> &Path {
                    &self.0
                }
            }
            impl Deref for TempDir {
                type Target = Path;
                fn deref(&self) -> &Path {
                    &self.0
                }
            }
            impl Drop for TempDir {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
            static SEQ: AtomicU64 = AtomicU64::new(0);
            pub fn tmpdir() -> TempDir {
                let base = std::env::temp_dir().join(format!(
                    "fstest-ruststd-{}-{}",
                    std::process::id(),
                    SEQ.fetch_add(1, Ordering::SeqCst)
                ));
                std::fs::create_dir(&base).expect("tmpdir create");
                TempDir(base)
            }
        }
    }
}

fn emit_rust_res_line(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Hidden child entrypoint `fstest __stdfs-rust [--jobs <N>] [--skip <pat>]...`
/// running the compile-time embedded `std::fs` test suite concurrently across
/// `N` workers with zero target-host toolchain dependency.
pub fn rust_helper_main(argv: &[String]) -> i32 {
    let mut skips: Vec<String> = Vec::new();
    let mut jobs: usize = std::env::var("STDFS_RUST_JOBS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(default_jobs);
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--skip"
            && let Some(p) = argv.get(i + 1)
        {
            if !p.is_empty() {
                skips.push(p.clone());
            }
            i += 2;
            continue;
        }
        if argv[i] == "--jobs"
            && let Some(j) = argv.get(i + 1)
        {
            if let Ok(n) = j.parse::<usize>() {
                jobs = n.max(1);
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    let per_test: u64 = std::env::var("STDFS_TEST_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let _ = run_parallel(embedded_rust::EMBEDDED_TESTS.len(), jobs.max(1), |idx| {
        let (name, func, ignored) = &embedded_rust::EMBEDDED_TESTS[idx];
        if *ignored {
            emit_rust_res_line(&format!("RES\t{name}\tskipped\t#[ignore]"));
            return;
        }
        if skips.iter().any(|p| name.contains(p.as_str())) {
            emit_rust_res_line(&format!("RES\t{name}\tskipped\t--rust-skip"));
            return;
        }
        let n = *name;
        let f = *func;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|p| {
                    if let Some(s) = p.downcast_ref::<&str>() {
                        (*s).to_string()
                    } else if let Some(s) = p.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "panic".to_string()
                    }
                });
                let _ = tx.send(r);
            })
            .expect("spawn test thread");
        let line = match rx.recv_timeout(std::time::Duration::from_secs(per_test)) {
            Ok(Ok(())) => format!("RES\t{n}\tok\t"),
            Ok(Err(msg)) => format!("RES\t{n}\tfail\t{}", msg.replace('\n', " ")),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                format!("RES\t{n}\ttimedout\tno result in {per_test}s (deadlock or blocked syscall)")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                format!("RES\t{n}\tfail\tthread panicked or aborted")
            }
        };
        emit_rust_res_line(&line);
    });
    emit_rust_res_line("DONE");
    std::process::exit(0);
}

fn collect_rust_runner_report(
    ex: Exec,
    total_tests: usize,
    elapsed: Duration,
    rustc_desc: String,
    source_desc: String,
    excluded: Vec<Value>,
) -> Value {
    let (mut pass, mut fail, mut skip, mut per_test_timeout) = (0u64, 0u64, 0u64, 0u64);
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut seen = 0u64;
    for line in ex.out.lines() {
        let mut it = line.splitn(4, '\t');
        if it.next() != Some("RES") {
            continue;
        }
        let Some(name) = it.next() else { continue };
        let Some(status) = it.next() else { continue };
        let msg = it.next().unwrap_or("");
        seen += 1;
        match status {
            "ok" => pass += 1,
            "skipped" => skip += 1,
            "timedout" => {
                per_test_timeout += 1;
                fail += 1;
                if failures.len() < 20 {
                    failures.push((name.to_string(), format!("[timedout] {}", tail(msg, 400))));
                }
            }
            _ => {
                fail += 1;
                if failures.len() < 20 {
                    failures.push((name.to_string(), tail(msg, 500)));
                }
            }
        }
    }
    let ok = ex.code == Some(0) && !ex.timed_out && fail == 0;
    if !ok {
        warn!("stdfs[rust]: FAILED ({pass} pass {fail} fail {skip} skip {per_test_timeout} timedout)");
    }
    info!(
        "stdfs[rust]: {} pass={} fail={} skip={} timedout={} ({}s)",
        if ok { "ok" } else { "fail" },
        pass,
        fail,
        skip,
        per_test_timeout,
        elapsed.as_secs()
    );
    let status = if ex.timed_out {
        "timedout"
    } else if ok {
        "ok"
    } else {
        "failed"
    };
    json!({
        "available": true,
        "status": status,
        "rustc": rustc_desc,
        "source": source_desc,
        "excluded": excluded,
        "summary": {
            "total": total_tests,
            "reported": seen,
            "pass": pass,
            "fail": fail,
            "skip": skip,
            "timedout": if ex.timed_out { (total_tests as u64).saturating_sub(seen) } else { per_test_timeout },
        },
        "failures": failures.iter().map(|(n, t)| json!({"name": n, "tail": t})).collect::<Vec<_>>(),
        "durationMs": elapsed.as_millis() as u64,
        "tail": tail(&ex.out, 2000),
    })
}

fn run_rust(_mountpoint: &Path, args: &Args, work_root: &Path, hosttmp: &Path) -> Value {
    let per_test_env = std::env::var("STDFS_TEST_TIMEOUT").unwrap_or_else(|_| args.rust_test_timeout.to_string());
    let jobs_str = args.jobs.max(1).to_string();

    // Fast, zero-dependency path: run the std::fs suite compiled directly into this binary
    // unless the caller explicitly asked for an external `--rust-src-dir`.
    if args.rust_src_dir.is_none() && embedded_rust::EMBEDDED_AVAILABLE {
        let Ok(exe) = std::env::current_exe() else {
            return unavailable("cannot resolve current_exe for embedded rust std::fs runner".into());
        };
        let wd = work_root.join("rust");
        let _ = std::fs::create_dir_all(&wd);
        let mut rc = Command::new(&exe);
        rc.arg("__stdfs-rust").arg("--jobs").arg(&jobs_str);
        for sk in &args.rust_skip {
            if !sk.is_empty() {
                rc.arg("--skip").arg(sk);
            }
        }
        rc.current_dir(&wd)
            .env("TMPDIR", &wd)
            .env("RUST_BACKTRACE", "0")
            .env("STDFS_RUST_JOBS", &jobs_str)
            .env("STDFS_TEST_TIMEOUT", &per_test_env);
        info!(
            "stdfs[rust]: running {} embedded std::fs tests across {} jobs (zero external toolchain)...",
            embedded_rust::EMBEDDED_TESTS.len(),
            args.jobs.max(1)
        );
        let t0 = Instant::now();
        let ex = exec_with_timeout(rc, Duration::from_secs(args.timeout.max(1)));
        let excluded_json: Vec<Value> = embedded_rust::EMBEDDED_EXCLUDED
            .iter()
            .map(|(n, r)| json!({"name": n, "reason": r}))
            .collect();
        return collect_rust_runner_report(
            ex,
            embedded_rust::EMBEDDED_TESTS.len(),
            t0.elapsed(),
            "embedded-at-build".into(),
            embedded_rust::EMBEDDED_SOURCE.into(),
            excluded_json,
        );
    }

    let Some(rustc) = find_in_path("rustc").or_else(|| std::env::var("RUSTC").ok().map(PathBuf::from)) else {
        return unavailable("no rustc on PATH; install Rust or pass --rust-src-dir".into());
    };
    let libdir: PathBuf = match &args.rust_src_dir {
        Some(d) => d.clone(),
        None => {
            let Some(sysroot) = cmd_stdout({
                let mut c = Command::new(&rustc);
                c.arg("--print").arg("sysroot");
                c
            }) else {
                return unavailable(format!("`{} --print sysroot` failed", rustc.display()));
            };
            PathBuf::from(sysroot).join("lib/rustlib/src/rust/library")
        }
    };
    let tests_rs = libdir.join("std/src/fs/tests.rs");
    if !tests_rs.is_file() {
        return unavailable(format!(
            "no rust-src sources at {}; run `rustup component add rust-src` or pass --rust-src-dir",
            tests_rs.display()
        ));
    }
    let src = std::fs::read_to_string(&tests_rs)
        .map_err(|e| format!("{}: {e}", tests_rs.display()))
        .unwrap();
    let (body, mut tests, mut excluded) = transform_rust_tests(&src);
    let skip_pats: Vec<&String> = args.rust_skip.iter().filter(|s| !s.is_empty()).collect();
    if !skip_pats.is_empty() {
        let mut dropped: Vec<String> = Vec::new();
        tests.retain(|(_, name)| {
            if skip_pats.iter().any(|p| name.contains(p.as_str())) {
                dropped.push(name.clone());
                false
            } else {
                true
            }
        });
        for n in &dropped {
            info!("stdfs[rust]: skipping {n} (--rust-skip)");
        }
        excluded.extend(dropped);
    }
    if tests.is_empty() {
        return unavailable(format!("no #[test] fns found in {}", tests_rs.display()));
    }
    let mut runner = String::new();
    runner.push_str("#![allow(warnings)]\n");
    runner.push_str(SHIM);
    runner.push_str("\nmod tests_impl {\n");
    runner.push_str(&body);
    runner.push_str("}\n\nstatic TESTS: &[(&str, fn(), bool)] = &[\n");
    for (attrs, name) in &tests {
        for a in attrs.iter().filter(|a| a.starts_with("#[cfg(")) {
            runner.push_str("    ");
            runner.push_str(a);
            runner.push('\n');
        }
        let ignored = if attrs.iter().any(|a| a.starts_with("#[ignore")) {
            "true"
        } else {
            "false"
        };
        runner.push_str(&format!("    ({name:?}, tests_impl::{name}, {ignored}),\n"));
    }
    runner.push_str("];\n\n");
    runner.push_str(
        r#"fn emit(line: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn main() {
    let filter = std::env::args().nth(1);
    let per_test: u64 = std::env::var("STDFS_TEST_TIMEOUT").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(120);
    let jobs: usize = std::env::var("STDFS_RUST_JOBS").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(4).max(1);
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| loop {
                let idx = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if idx >= TESTS.len() {
                    break;
                }
                let (name, func, ignored) = &TESTS[idx];
                if *ignored {
                    emit(&format!("RES\t{name}\tskipped\t#[ignore]"));
                    continue;
                }
                if let Some(f) = &filter {
                    if !name.contains(f.as_str()) {
                        emit(&format!("RES\t{name}\tskipped\t"));
                        continue;
                    }
                }
                let n = *name;
                let fn_ptr = *func;
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::Builder::new()
                    .stack_size(8 * 1024 * 1024)
                    .spawn(move || {
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(fn_ptr)).map_err(|p| {
                            if let Some(s) = p.downcast_ref::<&str>() {
                                (*s).to_string()
                            } else if let Some(s) = p.downcast_ref::<String>() {
                                s.clone()
                            } else {
                                "panic".to_string()
                            }
                        });
                        let _ = tx.send(r);
                    })
                    .expect("spawn test thread");
                let line = match rx.recv_timeout(std::time::Duration::from_secs(per_test)) {
                    Ok(Ok(())) => format!("RES\t{n}\tok\t"),
                    Ok(Err(msg)) => format!("RES\t{n}\tfail\t{}", msg.replace('\n', " ")),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        format!("RES\t{n}\ttimedout\tno result in {per_test}s (deadlock or blocked syscall)")
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        format!("RES\t{n}\tfail\tthread panicked or aborted")
                    }
                };
                emit(&line);
            });
        }
    });
    emit("DONE");
    std::process::exit(0);
}
"#,
    );
    let runner_rs = hosttmp.join("ruststd_runner.rs");
    let runner_bin = hosttmp.join("ruststd_runner");
    if let Err(e) = std::fs::write(&runner_rs, &runner) {
        return unavailable(format!("write {}: {e}", runner_rs.display()));
    }
    info!(
        "stdfs[rust]: {} ({}) -> {} tests ({} excluded); compiling with {}...",
        tests_rs.display(),
        "std/src/fs/tests.rs",
        tests.len(),
        excluded.len(),
        rustc.display()
    );
    let mut cc = Command::new(&rustc);
    cc.arg("--edition=2024")
        .args(["-C", "opt-level=1", "-C", "debuginfo=0"])
        .arg("-o")
        .arg(&runner_bin)
        .arg(&runner_rs)
        .current_dir(hosttmp);
    let ex = exec_with_timeout(cc, Duration::from_secs(args.timeout.max(60)));
    if ex.code != Some(0) {
        return json!({
            "available": true,
            "status": "failed",
            "rustc": rustc.display().to_string(),
            "source": tests_rs.display().to_string(),
            "reason": "generated runner failed to compile (upstream tests.rs uses APIs this rewrite doesn't handle)",
            "tail": tail(&ex.out, 4000),
        });
    }
    let wd = work_root.join("rust");
    let _ = std::fs::create_dir_all(&wd);
    let mut rc = Command::new(&runner_bin);
    rc.current_dir(&wd)
        .env("TMPDIR", &wd)
        .env("RUST_BACKTRACE", "0")
        .env("STDFS_RUST_JOBS", &jobs_str)
        .env("STDFS_TEST_TIMEOUT", &per_test_env);
    let t0 = Instant::now();
    let ex = exec_with_timeout(rc, Duration::from_secs(args.timeout.max(1)));
    let excluded_json: Vec<Value> = RUST_EXCLUDE
        .iter()
        .filter(|(n, _)| excluded.iter().any(|x| x == *n))
        .map(|(n, r)| json!({"name": n, "reason": r}))
        .collect();
    collect_rust_runner_report(
        ex,
        tests.len(),
        t0.elapsed(),
        rustc.display().to_string(),
        tests_rs.display().to_string(),
        excluded_json,
    )
}

// ---------- entry ----------

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    for l in &args.lang {
        if !matches!(l.as_str(), "go" | "python" | "node" | "rust") {
            return Err(format!("unknown language {l:?} (go|python|node|rust)"));
        }
    }
    let work_root = mountpoint.join(&args.work_dir);
    fs::create_dir_all(&work_root).map_err(|e| format!("{}: {e}", work_root.display()))?;
    let hosttmp = std::env::temp_dir().join(format!("fstest-stdfs-{}", std::process::id()));
    fs::create_dir_all(&hosttmp).map_err(|e| format!("{}: {e}", hosttmp.display()))?;
    let started = SystemTime::now();
    let mut languages = serde_json::Map::new();
    for lang in &args.lang {
        let t0 = Instant::now();
        let v = match lang.as_str() {
            "go" => run_go(mountpoint, args, &work_root, &hosttmp),
            "python" => run_python(mountpoint, args, &work_root),
            "node" => run_node(mountpoint, args, &work_root),
            "rust" => run_rust(mountpoint, args, &work_root, &hosttmp),
            _ => unreachable!(),
        };
        info!("stdfs: {lang} done in {}s", t0.elapsed().as_secs());
        languages.insert(lang.clone(), v);
    }
    // best-effort, bounded cleanup of the on-mount work tree unless `--keep` / `FSTEST_KEEP_TMP=1`.
    let keep = args.keep || std::env::var("FSTEST_KEEP_TMP").map(|v| v == "1").unwrap_or(false);
    if keep {
        info!(
            "stdfs: keeping work_root={} and hosttmp={}",
            work_root.display(),
            hosttmp.display()
        );
    } else {
        remove_dir_all_bounded(&work_root, Duration::from_secs(20));
        let _ = fs::remove_dir_all(&hosttmp);
    }
    let any_failed = languages
        .values()
        .any(|v| v.get("available").and_then(Value::as_bool).unwrap_or(false) && v.get("status") != Some(&json!("ok")));
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    let report = json!({
        "suite": "stdfs",
        "host": host,
        "date": iso8601(started),
        "top": mountpoint.display().to_string(),
        "params": {
            "langs": args.lang,
            "timeout_sec": args.timeout,
            "jobs": args.jobs,
            "keep": args.keep,
        },
        "status": if any_failed { "failed" } else { "ok" },
        "results": {"languages": Value::Object(languages)},
    });
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct CliWrap {
        #[command(flatten)]
        args: Args,
    }

    #[test]
    fn cli_defaults_align_with_end_state() {
        let parsed = CliWrap::try_parse_from(["test"]).expect("parse defaults");
        assert_eq!(parsed.args.lang, vec!["go", "python", "node", "rust"]);
        assert_eq!(parsed.args.go_packages, vec!["os", "path/filepath", "io/fs"]);
        assert_eq!(GO_SKIP_DEFAULT, "^TestVariousDeadlines(1Proc|4Proc)?$");
        assert_eq!(
            parsed.args.python_modules,
            vec![
                "os",
                "posix",
                "posixpath",
                "genericpath",
                "pathlib",
                "shutil",
                "tarfile",
                "stat",
                "fileio",
                "glob",
                "tempfile",
                "mmap",
                "fcntl"
            ]
        );
        assert!(!parsed.args.python_modules.iter().any(|m| m == "scandir"));
        assert!(!parsed.args.keep);
        assert_eq!(parsed.args.jobs, default_jobs());
        assert!(NODE_EXCLUDE_DEFAULT.contains(&"test-fs-null-bytes.js"));
        assert!(NODE_EXCLUDE_DEFAULT.contains(&"test-fs-read-file-sync-hostname.js"));
        assert!(NODE_EXCLUDE_DEFAULT.contains(&"test-fs-watch-recursive-validate.js"));
        assert!(!NODE_EXCLUDE_DEFAULT.contains(&"test-fs-promises-file-handle-sync.js"));
        assert_eq!(parsed.args.include_node_event_watch(), cfg!(target_os = "linux"));
    }

    #[test]
    fn node_watch_classification_separates_stat_polling_from_os_events() {
        assert!(is_node_stat_watchfile_test("test-fs-watchfile.js"));
        assert!(is_node_stat_watchfile_test("test-fs-watchfile-bigint.js"));
        assert!(is_node_stat_watchfile_test("test-fs-watchfile-ref-unref.js"));
        assert!(is_node_stat_watchfile_test(
            "test-fs-watch-file-enoent-after-deletion.js"
        ));
        assert!(!is_node_event_watch_test("test-fs-watchfile.js"));
        assert!(!is_node_event_watch_test("test-fs-watch-file-enoent-after-deletion.js"));

        assert!(is_node_event_watch_test("test-fs-watch.js"));
        assert!(is_node_event_watch_test("test-fs-promises-watch.js"));
        assert!(is_node_event_watch_test("test-fs-watch-recursive-watch-file.js"));

        let force_on = CliWrap::try_parse_from(["test", "--node-watch"]).expect("parse --node-watch");
        assert!(force_on.args.include_node_event_watch());
        let force_off = CliWrap::try_parse_from(["test", "--no-node-watch"]).expect("parse --no-node-watch");
        assert!(!force_off.args.include_node_event_watch());
    }

    #[test]
    fn parse_go_v_preserves_interleaved_parallel_failure_logs() {
        let out = "\
=== RUN   TestAlpha
=== PAUSE TestAlpha
=== RUN   TestBeta
=== PAUSE TestBeta
=== CONT  TestAlpha
    alpha_test.go:42: expected ENOENT, got EACCES
=== CONT  TestBeta
    beta_test.go:10: diagnostic note
--- PASS: TestBeta (0.01s)
--- FAIL: TestAlpha (0.02s)
=== RUN   TestGamma
--- SKIP: TestGamma (0.00s)
FAIL
";
        let p = parse_go_v(out);
        assert_eq!(p.pass, 1);
        assert_eq!(p.fail, 1);
        assert_eq!(p.skip, 1);
        assert_eq!(p.failures.len(), 1);
        assert_eq!(p.failures[0].0, "TestAlpha");
        assert!(
            p.failures[0].1.contains("alpha_test.go:42: expected ENOENT"),
            "got failure detail: {:?}",
            p.failures[0].1
        );
    }

    #[test]
    fn rust_runner_report_separates_skip_from_pass() {
        let ex = Exec {
            code: Some(0),
            timed_out: false,
            out: "RES\ttest_create\tok\t\nRES\ttest_ignored\tskipped\t#[ignore]\nRES\ttest_rename\tok\t\nDONE\n".into(),
            spawn_err: None,
        };
        let rep = collect_rust_runner_report(
            ex,
            3,
            Duration::from_millis(25),
            "embedded-at-build".into(),
            "std/src/fs/tests.rs".into(),
            vec![],
        );
        assert_eq!(rep["status"], json!("ok"));
        assert_eq!(rep["summary"]["total"], json!(3));
        assert_eq!(rep["summary"]["reported"], json!(3));
        assert_eq!(rep["summary"]["pass"], json!(2));
        assert_eq!(rep["summary"]["skip"], json!(1));
        assert_eq!(rep["summary"]["fail"], json!(0));
    }
}
