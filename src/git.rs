// Runs Git's `t/` test suite against `<mountpoint>` via `--root`, using the
// compile-time embedded Git build tree (`GIT_TEST_FSYNC=1` by default).

use crate::common::iso8601;
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

// Default curated suite (`fstest git <mnt>`): all Git t/ scripts that directly
// exercise filesystem contracts, object/ref storage backends, and core Git
// developer/agent workflows on the mount under test (with `GIT_TEST_FSYNC=1`
// enabled by default):
//   1. Repo init, capability probing & ownership (`safe.directory`)
//   2. Lockfiles, loose refs, reflogs, `packed-refs`, and `reftable` backend
//   3. Loose objects, packfiles, 1-page `mmap` sliding window, `.idx`/`.bitmap`,
//      `commit-graph`, `multi-pack-index` (`MIDX`), and `gc`/`repack`/cruft packs
//   4. `.git/index`, split-index, racy-mtime stat cache, untracked-cache & fsmonitor
//   5. Multi-worker parallel checkout, sparse-checkout, multi-worktree, hardlink
//      local clones & alternates
//   6. Symlinks, case-insensitivity, Unicode NFD/NFC, permissions/ACL, CRLF &
//      funny filenames
//   7. Core developer/agent workflows (`add`, `rm`, `mv`, `stash`, `rebase`,
//      `submodule`, executable `.git/hooks/*`)
// Names resolve against the embedded `t/` tree at runtime (exact stem first,
// then unique number prefix) so upstream suffix renames surface cleanly.
const DEFAULT_SCRIPTS: &[&str] = &[
    // 1. Capability probing, repo layout & ownership
    "t0001-init",
    "t0033-safe-directory",
    "t0050-filesystem",
    "t0055-beyond-symlinks",
    "t0060-path-utils",
    // 2. Lockfiles, loose refs, reflogs, packed-refs & reftable backend
    "t0031-lockfile-pid",
    "t0601-reffiles-pack-refs",
    "t0610-reftable-basics",
    "t0613-reftable-write-options",
    "t0614-reftable-fsck",
    "t1400-update-ref",
    "t1408-packed-refs",
    "t1410-reflog",
    // 3. Object store, packfiles, mmap sliding window, commit-graph, MIDX, bitmaps & gc/repack
    "t1050-large",
    "t5300-pack-object",
    "t5301-sliding-window",
    "t5302-pack-index",
    "t5304-prune",
    "t5310-pack-bitmaps",
    "t5318-commit-graph",
    "t5319-multi-pack-index",
    "t6500-gc",
    "t6501-freshen-objects",
    "t7700-repack",
    "t7704-repack-cruft",
    // 4. Index, split-index, racy-mtime stat cache, untracked-cache & fsmonitor
    "t1600-index",
    "t1700-split-index",
    "t1701-racy-split-index",
    "t2108-update-index-refresh-racy",
    "t7063-status-untracked-cache",
    "t7519-status-fsmonitor",
    // 5. Parallel checkout, sparse-checkout, multi-worktree, local clone & alternates
    "t1011-read-tree-sparse-checkout",
    "t1090-sparse-checkout-scope",
    "t2080-parallel-checkout-basics",
    "t2081-parallel-checkout-collisions",
    "t2400-worktree-add",
    "t2403-worktree-move",
    "t5605-clone-local",
    "t5613-info-alternate",
    // 6. Symlinks, case-sensitivity, Unicode NFD/NFC, permissions/ACL, CRLF & special filenames
    "t0003-attributes",
    "t0020-crlf",
    "t0028-working-tree-encoding",
    "t1301-shared-repo",
    "t1304-default-acl",
    "t2000-conflict-when-checking-files-out",
    "t2005-checkout-index-symlinks",
    "t2100-update-cache-badpath",
    "t2102-update-index-symlinks",
    "t2201-add-update-typechange",
    "t3300-funny-names",
    "t3902-quoted",
    "t3910-mac-os-precompose",
    "t4000-diff-format",
    "t4016-diff-quote",
    "t4129-apply-samemode",
    "t6405-merge-symlinks",
    "t6415-merge-dir-to-symlink",
    "t7515-status-symlinks",
    // 7. Core developer/agent workflows (add, rm, mv, stash, rebase, submodule, hooks)
    "t1800-hook",
    "t2200-add-update",
    "t3400-rebase",
    "t3600-rm",
    "t3700-add",
    "t3903-stash",
    "t4008-diff-break-rewrite",
    "t7001-mv",
    "t7400-submodule-basic",
];

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(4, 8))
        .unwrap_or(4)
}

#[derive(clap::Args)]
pub struct Args {
    /// git build tree (contains git, t/test-lib.sh, t/helper/test-tool);
    /// default: $FSTEST_GIT_DIR, else the embedded bundle extracted under ~/.cache/fstest
    #[arg(long)]
    pub git_dir: Option<PathBuf>,

    /// run every tNNNN script in the embedded Git tree (~1046 scripts) instead of the curated FS+workflow suite
    #[arg(long, conflicts_with = "tests")]
    pub all: bool,

    /// explicit scripts (comma list of tNNNN or tNNNN-name), overriding the default suite
    #[arg(long, value_delimiter = ',')]
    pub tests: Vec<String>,

    /// only run scripts whose name contains this substring (repeatable)
    #[arg(long = "filter")]
    pub filter: Vec<String>,

    /// resolve the selection and print it, without running
    #[arg(long)]
    pub list: bool,

    /// concurrent scripts (each owns its own trash directory)
    #[arg(long, default_value_t = default_jobs())]
    pub jobs: usize,

    /// per-script timeout in seconds
    #[arg(long, default_value_t = 600)]
    pub timeout: u64,

    /// disable GIT_TEST_FSYNC=1 (by default real fsync is enforced across objects, packfiles, refs, and lockfiles)
    #[arg(long)]
    pub no_fsync: bool,

    /// working directory name under the mount point (trash directories live here)
    #[arg(long, default_value = ".fstest-git")]
    pub work_dir: String,

    /// extra argument passed verbatim to every script (repeatable):
    /// -x, --immediate, --debug, --run=<n> ... (your own --root wins over ours)
    #[arg(long = "test-arg")]
    pub test_arg: Vec<String>,

    /// keep the working directory when done (failed scripts keep their trash
    /// directory anyway; this also keeps everything else)
    #[arg(long)]
    pub keep: bool,

    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

impl Args {
    pub fn fsync_enabled(&self) -> bool {
        !self.no_fsync
    }

    fn selection_mode(&self) -> &'static str {
        if !self.tests.is_empty() {
            "custom"
        } else if self.all {
            "all"
        } else {
            "default"
        }
    }
}

// ---------- tree discovery ----------

fn validate_tree(dir: &Path) -> bool {
    dir.join("git").is_file()
        && dir.join("t").join("test-lib.sh").is_file()
        && dir.join("t").join("helper").join("test-tool").is_file()
}

fn default_git_dir_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(d) = std::env::var("FSTEST_GIT_DIR")
        && !d.trim().is_empty()
    {
        v.push(PathBuf::from(d));
    }
    if let Some(bundle) = crate::bundle::ensure_extracted() {
        let p = bundle.join("git");
        if p.is_dir() {
            v.push(p);
        }
    }
    for base in crate::common::cache_dir_candidates() {
        let cache = base.join("git");
        if let Ok(rd) = fs::read_dir(&cache) {
            let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
            dirs.sort();
            dirs.reverse();
            v.extend(dirs);
        }
    }
    v
}

fn find_git_dir(args: &Args) -> Result<PathBuf, String> {
    let candidates = match &args.git_dir {
        Some(d) => vec![d.clone()],
        None => default_git_dir_candidates(),
    };
    for d in &candidates {
        if validate_tree(d) {
            return Ok(d.clone());
        }
    }
    Err(match &args.git_dir {
        Some(d) => format!(
            "{} is not a built git tree (needs git, t/test-lib.sh and t/helper/test-tool)",
            d.display()
        ),
        None => "no built git tree found (embedded bundle disabled and nothing in $FSTEST_GIT_DIR or ~/.cache/fstest/git/*; pass --git-dir or rebuild fstest with default features)"
            .into(),
    })
}

// ---------- selection ----------

fn discover_scripts(t_dir: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for e in fs::read_dir(t_dir).map_err(|e| format!("cannot read {}: {e}", t_dir.display()))? {
        let name = e
            .map_err(|e| format!("{}: {e}", t_dir.display()))?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name.starts_with('t') && name.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) && name.ends_with(".sh") {
            out.push(name.trim_end_matches(".sh").to_string());
        }
    }
    out.sort();
    Ok(out)
}

// resolve one selection entry: exact stem first, then unique number-prefix
// match (so `t1400` finds t1400-update-ref and upstream renames of the suffix
// still resolve); ambiguous or missing entries come back as failures
fn resolve_entry(entry: &str, scripts: &[String]) -> Result<String, String> {
    if scripts.iter().any(|s| s == entry) {
        return Ok(entry.to_string());
    }
    let prefixed: Vec<&String> = scripts.iter().filter(|s| s.starts_with(entry)).collect();
    match prefixed.len() {
        1 => Ok(prefixed[0].clone()),
        0 => Err(format!("{entry}: no such script in this tree")),
        _ => Err(format!(
            "{entry}: ambiguous, matches {}",
            prefixed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn selection(t_dir: &Path, args: &Args) -> Result<(Vec<String>, Vec<String>), String> {
    let scripts = discover_scripts(t_dir)?;
    let mut unmatched = Vec::new();
    let mut selected = Vec::new();
    if args.all && args.tests.is_empty() {
        selected = scripts;
    } else {
        let entries: Vec<String> = if !args.tests.is_empty() {
            args.tests.clone()
        } else {
            DEFAULT_SCRIPTS.iter().map(|s| s.to_string()).collect()
        };
        let mut seen = std::collections::HashSet::new();
        for e in &entries {
            match resolve_entry(e, &scripts) {
                Ok(s) => {
                    if seen.insert(s.clone()) {
                        selected.push(s);
                    }
                }
                Err(e) => unmatched.push(e),
            }
        }
    }
    if !args.filter.is_empty() {
        selected.retain(|s| args.filter.iter().any(|f| s.contains(f)));
    }
    selected.sort();
    Ok((selected, unmatched))
}

// ---------- TAP parsing ----------

struct Tap {
    plan: Option<u64>,
    pass: u64,
    fail: u64,
    skip: u64,
    todo: u64,
    fixed: u64,
    skip_all: Option<String>,
    skip_reasons: Vec<(String, u64)>,
    failures: Vec<(String, String)>,
}

// TAP as emitted by git's test-lib, plan line last: `ok N - name`,
// `ok N # skip <name> (missing PREREQ)`, `not ok N - name # TODO known
// breakage`, `ok N - name # TODO known breakage vanished`, failure detail as
// `#`-comment lines right after the `not ok` line, `1..0 # SKIP <reason>` for
// whole-file skips.
fn parse_tap(out: &str) -> Tap {
    let mut tap = Tap {
        plan: None,
        pass: 0,
        fail: 0,
        skip: 0,
        todo: 0,
        fixed: 0,
        skip_all: None,
        skip_reasons: Vec::new(),
        failures: Vec::new(),
    };
    let lines: Vec<&str> = out.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("1..") {
            let digits: &str = line[3..].split(|c: char| !c.is_ascii_digit()).next().unwrap_or("");
            tap.plan = digits.parse().ok();
            // whole-file skip: `1..0 # SKIP <reason>`
            if tap.plan == Some(0) {
                if let Some(pos) = line.find("# SKIP") {
                    tap.skip_all = Some(line[pos + 6..].trim().to_string());
                }
            }
            i += 1;
            continue;
        }
        let (ok, rest) = if let Some(r) = line.strip_prefix("not ok ") {
            (false, r)
        } else if let Some(r) = line.strip_prefix("ok ") {
            (true, r)
        } else {
            i += 1;
            continue;
        };
        // strip the test number, then split off the directive. Two shapes:
        // `5 - failing test # TODO ...` and `2 # skip ...` (skips carry no
        // `- name` part, so the remainder starts with the `#` directly)
        let rest = rest.trim_start().trim_start_matches(|c: char| c.is_ascii_digit());
        let rest = rest.trim_start();
        let (body, directive) = if let Some(d) = rest.strip_prefix("# ") {
            ("", Some(d))
        } else {
            match rest.split_once(" # ") {
                Some((b, d)) => (b, Some(d)),
                None => (rest, None),
            }
        };
        let name = body.strip_prefix("- ").unwrap_or(body).trim();
        let mut detail = String::new();
        match directive {
            Some(d) => {
                let mut it = d.splitn(2, ' ');
                let word = it.next().unwrap_or("").to_ascii_lowercase();
                let drest = it.next().unwrap_or("").trim();
                match word.as_str() {
                    "skip" => {
                        tap.skip += 1;
                        // `skip <name> (missing PREREQ)`: keep the reason in
                        // the trailing parenthesized group when there is one
                        let reason = match drest.rsplit_once('(') {
                            Some((_, r)) => r.trim_end_matches(')'),
                            None => drest,
                        };
                        if !reason.is_empty() {
                            let reason = reason.chars().take(100).collect::<String>();
                            match tap.skip_reasons.iter_mut().find(|(r, _)| *r == reason) {
                                Some((_, n)) => *n += 1,
                                None => tap.skip_reasons.push((reason, 1)),
                            }
                        }
                    }
                    // `ok … # TODO known breakage vanished` (and any other
                    // ok+todo) = a TODO test that passed
                    "todo" if drest.contains("vanished") || ok => tap.fixed += 1,
                    "todo" => tap.todo += 1,
                    _ => {
                        if ok {
                            tap.pass += 1;
                        } else {
                            tap.fail += 1;
                        }
                        if !ok && tap.failures.len() < 10 {
                            detail = name.to_string();
                        }
                    }
                }
            }
            None => {
                if ok {
                    tap.pass += 1;
                } else {
                    tap.fail += 1;
                    if tap.failures.len() < 10 {
                        detail = name.to_string();
                    }
                }
            }
        }
        i += 1;
        if !detail.is_empty() {
            let mut det = format!("{detail}\n");
            while i < lines.len() && lines[i].starts_with('#') {
                let t = lines[i].trim_start_matches('#').trim();
                // end-of-run summary lines are `#` comments too; stop there
                if t.starts_with("failed ")
                    || t.starts_with("passed ")
                    || t.starts_with("still have ")
                    || t.contains("known breakage(s)")
                {
                    break;
                }
                if det.len() < 800 {
                    det.push_str(t);
                    det.push('\n');
                }
                i += 1;
            }
            tap.failures.push((detail, tail(&det, 800)));
        }
    }
    tap
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

// ---------- execution ----------

struct Exec {
    code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
    spawn_err: Option<String>,
}

// runs a script in its own process group so a timeout can take the whole tree
// down (test scripts leave children holding our pipes otherwise)
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
                stdout: String::new(),
                stderr: String::new(),
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
    let stdout = String::from_utf8_lossy(&t_so.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&t_se.join().unwrap_or_default()).into_owned();
    Exec {
        code,
        timed_out,
        stdout,
        stderr,
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

// remove_dir_all in a detached thread with a bounded wait: recursive removal
// on a wedged network/FUSE mount can block forever, and the report must still
// be emitted (failed scripts intentionally leave their trash directories
// behind, so there is real work here after every failing run)
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
            "git: cleanup of {} did not finish in {:?}; leaving it (the mount may be wedged)",
            path.display(),
            bound
        );
    }
}

// ---------- listing ----------

pub fn list(args: &Args) -> Result<(), String> {
    let tree = find_git_dir(args)?;
    let (selected, unmatched) = selection(&tree.join("t"), args)?;
    for s in &selected {
        println!("{s}");
    }
    for u in &unmatched {
        warn!("git: selection not matched: {u}");
    }
    info!(
        "git: {} scripts selected from {} ({}) in {}",
        selected.len(),
        tree.display(),
        args.selection_mode(),
        tree.join("t").display()
    );
    Ok(())
}

// ---------- run ----------

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let tree = find_git_dir(args)?;
    let t_dir = tree.join("t");
    let (selected, unmatched) = selection(&t_dir, args)?;
    if selected.is_empty() {
        return Err("no test scripts selected (check --all/--tests/--filter against this tree)".into());
    }
    for u in &unmatched {
        warn!("git: selection not matched: {u}");
    }
    if !unmatched.is_empty() {
        info!(
            "git: {} of {} selection entries resolved",
            selected.len(),
            selected.len() + unmatched.len()
        );
    }

    // GIT-VERSION-FILE is written by the build; 2.55 writes `GIT_VERSION=2.55.0`
    let version = fs::read_to_string(tree.join("GIT-VERSION-FILE"))
        .ok()
        .and_then(|s| {
            let s = s.trim();
            let v = s.split_once('=').map(|(_, v)| v).unwrap_or(s);
            (!v.is_empty()).then(|| v.to_string())
        })
        .or_else(|| {
            let mut c = Command::new(tree.join("git"));
            c.arg("--version");
            c.output().ok().filter(|o| o.status.success()).map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .trim_start_matches("git version ")
                    .to_string()
            })
        });
    let commit = Command::new(tree.join("git"))
        .args(["--git-dir", ".git"])
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(&tree)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    let work_root = mountpoint.join(&args.work_dir);
    fs::create_dir_all(&work_root).map_err(|e| format!("{}: {e}", work_root.display()))?;
    let results_dir = work_root.join("test-results");
    let fsync_on = args.fsync_enabled();

    info!(
        "git: {} scripts selected ({}, fsync={}) on {} using {} ({})",
        selected.len(),
        args.selection_mode(),
        fsync_on,
        mountpoint.display(),
        tree.display(),
        if args.jobs == 1 {
            "1 job".to_string()
        } else {
            format!("{} jobs", args.jobs)
        },
    );
    let started = SystemTime::now();
    let t0 = Instant::now();
    let items = run_parallel(selected.len(), args.jobs, |i| {
        let it0 = Instant::now();
        let name = &selected[i];
        let script = t_dir.join(format!("{name}.sh"));
        let mut cmd = Command::new(&script);
        cmd.arg(format!("--root={}", work_root.display()))
            .args(&args.test_arg)
            .current_dir(&t_dir)
            // the harness manages HOME/GIT_* itself; a stray GIT_DIR/GIT_WORK_TREE
            // from the calling shell must not leak into the tests (test-lib
            // unsets the common ones, be thorough anyway)
            .env_clear();
        for (k, v) in std::env::vars_os() {
            let is_git = k.as_encoded_bytes().starts_with(b"GIT_");
            if !is_git {
                cmd.env(k, v);
            }
        }
        cmd.env("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME", "main")
            .env("GIT_TEST_FSYNC", if fsync_on { "1" } else { "0" })
            .env("TEST_OUTPUT_DIRECTORY", &results_dir);
        let ex = exec_with_timeout(cmd, Duration::from_secs(args.timeout.max(1)));
        let tap = parse_tap(&ex.stdout);
        let seen = tap.pass + tap.fail + tap.skip + tap.todo + tap.fixed;
        // upstream exits 1 when a known breakage vanished (fail == 0, fixed >
        // 0) — that is a signal, not a filesystem failure
        let status = if ex.spawn_err.is_some() {
            "broken"
        } else if ex.timed_out {
            "timedout"
        } else if tap.plan.is_none() || Some(seen) != tap.plan {
            // the plan line is printed last, so any harness death (missing
            // build artifacts, an unwritable tree, a wedged mount) shows up
            // as a missing/incomplete plan
            "broken"
        } else if tap.plan == Some(0) {
            "skip"
        } else if tap.fail > 0 {
            "fail"
        } else if ex.code != Some(0) && tap.fixed == 0 {
            "broken"
        } else {
            "ok"
        };
        if status != "ok" && status != "skip" {
            warn!(
                "git: {} -> {} ({} pass {} fail {} skip {} todo {} fixed)",
                name, status, tap.pass, tap.fail, tap.skip, tap.todo, tap.fixed
            );
        }
        info!(
            "git: {} -> {} ({} pass {} fail {} skip {} todo {} fixed, {}m{:02}s)",
            name,
            status,
            tap.pass,
            tap.fail,
            tap.skip,
            tap.todo,
            tap.fixed,
            it0.elapsed().as_secs() / 60,
            it0.elapsed().as_secs() % 60
        );
        let mut item = json!({
            "name": name, "status": status,
            "pass": tap.pass, "fail": tap.fail, "skip": tap.skip,
            "todo": tap.todo, "fixed": tap.fixed,
            "durationMs": it0.elapsed().as_millis() as u64,
            "exitCode": ex.code,
        });
        let obj = item.as_object_mut().unwrap();
        if let Some(r) = &tap.skip_all {
            obj.insert("skipAll".into(), json!(r));
        }
        if !tap.skip_reasons.is_empty() {
            obj.insert(
                "skipReasons".into(),
                json!(
                    tap.skip_reasons
                        .iter()
                        .map(|(r, n)| json!({"reason": r, "count": n}))
                        .collect::<Vec<_>>()
                ),
            );
        }
        if !tap.failures.is_empty() {
            obj.insert(
                "failures".into(),
                json!(
                    tap.failures
                        .iter()
                        .map(|(n, d)| json!({"name": n, "detail": d}))
                        .collect::<Vec<_>>()
                ),
            );
        }
        if status != "ok" {
            let mut tailtext = ex.stderr.clone();
            if tailtext.is_empty() || status == "broken" {
                tailtext.push_str(&ex.stdout);
            }
            obj.insert("tail".into(), json!(tail(&tailtext, 2000)));
        }
        if let Some(e) = ex.spawn_err {
            obj.insert("error".into(), json!(e));
        }
        item
    });

    let (mut ok_n, mut fail_n, mut broken_n, mut timedout_n, mut skip_n) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut t_pass, mut t_fail, mut t_skip, mut t_todo, mut t_fixed) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for item in items.iter() {
        match item["status"].as_str().unwrap_or("?") {
            "ok" => ok_n += 1,
            "fail" => fail_n += 1,
            "broken" => broken_n += 1,
            "timedout" => timedout_n += 1,
            "skip" => skip_n += 1,
            _ => {}
        }
        t_pass += item["pass"].as_u64().unwrap_or(0);
        t_fail += item["fail"].as_u64().unwrap_or(0);
        t_skip += item["skip"].as_u64().unwrap_or(0);
        t_todo += item["todo"].as_u64().unwrap_or(0);
        t_fixed += item["fixed"].as_u64().unwrap_or(0);
    }
    let run_ok = fail_n == 0 && broken_n == 0 && timedout_n == 0;
    info!(
        "git: done in {}m{:02}s — {} scripts: {} ok {} failed {} broken {} timed out {} skipped; \
         assertions: {} pass {} fail {} skip {} todo {} fixed",
        t0.elapsed().as_secs() / 60,
        t0.elapsed().as_secs() % 60,
        selected.len(),
        ok_n,
        fail_n,
        broken_n,
        timedout_n,
        skip_n,
        t_pass,
        t_fail,
        t_skip,
        t_todo,
        t_fixed
    );

    if !args.keep {
        // bound generous: failed runs leave whole .git trees behind on the mount
        remove_dir_all_bounded(&work_root, Duration::from_secs(60));
    }

    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    let report = json!({
        "suite": "git",
        "host": host,
        "date": iso8601(started),
        "top": mountpoint.display().to_string(),
        "params": {
            "git_dir": tree.display().to_string(),
            "git_version": version,
            "git_commit": commit,
            "selection": args.selection_mode(),
            "all": args.all,
            "tests": if args.tests.is_empty() { Value::Null } else { json!(args.tests) },
            "filter": args.filter,
            "fsync": fsync_on,
            "jobs": args.jobs,
            "timeout_sec": args.timeout,
        },
        "status": if run_ok { "ok" } else { "failed" },
        "results": {
            "summary": {
                "scripts": selected.len(),
                "ok": ok_n, "failed": fail_n, "broken": broken_n,
                "timedout": timedout_n, "skipped": skip_n,
                "tests": {
                    "pass": t_pass, "fail": t_fail, "skip": t_skip,
                    "todo": t_todo, "fixed": t_fixed,
                },
            },
            "items": items,
        },
    });
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: Args,
    }

    #[test]
    fn default_scripts_are_unique_and_cover_all_git_fs_pillars() {
        assert_eq!(DEFAULT_SCRIPTS.len(), 67);
        let mut uniq = std::collections::HashSet::new();
        for &s in DEFAULT_SCRIPTS {
            assert!(uniq.insert(s), "duplicate script in DEFAULT_SCRIPTS: {s}");
        }
        // Key representatives across reftable, pack/mmap/MIDX/commit-graph/gc/repack,
        // parallel-checkout, worktree, and porcelain workflows
        for must in [
            "t0610-reftable-basics",
            "t1408-packed-refs",
            "t1800-hook",
            "t2080-parallel-checkout-basics",
            "t2400-worktree-add",
            "t3400-rebase",
            "t3700-add",
            "t3903-stash",
            "t5300-pack-object",
            "t5301-sliding-window",
            "t5318-commit-graph",
            "t5319-multi-pack-index",
            "t6500-gc",
            "t7400-submodule-basic",
            "t7700-repack",
        ] {
            assert!(uniq.contains(must), "missing expected script {must}");
        }
    }

    #[test]
    fn default_cli_enables_fsync_and_default_selection() {
        let cli = TestCli::parse_from(["test"]);
        assert!(cli.args.fsync_enabled());
        assert!(!cli.args.all);
        assert_eq!(cli.args.selection_mode(), "default");

        let cli_no_fsync = TestCli::parse_from(["test", "--no-fsync", "--all"]);
        assert!(!cli_no_fsync.args.fsync_enabled());
        assert!(cli_no_fsync.args.all);
        assert_eq!(cli_no_fsync.args.selection_mode(), "all");
    }

    #[test]
    fn tap_parser_handles_todo_vanished_and_skips() {
        let sample = "\
ok 1 - basic init
ok 2 # skip case folding (missing CASE_INSENSITIVE_FS)
not ok 3 - known racy # TODO known breakage
ok 4 - fixed upstream # TODO known breakage vanished
1..4
";
        let tap = parse_tap(sample);
        assert_eq!(tap.plan, Some(4));
        assert_eq!(tap.pass, 1);
        assert_eq!(tap.skip, 1);
        assert_eq!(tap.todo, 1);
        assert_eq!(tap.fixed, 1);
        assert_eq!(tap.fail, 0);
    }
}
