// stdfs: runs the filesystem-relevant portions of the Go, Python, Node.js and
// Rust standard-library test suites against a mounted filesystem. Like the
// ltp/fio adapters the suites come from the user's own toolchains — nothing is
// embedded or redistributed:
//   go:     `go test -c` compiles GOROOT packages (default os, path/filepath,
//           io/fs); the test binary runs with cwd/TMPDIR on the mount.
//   python: `<python> -m test -v <modules>` from the interpreter's bundled
//           `test` package (or --python-dir) with cwd on the mount, where the
//           suite's TESTFN scratch files land.
//   node:   `node --test` over the FS-touching files of a nodejs/node checkout
//           (--node-dir; installed node binaries don't ship their test suite).
//   rust:   reads std/src/fs/tests.rs from the rustc sysroot's rust-src
//           component, rewrites `crate::` paths onto std, links them against a
//           small shim for std's test helpers and compiles a standalone runner
//           with the same rustc (the two tests using the unstable fs::Dir API
//           are excluded).
// Languages whose toolchain or source dir cannot be resolved are reported as
// unavailable and don't fail the run; only real test failures do.

use crate::smallfile::iso8601;
use log::{info, warn};
use serde_json::{Value, json};
use std::fs;
use std::io::Read as _;
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
    ("file_test_io_non_positional_read", "uses unstable read_buf/BorrowedBuf APIs"),
    ("test_read_buf_at", "uses unstable read_buf/BorrowedBuf APIs"),
    ("test_read_buf_exact_at", "uses unstable read_buf/BorrowedBuf APIs"),
    ("test_seek_read_buf", "uses unstable read_buf/BorrowedBuf APIs"),
    ("file_test_read_buf", "uses unstable read_buf/BorrowedBuf APIs"),
    ("test_fs_set_times", "uses unstable free-fn fs::set_times"),
    ("test_fs_set_times_nofollow", "uses unstable free-fn fs::set_times"),
    ("test_fs_set_times_follows_symlink", "uses unstable free-fn fs::set_times"),
    ("test_fs_set_times_on_dir", "uses unstable free-fn fs::set_times"),
    ("metadata_access_times", "uses unstable io ErrorKind::Uncategorized"),
    ("dir_entry_debug", "touches std-internal DirEntry field"),
];

// go tests skipped by default because they read GOROOT-relative testdata or
// the package source files and only pass with cwd == the GOROOT package dir;
// --go-skip "" disables the filter
const GO_SKIP_DEFAULT: &str = "^(TestClosedStat|TestReadFile|TestReadDir|TestFileReaddir|TestFileReadDir|TestFileReaddirnames|TestReaddirSmallSeek|TestDirFS|TestDirFSRootDir|TestRootDirFS|TestCopyFS|TestGlob|TestNonWindowsGlobEscape|ExampleReadFile)$";

#[derive(clap::Args)]
pub struct Args {
    /// languages to run, comma list (go,python,node,rust)
    #[arg(long, value_delimiter = ',', default_value = "go,python,node,rust")]
    pub lang: Vec<String>,

    /// Go: GOROOT override (uses <dir>/bin/go; default: `go` on PATH)
    #[arg(long)]
    pub go_dir: Option<PathBuf>,
    /// Go: packages to test
    #[arg(long, value_delimiter = ',', default_value = "os,path/filepath,io/fs")]
    pub go_packages: Vec<String>,
    /// Go: -test.skip regex override (empty string disables the default skip list)
    #[arg(long)]
    pub go_skip: Option<String>,

    /// Python: directory whose `test/` package to run (a CPython Lib/ dir)
    #[arg(long)]
    pub python_dir: Option<PathBuf>,
    /// Python: interpreter override (default: python3 on PATH, or one next to --python-dir)
    #[arg(long)]
    pub python_exe: Option<PathBuf>,
    /// Python: test modules (test_ prefix added as needed)
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "os,posix,shutil,stat,fileio,scandir,glob,tempfile,mmap,fcntl"
    )]
    pub python_modules: Vec<String>,

    /// Python: extra regrtest ignore patterns (test ids like test.test_os.ForkTests.test_fork)
    #[arg(long, value_delimiter = ',')]
    pub python_ignore: Vec<String>,

    /// Node: nodejs/node checkout containing test/parallel (required; installed node ships no tests)
    #[arg(long)]
    pub node_dir: Option<PathBuf>,
    /// Node: only run test files whose name contains this substring
    #[arg(long)]
    pub node_filter: Option<String>,

    /// Rust: rust `library/` source dir (default: rustc sysroot's rust-src component)
    #[arg(long)]
    pub rust_src_dir: Option<PathBuf>,

    /// per-item timeout in seconds
    #[arg(long, default_value_t = 600)]
    pub timeout: u64,
    /// concurrent items within a language
    #[arg(long, default_value_t = 4)]
    pub jobs: usize,
    /// working directory name under the mount point
    #[arg(long, default_value = ".fstest-stdfs")]
    pub work_dir: String,
    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let p = Path::new(dir).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
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
            return Exec { code: None, timed_out: false, out: String::new(), spawn_err: Some(e.to_string()) };
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
    Exec { code, timed_out, out, spawn_err: None }
}

fn run_parallel<R: Send>(n: usize, jobs: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..n).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..jobs.max(1) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= n {
                    break;
                }
                let r = f(i);
                out.lock().unwrap()[i] = Some(r);
            });
        }
    });
    out.into_inner().unwrap().into_iter().map(|x| x.expect("worker result")).collect()
}

fn unavailable(reason: String) -> Value {
    json!({"available": false, "reason": reason})
}

// ---------- go ----------

struct GoParsed {
    pass: u64,
    fail: u64,
    skip: u64,
    failures: Vec<(String, String)>,
}

fn parse_go_v(out: &str) -> GoParsed {
    let mut p = GoParsed { pass: 0, fail: 0, skip: 0, failures: Vec::new() };
    // with -test.v the failure detail is the indented block printed between
    // `=== RUN` and the `--- FAIL:` marker
    let mut pending = String::new();
    for line in out.lines() {
        if line.starts_with("=== ") {
            pending.clear();
            continue;
        }
        if line.starts_with("--- ") {
            let rest = &line[4..];
            let Some((verb, tailpart)) = rest.split_once(' ') else { continue };
            let name = tailpart.split(" (").next().unwrap_or(tailpart).trim();
            match verb {
                "PASS:" => p.pass += 1,
                "SKIP:" => p.skip += 1,
                "FAIL:" => {
                    p.fail += 1;
                    if p.failures.len() < 10 {
                        p.failures.push((name.to_string(), tail(&pending, 800)));
                    }
                }
                _ => {}
            }
            pending.clear();
            continue;
        }
        if line.starts_with(char::is_whitespace) && !line.trim().is_empty() {
            if pending.len() < 800 {
                pending.push_str(line.trim_start());
                pending.push('\n');
            }
            continue;
        }
        pending.clear();
    }
    p
}

fn run_go(_mountpoint: &Path, args: &Args, work_root: &Path, hosttmp: &Path) -> Value {
    let go = match &args.go_dir {
        Some(d) => {
            let g = d.join("bin/go");
            if !g.is_file() {
                return unavailable(format!("{} not found; --go-dir must be a GOROOT containing bin/go", g.display()));
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
    if !Path::new(&goroot).join("src/os").is_dir() {
        return unavailable(format!("GOROOT {} has no src/os", goroot));
    }
    let skip = args.go_skip.clone().unwrap_or_else(|| GO_SKIP_DEFAULT.to_string());
    info!("stdfs[go]: GOROOT {} ({} packages, skip filter: {:?})", goroot, args.go_packages.len(), skip);
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
        let wd = work_root.join("go").join(&safe);
        let _ = std::fs::create_dir_all(&wd);
        let mut rc = Command::new(&bin);
        // t.TempDir() resolves under TMPDIR, which pins scratch dirs to the
        // mount; parallel=1 keeps shared-TMPDIR listing tests deterministic
        rc.arg("-test.v")
            .arg("-test.parallel=1")
            .arg(format!("-test.timeout={}s", args.timeout))
            .current_dir(&wd)
            .env("TMPDIR", &wd);
        if !skip.is_empty() {
            rc.arg(format!("-test.skip={skip}"));
        }
        let t0 = Instant::now();
        let ex = exec_with_timeout(rc, Duration::from_secs(args.timeout.max(1)));
        let p = parse_go_v(&ex.out);
        let ok = ex.code == Some(0) && !ex.timed_out && p.fail == 0;
        if !ok {
            warn!("stdfs[go]: {} FAILED ({} pass {} fail {} skip)", pkg, p.pass, p.fail, p.skip);
        }
        info!(
            "stdfs[go]: {} {} pass={} fail={} skip={} ({}s)",
            pkg,
            if ok { "ok" } else { "fail" },
            p.pass, p.fail, p.skip,
            t0.elapsed().as_secs()
        );
        let status = if ex.timed_out { "timedout" } else if ok { "ok" } else { "fail" };
        json!({
            "name": pkg, "status": status,
            "pass": p.pass, "fail": p.fail, "skip": p.skip,
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
    let mut p = PyParsed { pass: 0, fail: 0, skip: 0, failures: Vec::new() };
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

// harness artifact when the suite runs from a --python-dir Lib: test_fork's
// child python re-execs with -E (ignoring PYTHONPATH) and cannot import test
const PYTHON_IGNORE_DEFAULT: &[&str] = &["test.test_os.ForkTests.test_fork"];

fn run_python(_mountpoint: &Path, args: &Args, work_root: &Path) -> Value {
    let exe: Option<PathBuf> = if let Some(p) = &args.python_exe {
        Some(p.clone())
    } else if let Some(d) = &args.python_dir {
        python_exe_for_dir(d).or_else(|| find_in_path("python3"))
    } else {
        find_in_path("python3")
    };
    let Some(exe) = exe else {
        return unavailable("no python3 on PATH; pass --python-exe".into());
    };
    let test_lib: PathBuf = match &args.python_dir {
        Some(d) => d.clone(),
        None => {
            let Some(stdlib) = cmd_stdout({
                let mut c = Command::new(&exe);
                c.args(["-c", "import sysconfig;print(sysconfig.get_path('stdlib'))"]);
                c
            }) else {
                return unavailable(format!("failed to query stdlib dir of {}", exe.display()));
            };
            let p = PathBuf::from(&stdlib);
            if !p.join("test").is_dir() {
                return unavailable(format!(
                    "python3 ({}) ships without the bundled `test` package; pass --python-dir (a CPython Lib/ dir containing test/)",
                    exe.display()
                ));
            }
            p
        }
    };
    if !test_lib.join("test").is_dir() {
        return unavailable(format!("{} contains no test/ package", test_lib.display()));
    }
    let modules: Vec<String> = args
        .python_modules
        .iter()
        .map(|m| if m.starts_with("test_") { m.clone() } else { format!("test_{m}") })
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
    let mut patterns: Vec<String> = PYTHON_IGNORE_DEFAULT.iter().map(|s| s.to_string()).collect();
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
        if !test_lib.join("test").join(format!("{m}.py")).is_file() {
            return json!({"name": m, "status": "missing"});
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
            warn!("stdfs[python]: {} FAILED ({} pass {} fail {} skip)", m, p.pass, p.fail, p.skip);
        }
        info!(
            "stdfs[python]: {} {} pass={} fail={} skip={} ({}s)",
            m,
            if ok { "ok" } else { "fail" },
            p.pass, p.fail, p.skip,
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
    let ok = items.iter().all(|it| it["status"] == json!("ok") || it["status"] == json!("missing"));
    json!({
        "available": true,
        "status": if ok { "ok" } else { "failed" },
        "exe": exe.display().to_string(),
        "testLib": test_lib.display().to_string(),
        "summary": {
            "modules": items.len(),
            "missing": items.iter().filter(|it| it["status"] == json!("missing")).count(),
            "failed": items.iter().filter(|it| it["status"] != json!("ok") && it["status"] != json!("missing")).count(),
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
    let mut p = TapParsed { pass: 0, fail: 0, failures: Vec::new() };
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

// node test files skipped by default because they assume cwd == the node
// checkout root (relative paths into test/)
const NODE_EXCLUDE_DEFAULT: &[&str] = &["test-fs-realpath-native.js"];

fn run_node(_mountpoint: &Path, args: &Args, work_root: &Path) -> Value {
    let Some(node) = find_in_path("node") else {
        return unavailable("no `node` on PATH; install Node.js".into());
    };
    if let Some(v) = cmd_stdout({
        let mut c = Command::new(&node);
        c.arg("--version");
        c
    }) {
        let major: usize = v.trim_start_matches('v').split('.').next().and_then(|s| s.parse().ok()).unwrap_or(0);
        if major < 18 {
            return unavailable(format!("node {v} lacks the --test runner (need >= 18)"));
        }
    }
    // installed node binaries never ship the test suite; a from-source tree has
    // test/ two levels up from the built binary (node/out/Release/node)
    let dir: PathBuf = match &args.node_dir {
        Some(d) => d.clone(),
        None => {
            let cand = node.parent().unwrap_or(Path::new("/")).ancestors().nth(2).map(|p| p.join("test"));
            match cand.filter(|c| c.join("parallel").is_dir()) {
                Some(c) => c.parent().unwrap().to_path_buf(),
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
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join("test/parallel"))
        .map_err(|e| format!("{}: {e}", dir.join("test/parallel").display()))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
            n.starts_with("test-fs-") && n.ends_with(".js") && !n.contains("watch")
                && !NODE_EXCLUDE_DEFAULT.iter().any(|x| n.contains(x))
        })
        .collect();
    files.sort();
    if let Some(f) = &args.node_filter {
        files.retain(|p| p.file_name().map(|n| n.to_string_lossy().contains(f.as_str())).unwrap_or(false));
    }
    if files.is_empty() {
        return unavailable("no test-fs-*.js files matched in test/parallel".into());
    }
    info!("stdfs[node]: {} {} fs test files (watch excluded)", node.display(), files.len());
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
            p.pass, p.fail,
            t0.elapsed().as_secs()
        );
        let status = if ex.timed_out { "timedout" } else if ok { "ok" } else { "fail" };
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
        if RUST_EXCLUDE.iter().any(|(n, _)| *n == name) {
            excluded.push(name.clone());
            // skip the whole fn block by net brace depth (braces inside
            // format strings come in balanced pairs, so they cancel out)
            let mut depth: i32 = 0;
            let mut seen_open = false;
            while j < lines.len() {
                for ch in lines[j].chars() {
                    if ch == '{' {
                        depth += 1;
                        seen_open = true;
                    } else if ch == '}' {
                        depth -= 1;
                    }
                }
                j += 1;
                if seen_open && depth <= 0 {
                    break;
                }
            }
            skip.push((back, j - 1));
            i = j;
            continue;
        }
        drops.push(i);
        pubs.push(j);
        tests.push((back_attrs, name));
        i = j;
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

const SHIM: &str = r#"
mod shim {
    // assert_matches! is not exported by current stable std; reproduce the
    // upstream macro (same match-and-panic semantics)
    macro_rules! assert_matches {
        ($left:expr, $($pattern:pat_param)|+ $(,)?) => {
            match $left {
                $($pattern)|+ => {}
                left => panic!(
                    "assertion failed: `{:?}` does not match any of the expected patterns",
                    left
                ),
            }
        };
    }
    pub(crate) use assert_matches;
    pub mod rand {
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
        pub fn test_rng() -> TestRng {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15);
            TestRng(nanos | 1)
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
"#;

fn run_rust(_mountpoint: &Path, args: &Args, work_root: &Path, hosttmp: &Path) -> Value {
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
    let (body, tests, excluded) = transform_rust_tests(&src);
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
        for a in attrs {
            runner.push_str("    ");
            runner.push_str(a);
            runner.push('\n');
        }
        let ignored = if attrs.iter().any(|a| a.starts_with("#[ignore")) { "true" } else { "false" };
        runner.push_str(&format!("    ({name:?}, tests_impl::{name}, {ignored}),\n"));
    }
    runner.push_str("];\n\n");
    runner.push_str(
        r#"fn main() {
    let filter = std::env::args().nth(1);
    for (name, func, ignored) in TESTS {
        if *ignored {
            println!("RES\t{name}\tskipped\t#[ignore]");
            continue;
        }
        if let Some(f) = &filter {
            if !name.contains(f.as_str()) {
                println!("RES\t{name}\tskipped\t");
                continue;
            }
        }
        let n = *name;
        let res = std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(func)).map_err(|p| {
                    if let Some(s) = p.downcast_ref::<&str>() {
                        (*s).to_string()
                    } else if let Some(s) = p.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "panic".to_string()
                    }
                })
            })
            .expect("spawn test thread")
            .join();
        match res {
            Ok(Ok(())) => println!("RES\t{n}\tok\t"),
            Ok(Err(msg)) => println!("RES\t{n}\tfail\t{}", msg),
            Err(_) => println!("RES\t{n}\tfail\tthread panicked or aborted"),
        }
    }
    println!("DONE");
}
"#,
    );
    let runner_rs = hosttmp.join("ruststd_runner.rs");
    let runner_bin = hosttmp.join("ruststd_runner");
    if let Err(e) = std::fs::write(&runner_rs, &runner) {
        return unavailable(format!("write {}: {e}", runner_rs.display()));
    }
    info!(
        "stdfs[rust]: {} ({}) -> {} tests ({} excluded: {}); compiling with {}...",
        tests_rs.display(),
        "std/src/fs/tests.rs",
        tests.len(),
        excluded.len(),
        RUST_EXCLUDE.iter().filter(|(n, _)| excluded.iter().any(|x| x == *n)).map(|(n, r)| format!("{n}: {r}")).collect::<Vec<_>>().join(", "),
        rustc.display()
    );
    let mut cc = Command::new(&rustc);
    cc.arg("--edition=2021")
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
    rc.current_dir(&wd).env("TMPDIR", &wd).env("RUST_BACKTRACE", "0");
    let t0 = Instant::now();
    let ex = exec_with_timeout(rc, Duration::from_secs(args.timeout.max(1)));
    let (mut pass, mut fail) = (0u64, 0u64);
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
            "ok" | "skipped" => pass += 1,
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
        warn!("stdfs[rust]: FAILED ({} pass {} fail)", pass, fail);
    }
    info!("stdfs[rust]: {} pass={} fail={} ({}s)", if ok { "ok" } else { "fail" }, pass, fail, t0.elapsed().as_secs());
    let status = if ex.timed_out { "timedout" } else if ok { "ok" } else { "fail" };
    json!({
        "available": true,
        "status": status,
        "rustc": rustc.display().to_string(),
        "source": tests_rs.display().to_string(),
        "excluded": RUST_EXCLUDE.iter().filter(|(n, _)| excluded.iter().any(|x| x == *n)).map(|(n, r)| json!({"name": n, "reason": r})).collect::<Vec<_>>(),
        "summary": {
            "total": tests.len(),
            "reported": seen,
            "pass": pass,
            "fail": fail,
            "timedout": if ex.timed_out { json!(tests.len() as u64 - seen) } else { json!(0) },
        },
        "failures": failures.iter().map(|(n, t)| json!({"name": n, "tail": t})).collect::<Vec<_>>(),
        "durationMs": t0.elapsed().as_millis() as u64,
        "tail": tail(&ex.out, 2000),
    })
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
    let _ = fs::remove_dir_all(&work_root);
    if std::env::var("FSTEST_KEEP_TMP").map(|v| v == "1").unwrap_or(false) {
        warn!("FSTEST_KEEP_TMP=1: keeping {}", hosttmp.display());
    } else {
        let _ = fs::remove_dir_all(&hosttmp);
    }
    let any_failed = languages.values().any(|v| {
        v.get("available").and_then(Value::as_bool).unwrap_or(false)
            && v.get("status") == Some(&json!("failed"))
    });
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
        },
        "status": if any_failed { "failed" } else { "ok" },
        "results": {"languages": Value::Object(languages)},
    });
    if let Some(path) = &args.json {
        if let Err(e) = fs::write(path, serde_json::to_vec_pretty(&report).unwrap()) {
            warn!("failed to write {}: {e}", path.display());
        }
    }
    Ok(report)
}
