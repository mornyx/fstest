// sqlite adapter: runs SQLite's mptest suite against a mounted filesystem.
// mptest is SQLite's own tester "for testing the ability of independent
// processes to access the same SQLite database concurrently" — real client
// processes exercising the filesystem's locking, atomic-rename, fsync and
// hot-journal recovery paths. The parent process and its --client children
// (five in the embedded scripts) coordinate through
// task/client/counters tables inside the shared database itself, so wherever
// the database file lives is what gets tested.
//
// Like the pjdfstest suite, everything needed ships inside the fstest
// binary — zero external dependencies, zero preparation:
//   - the four mptest scripts (+ the crash02.subtest that crash01 --source-es)
//     are embedded verbatim (public domain), and
//   - the 1.4k-line mptest.c coordinator is ported behavior-for-behavior on
//     top of the bundled SQLite engine (rusqlite "bundled", compiled in at
//     build time). The engine itself is not ported — it IS SQLite, which is
//     the system under test.
// The port keeps upstream semantics exactly: task claim protocol (BEGIN
// IMMEDIATE + counters flush), --match/--glob byte comparisons, the 30s
// no-work give-up, the 2s shutdown grace, --exit N>0 as a crash that leaves a
// hot journal for the survivors to recover, journal-mode matrix tasks, and
// the busy-handler timeout. Children are real processes (threads would share
// locks and test nothing); `fstest __sqlite-mptest` is the hidden helper mode
// they are spawned as.
//
// Per script the parent prints `BEGIN:`, per-test logs with `NNNNN.mptest:` /
// `NNNNN.clientNN:` prefixes, and finishes with `Summary: N errors out of M
// tests` + `END:` (exit code 1 iff errors). fstest runs one fresh database
// per script under <mount>/.fstest-sqlite/, enforces a per-script process-
// group timeout, parses the summary into the standard JSON envelope and
// cleans the work directory under a time bound.

use crate::smallfile::iso8601;
use log::{info, warn};
use rusqlite::Connection;
use rusqlite::ffi;
use serde_json::{Value, json};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs;
use std::io::{Read as _, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

// embedded mptest scripts, verbatim from the SQLite source tree (public domain)
const SCRIPTS: &[(&str, &str)] = &[
    ("config01.test", include_str!("../vendor/sqlite/mptest/config01.test")),
    ("config02.test", include_str!("../vendor/sqlite/mptest/config02.test")),
    ("crash01.test", include_str!("../vendor/sqlite/mptest/crash01.test")),
    (
        "crash02.subtest",
        include_str!("../vendor/sqlite/mptest/crash02.subtest"),
    ),
    (
        "multiwrite01.test",
        include_str!("../vendor/sqlite/mptest/multiwrite01.test"),
    ),
];

const DEFAULT_TIMEOUT: i32 = 10000;
const SQLITE_BUSY: c_int = 5;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_NOTICE: c_int = 27; // extended code 256+27; the &0xff form used below
const SQLITE_CONFIG_LOG: c_int = 16;
const SQLITE_FCNTL_VFSNAME: c_int = 12;

// ---------- global state (mptest.c uses plain globals; the small mutable
// ones are atomics so message paths and the busy handler can never deadlock
// on a re-entrant lock) ----------

enum LogTarget {
    Stdout,
    Stderr,
    File(fs::File),
}

struct LogPair {
    log: LogTarget,
    err_log: LogTarget,
    log_name: Option<String>,
    err_log_name: Option<String>,
}

impl LogPair {
    const fn new() -> Self {
        LogPair {
            log: LogTarget::Stdout,
            err_log: LogTarget::Stderr,
            log_name: None,
            err_log_name: None,
        }
    }
}

struct Statics {
    argv0: OnceLock<PathBuf>,
    db_file: OnceLock<String>,
    vfs: OnceLock<String>,
    name: OnceLock<String>,
    i_trace: AtomicI32,
    i_timeout: AtomicI32,
    b_sql_trace: AtomicBool,
    b_sync: AtomicBool,
    b_ignore_sql_errors: AtomicBool,
    n_error: AtomicU64,
    n_test: AtomicU64,
    logs: Mutex<LogPair>,
}

static G: Statics = Statics {
    argv0: OnceLock::new(),
    db_file: OnceLock::new(),
    vfs: OnceLock::new(),
    name: OnceLock::new(),
    i_trace: AtomicI32::new(1),
    i_timeout: AtomicI32::new(0),
    b_sql_trace: AtomicBool::new(false),
    b_sync: AtomicBool::new(false),
    b_ignore_sql_errors: AtomicBool::new(false),
    n_error: AtomicU64::new(0),
    n_test: AtomicU64::new(0),
    logs: Mutex::new(LogPair::new()),
};

fn sqlite_sleep(ms: i32) {
    if ms > 0 {
        std::thread::sleep(Duration::from_millis(ms as u64));
    }
}

fn print_with_prefix(out: &mut dyn Write, prefix: &str, msg: &str) {
    for line in msg.split('\n') {
        let _ = writeln!(out, "{prefix}{line}");
    }
    let _ = out.flush();
}

fn with_log_sink<R>(target: &mut LogTarget, f: impl FnOnce(&mut dyn Write) -> R) -> R {
    match target {
        LogTarget::Stdout => f(&mut std::io::stdout()),
        LogTarget::Stderr => f(&mut std::io::stderr()),
        LogTarget::File(file) => f(file),
    }
}

fn error_message(msg: String) {
    G.n_error.fetch_add(1, Ordering::SeqCst);
    let prefix = format!("{}:ERROR: ", G.name.get().map(String::as_str).unwrap_or("mptest"));
    let mut logs = G.logs.lock().unwrap();
    with_log_sink(&mut logs.log, |w| print_with_prefix(w, &prefix, &msg));
    if logs.log_name != logs.err_log_name {
        with_log_sink(&mut logs.err_log, |w| print_with_prefix(w, &prefix, &msg));
    }
}

fn log_message(msg: String) {
    let prefix = format!("{}: ", G.name.get().map(String::as_str).unwrap_or("mptest"));
    let mut logs = G.logs.lock().unwrap();
    with_log_sink(&mut logs.log, |w| print_with_prefix(w, &prefix, &msg));
}

fn fatal_error(conn: Option<&Connection>, msg: String) -> ! {
    {
        let prefix = format!("{}:FATAL: ", G.name.get().map(String::as_str).unwrap_or("mptest"));
        let mut logs = G.logs.lock().unwrap();
        with_log_sink(&mut logs.log, |w| print_with_prefix(w, &prefix, &msg));
        if logs.log_name != logs.err_log_name {
            with_log_sink(&mut logs.err_log, |w| print_with_prefix(w, &prefix, &msg));
        }
    }
    if let Some(conn) = conn {
        G.i_timeout.store(0, Ordering::SeqCst);
        for _ in 0..100 {
            let rc = try_sql(conn, "UPDATE client SET wantHalt=1");
            if rc != SQLITE_BUSY {
                break;
            }
            sqlite_sleep(10);
        }
    }
    std::process::exit(1);
}

// ---------- strglob: mptest.c's glob (verbatim port; `#` matches a run of
// digits with optional sign, unlike standard glob) ----------

fn strglob(glob: &[u8], s: &[u8]) -> bool {
    fn inner(g: &[u8], z: &[u8]) -> bool {
        let mut gi = 0;
        let mut zi = 0;
        loop {
            let c = if gi < g.len() { g[gi] } else { 0 };
            gi += 1;
            match c {
                0 => return zi == z.len(),
                b'*' => {
                    loop {
                        let c = if gi < g.len() { g[gi] } else { 0 };
                        if c == b'*' {
                            gi += 1;
                        } else if c == b'?' {
                            gi += 1;
                            if zi >= z.len() {
                                return false;
                            }
                            zi += 1;
                        } else {
                            break;
                        }
                    }
                    let c = if gi < g.len() { g[gi] } else { 0 };
                    gi += 1;
                    if c == 0 {
                        return true;
                    } else if c == b'[' {
                        while zi < z.len() && inner(&g[gi..], &z[zi..]) {
                            zi += 1;
                        }
                        return zi < z.len();
                    }
                    loop {
                        if zi >= z.len() {
                            return false;
                        }
                        let c2 = z[zi];
                        zi += 1;
                        if c2 == c && inner(&g[gi..], &z[zi..]) {
                            return true;
                        }
                    }
                }
                b'?' => {
                    if zi >= z.len() {
                        return false;
                    }
                    zi += 1;
                }
                b'[' => {
                    if zi >= z.len() {
                        return false;
                    }
                    let ch = z[zi];
                    zi += 1;
                    let mut prior: i32 = 0;
                    let mut seen = false;
                    let mut invert = false;
                    let mut c2 = if gi < g.len() { g[gi] } else { 0 };
                    gi += 1;
                    if c2 == b'^' {
                        invert = true;
                        c2 = if gi < g.len() { g[gi] } else { 0 };
                        gi += 1;
                    }
                    if c2 == b']' {
                        if ch == b']' {
                            seen = true;
                        }
                        c2 = if gi < g.len() { g[gi] } else { 0 };
                        gi += 1;
                    }
                    while c2 != 0 && c2 != b']' {
                        if c2 == b'-' && gi < g.len() && g[gi] != b']' && prior > 0 {
                            c2 = if gi < g.len() { g[gi] } else { 0 };
                            gi += 1;
                            if ch as i32 >= prior && ch as i32 <= c2 as i32 {
                                seen = true;
                            }
                            prior = 0;
                        } else {
                            if ch == c2 {
                                seen = true;
                            }
                            prior = c2 as i32;
                        }
                        c2 = if gi < g.len() { g[gi] } else { 0 };
                        gi += 1;
                    }
                    if c2 == 0 || seen == invert {
                        return false;
                    }
                }
                b'#' => {
                    if zi < z.len()
                        && (z[zi] == b'-' || z[zi] == b'+')
                        && zi + 1 < z.len()
                        && z[zi + 1].is_ascii_digit()
                    {
                        zi += 1;
                    }
                    if zi >= z.len() || !z[zi].is_ascii_digit() {
                        return false;
                    }
                    zi += 1;
                    while zi < z.len() && z[zi].is_ascii_digit() {
                        zi += 1;
                    }
                }
                _ => {
                    if zi >= z.len() || z[zi] != c {
                        return false;
                    }
                    zi += 1;
                }
            }
        }
    }
    inner(glob, s)
}

// ---------- the term string (mptest.c's String): query results accumulate
// here, space-separated, terms containing whitespace get quoted ----------

#[derive(Default)]
struct TermString {
    buf: Vec<u8>,
}

impl TermString {
    fn reset(&mut self) {
        self.buf.clear();
    }

    fn append_term(&mut self, term: Option<&[u8]>) {
        if !self.buf.is_empty() {
            self.buf.push(b' ');
        }
        let Some(z) = term else {
            self.buf.extend_from_slice(b"nil");
            return;
        };
        let i = z.iter().position(|&c| is_space(c)).unwrap_or(z.len());
        if i > 0 && i == z.len() {
            self.buf.extend_from_slice(z);
            return;
        }
        self.buf.push(b'\'');
        let mut rest = z;
        loop {
            match rest.iter().position(|&c| c == b'\'') {
                Some(j) => {
                    self.buf.extend_from_slice(&rest[..=j]);
                    self.buf.push(b'\'');
                    rest = &rest[j + 1..];
                }
                None => {
                    self.buf.extend_from_slice(rest);
                    break;
                }
            }
        }
        self.buf.push(b'\'');
    }
}

// ---------- raw sqlite plumbing (multi-statement exec like sqlite3_exec) ----------

type ExecCb = unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int;

// the raw engine handle; safe because the connection outlives every call site
// and all uses stay on the owning thread
fn dbh(conn: &Connection) -> *mut ffi::sqlite3 {
    unsafe { conn.handle() }
}

fn exec_raw(db: *mut ffi::sqlite3, sql: &[u8], cb: Option<ExecCb>, out: *mut c_void) -> (c_int, Option<String>) {
    let Ok(csql) = CString::new(strip_trailing_nul(sql)) else {
        return (ffi::SQLITE_MISUSE, Some("sql contains NUL".into()));
    };
    unsafe {
        let mut errmsg: *mut c_char = std::ptr::null_mut();
        let rc = ffi::sqlite3_exec(db, csql.as_ptr(), cb, out, &mut errmsg);
        let err = if errmsg.is_null() {
            None
        } else {
            let s = CStr::from_ptr(errmsg).to_string_lossy().into_owned();
            ffi::sqlite3_free(errmsg as *mut c_void);
            Some(s)
        };
        (rc, err)
    }
}

fn strip_trailing_nul(sql: &[u8]) -> Vec<u8> {
    let mut v = sql.to_vec();
    while v.last() == Some(&0) {
        v.pop();
    }
    v
}

fn last_errmsg(db: *mut ffi::sqlite3) -> String {
    unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(db)).to_string_lossy().into_owned() }
}

// runSql: fatal on failure
fn run_sql(conn: &Connection, sql: &str) {
    let (rc, _) = exec_raw(dbh(&conn), sql.as_bytes(), None, std::ptr::null_mut());
    if rc != ffi::SQLITE_OK {
        fatal_error(Some(conn), format!("{}\n{}\n", last_errmsg(dbh(&conn)), sql));
    }
}

// trySql: return the rc
fn try_sql(conn: &Connection, sql: &str) -> c_int {
    let (rc, _) = exec_raw(dbh(&conn), sql.as_bytes(), None, std::ptr::null_mut());
    rc
}

unsafe extern "C" fn eval_cb(p: *mut c_void, argc: c_int, argv: *mut *mut c_char, _col: *mut *mut c_char) -> c_int {
    unsafe {
        let out = &mut *(p as *mut TermString);
        for i in 0..argc {
            let cell = *argv.add(i as usize);
            let term = if cell.is_null() {
                None
            } else {
                Some(CStr::from_ptr(cell).to_bytes())
            };
            out.append_term(term);
        }
    }
    0
}

// evalSql: run SQL, appending each result cell as a term; errors land in the
// result as an `error(N)` term (never fatal)
fn eval_sql(conn: &Connection, out: &mut TermString, sql: &[u8]) {
    let (rc, err) = exec_raw(dbh(&conn), sql, Some(eval_cb), out as *mut TermString as *mut c_void);
    if rc != 0 {
        out.append_term(Some(format!("error({rc})").as_bytes()));
        if let Some(e) = err {
            out.append_term(Some(e.as_bytes()));
        }
    }
}

fn transient() -> ffi::sqlite3_destructor_type {
    unsafe { std::mem::transmute::<isize, ffi::sqlite3_destructor_type>(-1isize) }
}

// vfsname(): the FCNTL_VFSNAME chain, e.g. "unix" — scripts gate on it
unsafe extern "C" fn vfs_name_func(ctx: *mut ffi::sqlite3_context, _argc: c_int, _argv: *mut *mut ffi::sqlite3_value) {
    unsafe {
        let db = ffi::sqlite3_context_db_handle(ctx);
        let mut z: *mut c_char = std::ptr::null_mut();
        ffi::sqlite3_file_control(
            db,
            b"main\0".as_ptr() as *const c_char,
            SQLITE_FCNTL_VFSNAME,
            &mut z as *mut *mut c_char as *mut c_void,
        );
        if !z.is_null() {
            ffi::sqlite3_result_text(ctx, z, -1, Some(ffi::sqlite3_free));
        }
    }
}

// eval(sql): recursive SQL evaluation, used by config01's page-size checks
unsafe extern "C" fn eval_func(ctx: *mut ffi::sqlite3_context, _argc: c_int, argv: *mut *mut ffi::sqlite3_value) {
    unsafe {
        let db = ffi::sqlite3_context_db_handle(ctx);
        let z_sql = ffi::sqlite3_value_text(*argv);
        let mut res = TermString::default();
        let mut errmsg: *mut c_char = std::ptr::null_mut();
        let rc = ffi::sqlite3_exec(
            db,
            z_sql as *const c_char,
            Some(eval_cb),
            &mut res as *mut TermString as *mut c_void,
            &mut errmsg,
        );
        if !errmsg.is_null() {
            ffi::sqlite3_result_error(ctx, errmsg, -1);
            ffi::sqlite3_free(errmsg as *mut c_void);
        } else if rc != 0 {
            ffi::sqlite3_result_error_code(ctx, rc);
        } else {
            res.buf.push(0);
            let c = CString::from_vec_unchecked(std::mem::take(&mut res.buf));
            ffi::sqlite3_result_text(ctx, c.as_ptr(), -1, transient());
        }
    }
}

// busy handler with the g.iTimeout-millisecond timeout
fn busy_handler(count: i32) -> bool {
    let timeout = G.i_timeout.load(Ordering::SeqCst);
    if count * 10 > timeout {
        if timeout > 0 {
            error_message(format!("timeout after {timeout}ms"));
        }
        return false;
    }
    sqlite_sleep(10);
    true
}

// global error log (SQLITE_CONFIG_LOG), mirrors sqlErrorCallback
unsafe extern "C" fn sql_error_cb(_p: *mut c_void, code: c_int, msg: *const c_char) {
    unsafe {
        let msg = if msg.is_null() {
            String::new()
        } else {
            CStr::from_ptr(msg).to_string_lossy().into_owned()
        };
        let ignore = G.b_ignore_sql_errors.load(Ordering::SeqCst);
        let trace = G.i_trace.load(Ordering::SeqCst);
        let timeout = G.i_timeout.load(Ordering::SeqCst);
        if code == ffi::SQLITE_ERROR && ignore {
            return;
        }
        if code & 0xff == ffi::SQLITE_SCHEMA && trace < 3 {
            return;
        }
        if timeout == 0 && code & 0xff == SQLITE_BUSY && trace < 3 {
            return;
        }
        if code & 0xff == SQLITE_NOTICE {
            log_message(format!("(info) {msg}"));
        } else {
            error_message(format!("(errcode={code}) {msg}"));
        }
    }
}

fn clip_length(s: &str) -> &str {
    s.trim_end_matches(|c: char| c.is_ascii_whitespace())
}

// ---------- script tokenizer (verbatim port of tokenLength et al) ----------

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn token_length(z: &[u8], pn_line: &mut i32) -> usize {
    if z.is_empty() {
        return 1; // mirrors the C else-branch at end-of-string
    }
    let mut n: usize;
    if is_space(z[0]) || (z[0] == b'/' && z.len() > 1 && z[1] == b'*') {
        let mut in_c = false;
        if z[0] == b'/' {
            in_c = true;
            n = 2;
        } else {
            n = 0;
        }
        loop {
            if n >= z.len() {
                n += 1;
                break;
            }
            let c = z[n];
            n += 1;
            if c == b'\n' {
                *pn_line += 1;
            }
            if is_space(c) {
                continue;
            }
            if in_c && c == b'*' && n < z.len() && z[n] == b'/' {
                n += 1;
                in_c = false;
            } else if !in_c && c == b'/' && n < z.len() && z[n] == b'*' {
                n += 1;
                in_c = true;
            } else if !in_c {
                break;
            }
        }
        n -= 1;
    } else if z[0] == b'-' && z.len() > 1 && z[1] == b'-' {
        n = 2;
        while n < z.len() && z[n] != b'\n' {
            n += 1;
        }
        if n < z.len() {
            *pn_line += 1;
            n += 1;
        }
    } else if z[0] == b'"' || z[0] == b'\'' {
        let delim = z[0];
        n = 1;
        while n < z.len() {
            if z[n] == b'\n' {
                *pn_line += 1;
            }
            if z[n] == delim {
                n += 1;
                if n >= z.len() || z.get(n + 1).copied().unwrap_or(0) != delim {
                    break;
                }
            }
            n += 1;
        }
    } else {
        n = 1;
        while n < z.len() {
            let c = z[n];
            if is_space(c) || c == b'"' || c == b'\'' || c == b';' {
                break;
            }
            n += 1;
        }
    }
    n
}

fn extract_token(z: &[u8], n_in: usize, cap: usize) -> String {
    let mut s = Vec::new();
    let mut i = 0;
    while i < n_in && i < z.len() && i < cap && !is_space(z[i]) {
        s.push(z[i]);
        i += 1;
    }
    String::from_utf8_lossy(&s).into_owned()
}

// bytes up to the start of the next "--end" token
fn find_end(z: &[u8], pn_line: &mut i32) -> usize {
    let mut n = 0;
    while n < z.len() && !(z[n..].starts_with(b"--end") && n + 5 < z.len() && is_space(z[n + 5])) {
        n += token_length(&z[n..], pn_line);
    }
    n
}

// bytes past the next --endif (or --else when stopping there), skipping nested --if
fn find_endif(z: &[u8], stop_at_else: bool, pn_line: &mut i32) -> usize {
    let mut n = 0;
    while n < z.len() {
        let len = token_length(&z[n..], pn_line);
        let is_endif = z[n..].starts_with(b"--endif") && n + 7 < z.len() && is_space(z[n + 7]);
        let is_else = stop_at_else && z[n..].starts_with(b"--else") && n + 6 < z.len() && is_space(z[n + 6]);
        if is_endif || is_else {
            return n + len;
        }
        if z[n..].starts_with(b"--if") && n + 4 < z.len() && is_space(z[n + 4]) {
            let skip = find_endif(&z[n + len..], false, pn_line);
            n += skip + len;
        } else {
            n += len;
        }
    }
    n
}

// C atoi: leading [+-]?digits of a token, 0 otherwise
fn atoi(s: &str) -> i64 {
    let t = s.trim_start();
    let mut end = 0;
    let b = t.as_bytes();
    if end < b.len() && (b[end] == b'+' || b[end] == b'-') {
        end += 1;
    }
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    t[..end].parse().unwrap_or(0)
}

fn boolean_value(arg: Option<&str>) -> i32 {
    let Some(arg) = arg else { return 0 };
    if atoi(arg) != 0
        && arg.bytes().all(|c| c.is_ascii_digit() || c == b'+' || c == b'-')
        && arg.bytes().any(|c| c.is_ascii_digit())
    {
        return atoi(arg) as i32;
    }
    match arg.to_ascii_lowercase().as_str() {
        "on" | "yes" => 1,
        "off" | "no" => 0,
        _ => {
            error_message(format!("unknown boolean: [{arg}]"));
            0
        }
    }
}

fn filename_tail(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

// resolve --source targets: embedded map first, then the filesystem relative
// to the sourcing file
fn read_source(from_filename: &str, arg: &str) -> (String, Vec<u8>) {
    let fail = |name: String| -> (String, Vec<u8>) {
        fatal_error(None, format!("cannot open \"{name}\" for reading"));
    };
    if let Some((_, content)) = SCRIPTS.iter().find(|(n, _)| *n == arg) {
        return (arg.to_string(), content.as_bytes().to_vec());
    }
    if arg.starts_with('/') {
        let p = Path::new(arg);
        if !p.is_file() {
            return fail(arg.to_string());
        }
        return (arg.to_string(), fs::read(p).unwrap_or_default());
    }
    let dir = match from_filename.rfind('/') {
        Some(k) if k > 0 => &from_filename[..k],
        _ => "",
    };
    let joined = if dir.is_empty() {
        arg.to_string()
    } else {
        format!("{dir}/{arg}")
    };
    match fs::read(&joined) {
        Ok(b) => (joined, b),
        Err(_) => fail(joined),
    }
}

// ---------- runScript: the interpreter ----------

const MX_ARG: usize = 2;

fn run_script(conn: &Connection, i_client: i64, task_id: i64, script: &[u8], filename: &str) {
    let z = script;
    let mut lineno: i32 = 1;
    #[allow(unused_assignments)]
    let mut prev_line: i32 = 1;
    let mut ii = 0usize;
    let mut i_begin = 0usize;
    let mut s_result = TermString::default();
    while ii < z.len() {
        prev_line = lineno;
        let c = z[ii];
        let len = token_length(&z[ii..], &mut lineno);
        if is_space(c) || (c == b'/' && ii + 1 < z.len() && z[ii + 1] == b'*') {
            ii += len;
            continue;
        }
        if c != b'-' || ii + 1 >= z.len() || z[ii + 1] != b'-' || ii + 2 >= z.len() || !z[ii + 2].is_ascii_alphabetic()
        {
            ii += len;
            continue;
        }

        // run any prior SQL before processing the new --command
        if ii > i_begin {
            eval_sql(conn, &mut s_result, &z[i_begin..ii]);
            i_begin = ii + len;
        }

        // parse the --command (zCmd up to 29 bytes, args up to 99, max 2 — the C buffers)
        let span = &z[ii + 2..(ii + len).min(z.len())];
        let z_cmd = extract_token(span, span.len(), 29);
        let mut n = z_cmd.len();
        let mut args: [String; MX_ARG] = [String::new(), String::new()];
        let mut n_arg = 0usize;
        while n < span.len() && n_arg < MX_ARG {
            while n < span.len() && is_space(span[n]) {
                n += 1;
            }
            if n >= span.len() {
                break;
            }
            let tok = extract_token(&span[n..], span.len() - n, 99);
            n += tok.len();
            args[n_arg] = tok;
            n_arg += 1;
        }

        if G.i_trace.load(Ordering::SeqCst) >= 2 {
            log_message(String::from_utf8_lossy(&z[ii..(ii + len).min(z.len())]).into_owned());
        }

        let mut advance = len;
        match z_cmd.as_str() {
            // --sleep N: pause for N milliseconds
            "sleep" => {
                sqlite_sleep(atoi(&args[0]) as i32);
            }
            // --exit N: exit this process; N>0 exits WITHOUT closing the
            // database — a simulated crash leaving a hot journal
            "exit" => {
                let rc = atoi(&args[0]);
                finish_script(conn, i_client, task_id, true);
                std::process::exit(rc as i32);
            }
            // --testcase NAME: begin a new test case
            "testcase" => {
                if G.i_trace.load(Ordering::SeqCst) == 1 {
                    log_message(String::from_utf8_lossy(&z[ii..(ii + len - 1).min(z.len())]).into_owned());
                }
                s_result.reset();
            }
            // --finish: mark the current task finished even if it is not
            // (crash simulation with --exit)
            "finish" if i_client > 0 => {
                finish_script(conn, i_client, task_id, true);
            }
            // --reset: clear accumulated results
            "reset" => {
                s_result.reset();
            }
            // --match ANSWER...: byte-exact compare of the accumulated result
            "match" => {
                let mut jj = 7usize;
                while jj < len - 1 && is_space(z[ii + jj]) {
                    jj += 1;
                }
                let end = (ii + len - 1).min(z.len());
                let want = &z[(ii + jj).min(end)..end];
                if want != &s_result.buf[..] {
                    error_message(format!(
                        "line {} of {}:\nExpected [{}]\n     Got [{}]",
                        prev_line,
                        filename,
                        String::from_utf8_lossy(want),
                        String::from_utf8_lossy(&s_result.buf),
                    ));
                }
                G.n_test.fetch_add(1, Ordering::SeqCst);
                s_result.reset();
            }
            // --glob ANSWER / --notglob ANSWER
            "glob" | "notglob" => {
                let is_glob = z_cmd == "glob";
                let mut jj = if is_glob { 6 } else { 9 };
                while jj < len - 1 && is_space(z[ii + jj]) {
                    jj += 1;
                }
                let end = (ii + len - 1).min(z.len());
                let want = &z[(ii + jj).min(end)..end];
                if strglob(want, &s_result.buf) != is_glob {
                    error_message(format!(
                        "line {} of {}:\nExpected [{}]\n     Got [{}]",
                        prev_line,
                        filename,
                        String::from_utf8_lossy(want),
                        String::from_utf8_lossy(&s_result.buf),
                    ));
                }
                G.n_test.fetch_add(1, Ordering::SeqCst);
                s_result.reset();
            }
            // --output: log the previous result
            "output" => {
                log_message(String::from_utf8_lossy(&s_result.buf).into_owned());
            }
            // --source FILENAME: run a subscript
            "source" => {
                let (new_file, new_script) = read_source(filename, &args[0]);
                if G.i_trace.load(Ordering::SeqCst) != 0 {
                    log_message(format!("begin script [{new_file}]"));
                }
                run_script(conn, 0, 0, &new_script, &new_file);
                if G.i_trace.load(Ordering::SeqCst) != 0 {
                    log_message(format!("end script [{new_file}]"));
                }
            }
            // --print MESSAGE
            "print" => {
                let mut jj = 7usize;
                while jj < len && is_space(z[ii + jj]) {
                    jj += 1;
                }
                log_message(String::from_utf8_lossy(&z[(ii + jj).min(z.len())..(ii + len).min(z.len())]).into_owned());
            }
            // --if EXPR: SQL expression; false skips to the matching --else/--endif
            "if" => {
                let mut jj = 4usize;
                while jj < len && is_space(z[ii + jj]) {
                    jj += 1;
                }
                let expr = String::from_utf8_lossy(&z[(ii + jj).min(z.len())..(ii + len).min(z.len())]);
                let sql = format!("SELECT {}", expr.trim_end());
                match conn.query_row(&sql, [], |r| r.get::<_, i32>(0)) {
                    Ok(0) | Err(_) => {
                        ii += find_endif(&z[ii + len..], true, &mut lineno);
                    }
                    Ok(_) => {}
                }
            }
            // --else: reached while inside a true --if; skip to --endif
            "else" => {
                ii += find_endif(&z[ii + len..], false, &mut lineno);
            }
            // --endif: no-op
            "endif" => {}
            // --start CLIENT
            "start" if i_client == 0 => {
                let target = atoi(&args[0]);
                if target > 0 {
                    start_client(conn, target);
                }
            }
            // --wait CLIENT TIMEOUT
            "wait" if i_client == 0 => {
                let timeout = if n_arg >= 2 { atoi(&args[1]) as i32 } else { 10000 };
                wait_for_client(
                    conn,
                    atoi(&args[0]),
                    timeout,
                    &format!("line {prev_line} of {filename}\n"),
                );
            }
            // --task CLIENT ?NAME?
            "task" if i_client == 0 => {
                let target = atoi(&args[0]);
                let i_end = find_end(&z[ii + len..], &mut lineno);
                if target < 0 {
                    error_message(format!("line {prev_line} of {filename}: bad client number: {target}"));
                } else {
                    let task = String::from_utf8_lossy(&z[ii + len..ii + len + i_end]);
                    let t_name = if n_arg > 1 {
                        args[1].clone()
                    } else {
                        format!("{}:{}", filename_tail(filename), prev_line)
                    };
                    start_client(conn, target);
                    // %q / %Q quoting from the C mprintf: double single quotes
                    run_sql(
                        conn,
                        &format!(
                            "INSERT INTO task(client,script,name) VALUES({},'{}','{}')",
                            target,
                            task.replace('\'', "''"),
                            t_name.replace('\'', "''")
                        ),
                    );
                }
                advance = len + i_end + token_length(&z[ii + len + i_end..], &mut lineno);
                i_begin = ii + advance;
            }
            // --breakpoint: debugger hook
            "breakpoint" => {}
            // --show-sql-errors BOOLEAN
            "show-sql-errors" => {
                G.b_ignore_sql_errors.store(
                    if n_arg >= 1 {
                        boolean_value(Some(&args[0])) == 0
                    } else {
                        true
                    },
                    Ordering::SeqCst,
                );
            }
            _ => {
                error_message(format!("line {prev_line} of {filename}: unknown command --{z_cmd}"));
            }
        }
        ii += advance;
    }
    if i_begin < ii {
        run_sql(conn, &String::from_utf8_lossy(&z[i_begin..ii]));
    }
}

// ---------- task/client coordination ----------

enum StartResult {
    Task { script: String, id: i64, name: String },
    Done,
}

// claim the next task for this client inside BEGIN IMMEDIATE, flushing local
// counters into the shared counters table on the way
fn start_script(conn: &Connection, i_client: i64) -> StartResult {
    G.i_timeout.store(0, Ordering::SeqCst);
    let mut total_time: i32 = 0;
    loop {
        let rc = try_sql(conn, "BEGIN IMMEDIATE");
        if rc == SQLITE_BUSY {
            sqlite_sleep(10);
            total_time += 10;
            continue;
        }
        if rc != ffi::SQLITE_OK {
            fatal_error(Some(conn), format!("in startScript: {}", last_errmsg(dbh(&conn))));
        }
        {
            let n_error = G.n_error.swap(0, Ordering::SeqCst);
            let n_test = G.n_test.swap(0, Ordering::SeqCst);
            if n_error != 0 || n_test != 0 {
                run_sql(
                    conn,
                    &format!("UPDATE counters SET nError=nError+{n_error}, nTest=nTest+{n_test}"),
                );
            }
        }
        let halted: Option<i32> = conn
            .query_row(
                &format!("SELECT 1 FROM client WHERE id={i_client} AND wantHalt"),
                [],
                |r| r.get(0),
            )
            .ok();
        if halted.is_some() {
            run_sql(conn, &format!("DELETE FROM client WHERE id={i_client}"));
            G.i_timeout.store(DEFAULT_TIMEOUT, Ordering::SeqCst);
            run_sql(conn, "COMMIT TRANSACTION;");
            return StartResult::Done;
        }
        let task = conn
            .query_row(
                &format!(
                    "SELECT script, id, name FROM task WHERE client={i_client} AND starttime IS NULL ORDER BY id LIMIT 1"
                ),
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)),
            )
            .ok();
        if let Some((script, id, name)) = task {
            run_sql(
                conn,
                &format!("UPDATE task SET starttime=strftime('%Y-%m-%d %H:%M:%f','now') WHERE id={id};"),
            );
            G.i_timeout.store(DEFAULT_TIMEOUT, Ordering::SeqCst);
            run_sql(conn, "COMMIT TRANSACTION;");
            return StartResult::Task { script, id, name };
        }
        // no task waiting: hold the transaction open and poll
        if total_time > 30000 {
            error_message("Waited over 30 seconds with no work.  Giving up.".into());
            run_sql(conn, &format!("DELETE FROM client WHERE id={i_client}; COMMIT;"));
            std::process::exit(1);
        }
        while try_sql(conn, "COMMIT") == SQLITE_BUSY {
            sqlite_sleep(10);
            total_time += 10;
        }
        sqlite_sleep(100);
        total_time += 100;
    }
}

fn finish_script(conn: &Connection, i_client: i64, task_id: i64, b_shutdown: bool) {
    run_sql(
        conn,
        &format!("UPDATE task SET endtime=strftime('%Y-%m-%d %H:%M:%f','now') WHERE id={task_id};"),
    );
    if b_shutdown {
        run_sql(conn, &format!("DELETE FROM client WHERE id={i_client}"));
    }
}

// spawn a real client process for i_client if not already running — the
// INSERT OR IGNORE + changes() dance is the claim
fn start_client(conn: &Connection, i_client: i64) {
    run_sql(conn, &format!("INSERT OR IGNORE INTO client VALUES({i_client},0)"));
    if unsafe { ffi::sqlite3_changes(dbh(&conn)) } != 0 {
        let mut cmd = Command::new(
            G.argv0
                .get()
                .map(|p| p.as_path())
                .unwrap_or_else(|| Path::new("fstest")),
        );
        cmd.arg("__sqlite-mptest")
            .arg(G.db_file.get().map(String::as_str).unwrap_or(""))
            .args([
                "--client",
                &i_client.to_string(),
                "--trace",
                &G.i_trace.load(Ordering::SeqCst).to_string(),
            ]);
        if G.b_sql_trace.load(Ordering::SeqCst) {
            cmd.arg("--sqltrace");
        }
        if G.b_sync.load(Ordering::SeqCst) {
            cmd.arg("--sync");
        }
        if let Some(vfs) = G.vfs.get() {
            cmd.args(["--vfs", vfs]);
        }
        if G.i_trace.load(Ordering::SeqCst) >= 2 {
            log_message(format!("system('{:?}')", cmd));
        }
        // spawned and deliberately not awaited (the C backgrounds it too);
        // children exit on their own and are reaped when the master exits
        if let Err(e) = cmd.spawn() {
            error_message(format!("system() fails with error: {e}"));
        }
    }
}

// wait until all tasks of one client (or all clients) are finished
fn wait_for_client(conn: &Connection, i_client: i64, mut timeout: i32, err_prefix: &str) {
    let sql = if i_client > 0 {
        format!("SELECT 1 FROM task WHERE client={i_client} AND client IN (SELECT id FROM client) AND endtime IS NULL")
    } else {
        "SELECT 1 FROM task WHERE client IN (SELECT id FROM client) AND endtime IS NULL".to_string()
    };
    G.i_timeout.store(0, Ordering::SeqCst);
    let final_rc;
    unsafe {
        let db = dbh(&conn);
        let Ok(csql) = CString::new(sql.clone()) else { return };
        let mut stmt: *mut ffi::sqlite3_stmt = std::ptr::null_mut();
        if ffi::sqlite3_prepare_v2(db, csql.as_ptr(), -1, &mut stmt, std::ptr::null_mut()) != ffi::SQLITE_OK {
            fatal_error(Some(conn), format!("{}\n{}\n", last_errmsg(db), sql));
        }
        let mut rc = ffi::sqlite3_step(stmt);
        while (rc == SQLITE_BUSY || rc == SQLITE_ROW) && timeout > 0 {
            ffi::sqlite3_reset(stmt);
            sqlite_sleep(50);
            timeout -= 50;
            rc = ffi::sqlite3_step(stmt);
        }
        final_rc = rc;
        ffi::sqlite3_finalize(stmt);
    }
    G.i_timeout.store(DEFAULT_TIMEOUT, Ordering::SeqCst);
    if final_rc != SQLITE_DONE {
        if i_client > 0 {
            error_message(format!("{err_prefix}timeout waiting for client {i_client}"));
        } else {
            error_message(format!("{err_prefix}timeout waiting for all clients"));
        }
    }
}

// ---------- option handling ----------

fn find_option(args: &mut Vec<String>, option: &str, has_arg: bool) -> Option<String> {
    for i in 0..args.len() {
        if i + has_arg as usize >= args.len() {
            break;
        }
        let mut z = args[i].as_str();
        if !z.starts_with('-') {
            continue;
        }
        z = &z[1..];
        if z.starts_with('-') {
            if z.len() == 1 {
                break;
            }
            z = &z[1..];
        }
        if z == option {
            let ret = if has_arg {
                Some(args[i + 1].clone())
            } else {
                Some(args[i].clone())
            };
            args.drain(i..i + 1 + has_arg as usize);
            return ret;
        }
    }
    None
}

fn usage() -> ! {
    eprintln!("Usage: fstest __sqlite-mptest DATABASE ?OPTIONS? ?SCRIPT?");
    eprint!(
        "Options:\n   --errlog FILENAME           Write errors to FILENAME\n   --journalmode MODE          Use MODE as the journal_mode\n   --log FILENAME              Log messages to FILENAME\n   --quiet                     Suppress unnecessary output\n   --vfs NAME                  Use NAME as the VFS\n   --repeat N                  Repeat the test N times\n   --sqltrace                  Enable SQL tracing\n   --sync                      Enable synchronous disk writes\n   --timeout MILLISEC          Busy timeout is MILLISEC\n   --trace BOOLEAN             Enable or disable tracing\n"
    );
    std::process::exit(1);
}

fn unrecognized_arguments(argv0: &str, rest: &[String]) -> ! {
    eprint!("{argv0}: unrecognized arguments:");
    for a in rest {
        eprint!(" {a}");
    }
    eprintln!();
    std::process::exit(1);
}

// ---------- the engine entry point (mptest.c main) ----------
//
// argv is everything after `fstest __sqlite-mptest`: [db, ?script, ?options].
// As master, the script may be an embedded name (config01.test …) or a path.

pub fn mptest_main(args: &[String]) -> i32 {
    G.argv0
        .set(std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fstest")))
        .ok();
    let pid = std::process::id();
    if args.is_empty() {
        usage();
    }
    let db_file = args[0].clone();
    G.db_file.set(db_file.clone()).ok();
    if strglob(b"*.test", db_file.as_bytes()) {
        usage();
    }
    let mut rest: Vec<String> = args[1..].to_vec();
    let z_jmode = find_option(&mut rest, "journalmode", true);
    let n_rep = find_option(&mut rest, "repeat", true)
        .map(|v| atoi(&v).max(1) as u32)
        .unwrap_or(1);
    let z_vfs = find_option(&mut rest, "vfs", true);
    if let Some(v) = &z_vfs {
        G.vfs.set(v.clone()).ok();
    }
    let z_client = find_option(&mut rest, "client", true);
    let z_errlog = find_option(&mut rest, "errlog", true);
    let z_log = find_option(&mut rest, "log", true);
    if let Some(t) = find_option(&mut rest, "trace", true) {
        G.i_trace.store(atoi(&t) as i32, Ordering::SeqCst);
    }
    if find_option(&mut rest, "quiet", false).is_some() {
        G.i_trace.store(0, Ordering::SeqCst);
    }
    let z_tmout = find_option(&mut rest, "timeout", true);
    if find_option(&mut rest, "sqltrace", false).is_some() {
        G.b_sql_trace.store(true, Ordering::SeqCst);
    }
    if find_option(&mut rest, "sync", false).is_some() {
        G.b_sync.store(true, Ordering::SeqCst);
    }
    {
        let open = |name: &Option<String>, default: LogTarget| -> LogTarget {
            match name {
                Some(n) => LogTarget::File(fs::OpenOptions::new().create(true).append(true).open(n).unwrap_or_else(
                    |e| {
                        eprintln!("mptest: cannot open log {n}: {e}");
                        std::process::exit(1);
                    },
                )),
                None => default,
            }
        };
        let mut logs = G.logs.lock().unwrap();
        logs.err_log = open(&z_errlog, LogTarget::Stderr);
        logs.err_log_name = z_errlog.clone();
        logs.log = open(&z_log, LogTarget::Stdout);
        logs.log_name = z_log.clone();
    }
    unsafe {
        ffi::sqlite3_config(
            SQLITE_CONFIG_LOG,
            sql_error_cb as unsafe extern "C" fn(*mut c_void, c_int, *const c_char),
            std::ptr::null_mut::<c_void>(),
        );
    }

    let i_client: i64;
    let mut open_create = false;
    match &z_client {
        Some(c) => {
            i_client = atoi(c);
            if i_client < 1 {
                fatal_error(None, format!("illegal client number: {i_client}\n"));
            }
            G.name.set(format!("{:05}.client{:02}", pid, i_client)).ok();
        }
        None => {
            i_client = 0;
            if G.i_trace.load(Ordering::SeqCst) > 0 {
                print!(
                    "BEGIN: {}",
                    G.argv0.get().map(|p| p.display().to_string()).unwrap_or_default()
                );
                for a in args {
                    print!(" {a}");
                }
                println!();
                unsafe {
                    println!(
                        "With SQLite {} {}",
                        CStr::from_ptr(ffi::sqlite3_libversion()).to_string_lossy(),
                        CStr::from_ptr(ffi::sqlite3_sourceid()).to_string_lossy()
                    );
                    let mut i = 0;
                    loop {
                        let opt = ffi::sqlite3_compileoption_get(i);
                        if opt.is_null() {
                            break;
                        }
                        println!("-DSQLITE_{}", CStr::from_ptr(opt).to_string_lossy());
                        i += 1;
                    }
                }
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
            }
            // the master unlinks a leftover database before starting; the loop
            // gives a wedged/warm filesystem a minute before giving up
            let mut n_try = 0;
            loop {
                if n_try % 5 == 4 && G.i_trace.load(Ordering::SeqCst) > 0 {
                    println!(
                        "... {}trying to unlink '{}'",
                        if n_try > 5 { "still " } else { "" },
                        db_file
                    );
                }
                match fs::remove_file(&db_file) {
                    Ok(()) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                    Err(_) => {}
                }
                n_try += 1;
                if n_try >= 60 {
                    break;
                }
                sqlite_sleep(1000);
            }
            if fs::symlink_metadata(&db_file).is_ok() {
                fatal_error(None, format!("unable to unlink '{}' after {n_try} attempts\n", db_file));
            }
            open_create = true;
        }
    }

    let conn = open_connection(&db_file, open_create);
    if let Some(t) = &z_tmout {
        if atoi(t) > 0 {
            let _ = conn.busy_timeout(Duration::from_millis(atoi(t).max(0) as u64));
        }
    }
    if let Some(jm) = &z_jmode {
        #[cfg(windows)]
        let jm = if jm.eq_ignore_ascii_case("persist") || jm.eq_ignore_ascii_case("truncate") {
            println!("Changing journal mode to DELETE from {jm}");
            "DELETE".to_string()
        } else {
            jm.clone()
        };
        run_sql(&conn, &format!("PRAGMA journal_mode='{jm}';"));
    }
    if !G.b_sync.load(Ordering::SeqCst) {
        try_sql(&conn, "PRAGMA synchronous=OFF");
    }
    unsafe {
        ffi::sqlite3_enable_load_extension(dbh(&conn), 1);
    }
    let _ = conn.busy_handler(Some(busy_handler));
    unsafe {
        let db = dbh(&conn);
        ffi::sqlite3_create_function_v2(
            db,
            c"vfsname".as_ptr(),
            0,
            1, /* SQLITE_UTF8 */
            std::ptr::null_mut(),
            Some(vfs_name_func),
            None,
            None,
            None,
        );
        ffi::sqlite3_create_function_v2(
            db,
            c"eval".as_ptr(),
            1,
            1,
            std::ptr::null_mut(),
            Some(eval_func),
            None,
            None,
            None,
        );
    }
    G.i_timeout.store(DEFAULT_TIMEOUT, Ordering::SeqCst);
    if G.b_sql_trace.load(Ordering::SeqCst) {
        conn.trace_v2(
            rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
            Some(|evt| {
                if let rusqlite::trace::TraceEvent::Stmt(_, sql) = evt {
                    log_message(format!("[{}]", clip_length(sql)));
                }
            }),
        );
    }

    if i_client > 0 {
        if !rest.is_empty() {
            unrecognized_arguments("mptest", &rest);
        }
        if G.i_trace.load(Ordering::SeqCst) != 0 {
            log_message("start-client".into());
        }
        loop {
            let StartResult::Task { script, id, name } = start_script(&conn, i_client) else {
                break;
            };
            if G.i_trace.load(Ordering::SeqCst) != 0 {
                log_message(format!("begin {name} ({id})"));
            }
            run_script(&conn, i_client, id, script.as_bytes(), &name);
            if G.i_trace.load(Ordering::SeqCst) != 0 {
                log_message(format!("end {name} ({id})"));
            }
            finish_script(&conn, i_client, id, false);
            sqlite_sleep(10);
        }
        if G.i_trace.load(Ordering::SeqCst) != 0 {
            log_message("end-client".into());
        }
    } else {
        if rest.is_empty() {
            fatal_error(Some(&conn), "missing script filename".into());
        }
        if rest.len() > 1 {
            unrecognized_arguments("mptest", &rest);
        }
        run_sql(
            &conn,
            "DROP TABLE IF EXISTS task;\nDROP TABLE IF EXISTS counters;\nDROP TABLE IF EXISTS client;\n\
             CREATE TABLE task(\n  id INTEGER PRIMARY KEY,\n  name TEXT,\n  client INTEGER,\n  starttime DATE,\n  endtime DATE,\n  script TEXT\n);\
             CREATE INDEX task_i1 ON task(client, starttime);\nCREATE INDEX task_i2 ON task(client, endtime);\n\
             CREATE TABLE counters(nError,nTest);\nINSERT INTO counters VALUES(0,0);\n\
             CREATE TABLE client(id INTEGER PRIMARY KEY, wantHalt);\n",
        );
        let script_ref = rest[0].clone();
        let (s_name, s_body) = load_script_reference(&script_ref);
        for i_rep in 1..=n_rep {
            if G.i_trace.load(Ordering::SeqCst) != 0 {
                log_message(format!("begin script [{s_name}] cycle {i_rep}"));
            }
            run_script(&conn, 0, 0, &s_body, &s_name);
            if G.i_trace.load(Ordering::SeqCst) != 0 {
                log_message(format!("end script [{s_name}] cycle {i_rep}"));
            }
        }
        wait_for_client(&conn, 0, 2000, "during shutdown...\n");
        try_sql(&conn, "UPDATE client SET wantHalt=1");
        sqlite_sleep(10);
        G.i_timeout.store(0, Ordering::SeqCst);
        let mut timeout = 1000;
        while {
            let rc = try_sql(&conn, "SELECT 1 FROM client");
            let busy = rc == SQLITE_BUSY || rc == SQLITE_ROW;
            busy && {
                sqlite_sleep(10);
                timeout -= 10;
                timeout > 0
            }
        } {}
        sqlite_sleep(100);
        unsafe {
            let db = dbh(&conn);
            let csql = CString::new("SELECT nError, nTest FROM counters").unwrap();
            let mut stmt: *mut ffi::sqlite3_stmt = std::ptr::null_mut();
            if ffi::sqlite3_prepare_v2(db, csql.as_ptr(), -1, &mut stmt, std::ptr::null_mut()) == ffi::SQLITE_OK {
                let mut timeout = 1000;
                let mut rc = ffi::sqlite3_step(stmt);
                while rc == SQLITE_BUSY && timeout > 0 {
                    sqlite_sleep(10);
                    timeout -= 10;
                    rc = ffi::sqlite3_step(stmt);
                }
                if rc == SQLITE_ROW {
                    let e = ffi::sqlite3_column_int(stmt, 0) as u64;
                    let t = ffi::sqlite3_column_int(stmt, 1) as u64;
                    G.n_error.fetch_add(e, Ordering::SeqCst);
                    G.n_test.fetch_add(t, Ordering::SeqCst);
                }
                ffi::sqlite3_finalize(stmt);
            }
        }
    }
    drop(conn);

    if i_client == 0 {
        let n_error = G.n_error.load(Ordering::SeqCst);
        let n_test = G.n_test.load(Ordering::SeqCst);
        println!("Summary: {n_error} errors out of {n_test} tests");
        print!(
            "END: {}",
            G.argv0.get().map(|p| p.display().to_string()).unwrap_or_default()
        );
        for a in args {
            print!(" {a}");
        }
        println!();
    }
    (G.n_error.load(Ordering::SeqCst) > 0) as i32
}

fn open_connection(db_file: &str, create: bool) -> Connection {
    let cpath = CString::new(db_file).unwrap_or_else(|_| CString::new(":memory:").unwrap());
    let mut flags = ffi::SQLITE_OPEN_READWRITE;
    if create {
        flags |= ffi::SQLITE_OPEN_CREATE;
    }
    let vfs = G.vfs.get().map(|v| CString::new(v.as_bytes()).unwrap());
    unsafe {
        let mut db: *mut ffi::sqlite3 = std::ptr::null_mut();
        let rc = ffi::sqlite3_open_v2(
            cpath.as_ptr(),
            &mut db,
            flags,
            vfs.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null()),
        );
        if rc != ffi::SQLITE_OK {
            let msg = if db.is_null() {
                "cannot open database".to_string()
            } else {
                last_errmsg(db)
            };
            fatal_error(None, format!("cannot open [{db_file}]: {msg}"));
        }
        Connection::from_handle(db).unwrap_or_else(|e| fatal_error(None, format!("cannot wrap [{db_file}]: {e}")))
    }
}

// resolve a script reference: embedded mptest scripts by (tail) name, else a
// filesystem path
fn load_script_reference(r: &str) -> (String, Vec<u8>) {
    let tail = filename_tail(r);
    if let Some((_, content)) = SCRIPTS.iter().find(|(n, _)| *n == tail) {
        return (tail.to_string(), content.as_bytes().to_vec());
    }
    let p = Path::new(r);
    if p.is_file() {
        return match fs::read(p) {
            Ok(b) => (r.to_string(), b),
            Err(e) => fatal_error(None, format!("cannot open \"{r}\" for reading: {e}")),
        };
    }
    fatal_error(
        None,
        format!(
            "cannot open \"{r}\" for reading (not one of the embedded scripts: {})",
            embedded_names().join(" ")
        ),
    )
}

fn embedded_names() -> Vec<&'static str> {
    SCRIPTS.iter().map(|(n, _)| *n).collect()
}

// ---------- fstest adapter ----------

#[derive(clap::Args)]
pub struct Args {
    /// scripts to run (embedded mptest scripts, comma list, extension optional)
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "config01,config02,crash01,multiwrite01"
    )]
    pub script: Vec<String>,

    /// only run scripts whose name contains this substring (repeatable)
    #[arg(long = "filter")]
    pub filter: Vec<String>,

    /// list the embedded scripts and exit
    #[arg(long)]
    pub list: bool,

    /// scripts in parallel (each gets its own database and work directory)
    #[arg(long, default_value_t = 4)]
    pub jobs: usize,

    /// per-script timeout in seconds (covers the whole multi-process run)
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,

    /// mptest --repeat: run each script N times against one database
    #[arg(long)]
    pub repeat: Option<u32>,

    /// mptest --journalmode: DELETE/TRUNCATE/PERSIST/MEMORY/WAL/OFF
    #[arg(long)]
    pub journalmode: Option<String>,

    /// mptest --vfs: VFS under test (unix, unix-dotfile, unix-excl, unix-namedsem)
    #[arg(long)]
    pub vfs: Option<String>,

    /// keep synchronous=FULL (mptest --sync; default is synchronous=OFF like upstream)
    #[arg(long)]
    pub sync: bool,

    /// mptest --sqltrace
    #[arg(long)]
    pub sqltrace: bool,

    /// mptest --quiet: suppress progress logs
    #[arg(long)]
    pub quiet: bool,

    /// mptest --trace LEVEL (default 1)
    #[arg(long)]
    pub trace: Option<i32>,

    /// working directory name under the mount point
    #[arg(long, default_value = ".fstest-sqlite")]
    pub work_dir: String,

    /// keep the working directory when done
    #[arg(long)]
    pub keep: bool,

    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

pub fn list(args: &Args) {
    for (name, content) in SCRIPTS {
        if !args.filter.is_empty() && !args.filter.iter().any(|f| name.contains(f)) {
            continue;
        }
        // description = the leading /* ... */ comment block, joined
        let mut words: Vec<&str> = Vec::new();
        for l in content.lines() {
            let l = l.trim();
            if l.starts_with("/*") || l.starts_with("**") {
                words.extend(
                    l.trim_start_matches('/')
                        .trim_start_matches('*')
                        .trim()
                        .split_whitespace(),
                );
            } else if !words.is_empty() {
                break;
            }
        }
        let joined = words.join(" ");
        let desc = if joined.len() <= 72 {
            joined
        } else {
            let cut = joined[..73].rfind(' ').unwrap_or(72);
            format!("{} ...", &joined[..cut])
        };
        println!("{name:20} {desc}");
    }
}

fn resolve_selection(args: &Args) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for s in &args.script {
        let cand = if s.ends_with(".test") || s.ends_with(".subtest") {
            s.clone()
        } else {
            format!("{s}.test")
        };
        if SCRIPTS.iter().any(|(n, _)| *n == cand) {
            if !out.contains(&cand) {
                out.push(cand);
            }
            continue;
        }
        // an explicit path to an external mptest script
        if s.contains('/') && Path::new(s).is_file() {
            if !out.contains(s) {
                out.push(s.clone());
            }
            continue;
        }
        return Err(format!(
            "{s}: not an embedded mptest script (config01, config02, crash01, multiwrite01) and not a file"
        ));
    }
    if !args.filter.is_empty() {
        out.retain(|s| args.filter.iter().any(|f| s.contains(f)));
    }
    if out.is_empty() {
        return Err("no scripts selected (check --script/--filter)".into());
    }
    Ok(out)
}

struct Exec {
    code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
    spawn_err: Option<String>,
}

// process-group spawn so a timeout takes the master and every --client child
// down together (children inherit the master's process group)
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
            "sqlite: cleanup of {} did not finish in {:?}; leaving it (the mount may be wedged)",
            path.display(),
            bound
        );
    }
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

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let selected = resolve_selection(args)?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot resolve fstest binary: {e}"))?;
    let sqlite_version = Connection::open_in_memory().ok().and_then(|c| {
        c.query_row("select sqlite_version()", [], |r| r.get::<_, String>(0))
            .ok()
    });

    let work_root = mountpoint.join(&args.work_dir);
    fs::create_dir_all(&work_root).map_err(|e| format!("{}: {e}", work_root.display()))?;

    info!(
        "sqlite: {} scripts ({}) on {} with {} jobs",
        selected.len(),
        selected.join(","),
        mountpoint.display(),
        args.jobs
    );
    let started = SystemTime::now();
    let items = run_parallel(selected.len(), args.jobs, |i| {
        let it0 = Instant::now();
        let script = &selected[i];
        let stem = filename_tail(script)
            .trim_end_matches(".test")
            .trim_end_matches(".subtest")
            .to_string();
        let sub = work_root.join(&stem);
        // a stale database from a previous run would start the run inside a
        // half-crashed state; fresh per run (the master unlinks the db too)
        let _ = fs::remove_dir_all(&sub);
        if let Err(e) = fs::create_dir_all(&sub) {
            return json!({ "name": stem, "status": "broken", "error": format!("create {}: {e}", sub.display()) });
        }
        let db = sub.join(format!("{stem}.db"));
        let mut cmd = Command::new(&exe);
        cmd.arg("__sqlite-mptest").arg(&db).arg(script);
        if let Some(jm) = &args.journalmode {
            cmd.args(["--journalmode", jm]);
        }
        if let Some(v) = &args.vfs {
            cmd.args(["--vfs", v]);
        }
        if let Some(r) = args.repeat {
            let r = r.to_string();
            cmd.args(["--repeat", &r]);
        }
        if let Some(t) = args.trace {
            let t = t.to_string();
            cmd.args(["--trace", &t]);
        }
        if args.sync {
            cmd.arg("--sync");
        }
        if args.sqltrace {
            cmd.arg("--sqltrace");
        }
        if args.quiet {
            cmd.arg("--quiet");
        }
        cmd.current_dir(&sub);
        let ex = exec_with_timeout(cmd, Duration::from_secs(args.timeout.max(1)));

        let combined = format!("{}\n{}", ex.stdout, ex.stderr);
        let mut errors: u64 = 0;
        let mut tests: u64 = 0;
        let mut summary_seen = false;
        let mut failures: Vec<(String, String)> = Vec::new();
        for line in combined.lines() {
            if let Some(rest) = line.strip_prefix("Summary: ") {
                let mut it = rest.split_whitespace();
                errors = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let _ = it.next(); // "errors"
                let _ = it.next(); // "out"
                let _ = it.next(); // "of"
                tests = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                summary_seen = true;
                continue;
            }
            if let Some(pos) = line.find(":ERROR: ") {
                if failures.len() < 20 {
                    failures.push((
                        line[..pos].split('.').last().unwrap_or("").to_string(),
                        line[pos + 8..].to_string(),
                    ));
                }
            } else if let Some(pos) = line.find(":FATAL: ") {
                if failures.len() < 20 {
                    failures.push((
                        line[..pos].split('.').last().unwrap_or("").to_string(),
                        format!("[fatal] {}", &line[pos + 8..]),
                    ));
                }
            }
        }
        let status = if ex.spawn_err.is_some() {
            "broken"
        } else if ex.timed_out {
            "timedout"
        } else if !summary_seen {
            // no Summary line: the master died before finishing (e.g. fatal)
            "broken"
        } else if errors > 0 {
            "fail"
        } else {
            "ok"
        };
        if status != "ok" {
            warn!("sqlite: {script} -> {status} ({errors} errors out of {tests} tests)");
        }
        info!(
            "sqlite: {} -> {} ({} errors out of {} tests, {}m{:02}s)",
            script,
            status,
            errors,
            tests,
            it0.elapsed().as_secs() / 60,
            it0.elapsed().as_secs() % 60
        );
        let mut item = json!({
            "name": stem,
            "script": script,
            "status": status,
            "errors": errors,
            "tests": tests,
            "durationMs": it0.elapsed().as_millis() as u64,
            "exitCode": ex.code,
        });
        let obj = item.as_object_mut().unwrap();
        if !failures.is_empty() {
            obj.insert(
                "failures".into(),
                json!(
                    failures
                        .iter()
                        .map(|(n, m)| json!({"client": n, "message": m}))
                        .collect::<Vec<_>>()
                ),
            );
        }
        if status != "ok" {
            obj.insert("tail".into(), json!(tail(&combined, 2000)));
        }
        if let Some(e) = ex.spawn_err {
            obj.insert("error".into(), json!(e));
        }
        item
    });

    let (mut ok_n, mut fail_n, mut broken_n, mut timedout_n) = (0u64, 0u64, 0u64, 0u64);
    let (mut t_errors, mut t_tests) = (0u64, 0u64);
    for item in items.iter() {
        match item["status"].as_str().unwrap_or("?") {
            "ok" => ok_n += 1,
            "fail" => fail_n += 1,
            "broken" => broken_n += 1,
            "timedout" => timedout_n += 1,
            _ => {}
        }
        t_errors += item["errors"].as_u64().unwrap_or(0);
        t_tests += item["tests"].as_u64().unwrap_or(0);
    }
    let run_ok = fail_n == 0 && broken_n == 0 && timedout_n == 0;
    info!(
        "sqlite: done in {}m{:02}s — {} scripts: {} ok {} failed {} broken {} timed out; {} errors out of {} tests",
        started.elapsed().map(|d| d.as_secs()).unwrap_or(0) / 60,
        started.elapsed().map(|d| d.as_secs()).unwrap_or(0) % 60,
        selected.len(),
        ok_n,
        fail_n,
        broken_n,
        timedout_n,
        t_errors,
        t_tests
    );

    if !args.keep {
        remove_dir_all_bounded(&work_root, Duration::from_secs(60));
    }

    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    let report = json!({
        "suite": "sqlite",
        "host": host,
        "date": iso8601(started),
        "top": mountpoint.display().to_string(),
        "params": {
            "sqlite_version": sqlite_version,
            "scripts": selected,
            "jobs": args.jobs,
            "timeout_sec": args.timeout,
            "repeat": args.repeat,
            "journalmode": args.journalmode,
            "vfs": args.vfs,
            "sync": args.sync,
        },
        "status": if run_ok { "ok" } else { "failed" },
        "results": {
            "summary": {
                "scripts": selected.len(),
                "ok": ok_n, "failed": fail_n, "broken": broken_n, "timedout": timedout_n,
                "errors": t_errors, "tests": t_tests,
            },
            "items": items,
        },
    });
    if let Some(path) = &args.json {
        if let Err(e) = fs::write(path, serde_json::to_vec_pretty(&report).unwrap()) {
            warn!("failed to write {}: {e}", path.display());
        }
    }
    Ok(report)
}
