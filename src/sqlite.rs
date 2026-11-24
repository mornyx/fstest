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

use crate::common::iso8601;
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
            // database — a simulated crash leaving a hot journal / uncommitted WAL tail
            "exit" => {
                let rc = atoi(&args[0]);
                if rc == 0 {
                    finish_script(conn, i_client, task_id, true);
                }
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
            for suffix in ["-wal", "-shm", "-journal"] {
                let _ = fs::remove_file(format!("{db_file}{suffix}"));
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
    /// probe families to run: mptest, crud, speedtest, kvtest, walthread, wal-lifecycle, mmap
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "mptest,crud,speedtest,kvtest,walthread,wal-lifecycle,mmap"
    )]
    pub probes: Vec<String>,

    /// scripts to run in the mptest probe (embedded scripts + *-wal multi-process WAL targets)
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "config01,config02,crash01,multiwrite01,multiwrite01-wal,crash01-wal"
    )]
    pub script: Vec<String>,

    /// only run scripts/probes whose name contains this substring (repeatable)
    #[arg(long = "filter")]
    pub filter: Vec<String>,

    /// list the embedded scripts and built-in probes and exit
    #[arg(long)]
    pub list: bool,

    /// scripts in parallel (each gets its own database and work directory)
    #[arg(long, default_value_t = 6)]
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

    /// enable synchronous disk writes (enabled by default; accepted for compatibility)
    #[arg(long)]
    pub sync: bool,

    /// disable fsync (PRAGMA synchronous=OFF) for faster lock-only stress
    #[arg(long, conflicts_with = "sync")]
    pub no_sync: bool,

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

impl Args {
    pub fn sync_enabled(&self) -> bool {
        !self.no_sync
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MptestJob {
    name: String,
    script: String,
    journalmode: Option<String>,
}

pub fn list(args: &Args) {
    for (name, content) in SCRIPTS {
        if !args.filter.is_empty() && !args.filter.iter().any(|f| name.contains(f)) {
            continue;
        }
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
        println!("{name:28} [mptest       ] {desc}");
    }
    let extra = [
        (
            "multiwrite01-wal",
            "mptest",
            "5-process concurrent table/index/VACUUM stress over WAL + -shm locks",
        ),
        (
            "crash01-wal",
            "mptest",
            "5-process WAL mid-transaction _exit(1) crash & survivor WAL recovery",
        ),
        (
            "crud-delete",
            "crud",
            "Core SQL CRUD + FK cascade + external sort + rollback + ATTACH + blob + FTS5 (DELETE)",
        ),
        (
            "crud-wal",
            "crud",
            "Core SQL CRUD + FK cascade + external sort + rollback + ATTACH + blob + FTS5 (WAL)",
        ),
        (
            "speedtest-delete",
            "speedtest",
            "B-tree/index/trigger/savepoint/VACUUM + SHA-256 verify (DELETE journal)",
        ),
        (
            "speedtest-truncate",
            "speedtest",
            "B-tree/index/trigger/savepoint/VACUUM + SHA-256 verify (TRUNCATE journal)",
        ),
        (
            "speedtest-persist",
            "speedtest",
            "B-tree/index/trigger/savepoint/VACUUM + SHA-256 verify (PERSIST journal)",
        ),
        (
            "speedtest-wal-mmap",
            "speedtest",
            "B-tree/index/trigger/savepoint/VACUUM + 4MiB mmap (WAL journal)",
        ),
        (
            "speedtest-wal-exclusive",
            "speedtest",
            "B-tree/index/trigger/savepoint/VACUUM (WAL + locking_mode=EXCLUSIVE)",
        ),
        (
            "kvtest-wal",
            "kvtest",
            "4KiB-page overflow-blob KV updates + PRAGMA integrity_check (WAL)",
        ),
        (
            "kvtest-delete",
            "kvtest",
            "4KiB-page overflow-blob KV updates + PRAGMA integrity_check (DELETE)",
        ),
        (
            "walthread-concurrent",
            "walthread",
            "Concurrent multi-thread WAL readers + writers + PASSIVE/FULL/RESTART/TRUNCATE checkpointer",
        ),
        (
            "wal-checkpoint-truncate-cycle",
            "wal-lifecycle",
            "Repeated WAL RESTART/TRUNCATE 0-byte truncation + immediate frame-1 reuse",
        ),
        (
            "wal-journal-mode-transitions",
            "wal-lifecycle",
            "Live DELETE <-> WAL <-> TRUNCATE <-> PERSIST mode switches & sidecar cleanup",
        ),
        (
            "mmap-probe-wal",
            "mmap",
            "MAP_SHARED kernel mmap probe of SQLite main DB and -shm WAL index",
        ),
        (
            "mmap-shared-rw-multiprocess",
            "mmap",
            "Cross-process O_RDWR MAP_SHARED coherence + fcntl WAL byte-range lock probe",
        ),
    ];
    for (name, kind, desc) in extra {
        if !args.filter.is_empty() && !args.filter.iter().any(|f| name.contains(f)) {
            continue;
        }
        println!("{name:28} [{kind:13}] {desc}");
    }
}

fn resolve_selection(args: &Args) -> Result<Vec<MptestJob>, String> {
    if !args.probes.iter().any(|p| p == "mptest" || p == "all") {
        return Ok(Vec::new());
    }
    let has_base = |stem: &str| {
        args.script.iter().any(|raw| {
            let trimmed = raw.trim_end_matches(".test");
            trimmed == stem
        })
    };
    let mut out: Vec<MptestJob> = Vec::new();
    for s in &args.script {
        let trimmed = s.trim_end_matches(".test");
        if let Some(base_stem) = trimmed.strip_suffix("-wal")
            && matches!(base_stem, "multiwrite01" | "crash01")
        {
            // When the caller explicitly forces `--journalmode <jm>` alongside the base script,
            // avoid running the same script twice under `<jm>`.
            if args.journalmode.is_some() && has_base(base_stem) {
                continue;
            }
            let job = MptestJob {
                name: format!("{base_stem}-wal"),
                script: format!("{base_stem}.test"),
                journalmode: Some(args.journalmode.clone().unwrap_or_else(|| "wal".to_string())),
            };
            if !out.iter().any(|j| j.name == job.name) {
                out.push(job);
            }
            continue;
        }
        let cand = if s.ends_with(".test") || s.ends_with(".subtest") {
            s.clone()
        } else {
            format!("{s}.test")
        };
        if SCRIPTS.iter().any(|(n, _)| *n == cand) {
            let stem = filename_tail(&cand)
                .trim_end_matches(".test")
                .trim_end_matches(".subtest")
                .to_string();
            if !out.iter().any(|j| j.name == stem) {
                out.push(MptestJob {
                    name: stem,
                    script: cand,
                    journalmode: args.journalmode.clone(),
                });
            }
            continue;
        }
        if s.contains('/') && Path::new(s).is_file() {
            let stem = filename_tail(s)
                .trim_end_matches(".test")
                .trim_end_matches(".subtest")
                .to_string();
            if !out.iter().any(|j| j.script == *s) {
                out.push(MptestJob {
                    name: stem,
                    script: s.clone(),
                    journalmode: args.journalmode.clone(),
                });
            }
            continue;
        }
        return Err(format!(
            "{s}: not an embedded mptest target (config01, config02, crash01, multiwrite01, multiwrite01-wal, crash01-wal) and not a file"
        ));
    }
    if !args.filter.is_empty() {
        out.retain(|j| args.filter.iter().any(|f| j.name.contains(f) || j.script.contains(f)));
    }
    Ok(out)
}

// ---------- Mount-scoped SQLite temp directory guard ----------

static SQLITE_TEMP_DIR_LOCK: Mutex<()> = Mutex::new(());

struct SqliteTempDirGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl SqliteTempDirGuard {
    fn activate(tmp_dir: &Path) -> Option<Self> {
        let guard = SQLITE_TEMP_DIR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let escaped = tmp_dir.display().to_string().replace('\'', "''");
        if let Ok(conn) = Connection::open_in_memory() {
            let _ = conn.execute_batch(&format!("PRAGMA temp_store_directory = '{escaped}';"));
        }
        Some(Self { _lock: guard })
    }
}

impl Drop for SqliteTempDirGuard {
    fn drop(&mut self) {
        if let Ok(conn) = Connection::open_in_memory() {
            let _ = conn.execute_batch("PRAGMA temp_store_directory = '';");
        }
    }
}

fn sql_quote_path(p: &Path) -> String {
    p.display().to_string().replace('\'', "''")
}

// ---------- Incremental BLOB helpers via bundled sqlite3_blob_* ----------

fn blob_rmw_write(
    conn: &Connection,
    table: &CStr,
    col: &CStr,
    rowid: i64,
    expected_len: usize,
    edits: &[(usize, &[u8])],
) -> Result<(), String> {
    unsafe {
        let db = dbh(conn);
        let mut p_blob: *mut ffi::sqlite3_blob = std::ptr::null_mut();
        let rc = ffi::sqlite3_blob_open(
            db,
            c"main".as_ptr(),
            table.as_ptr(),
            col.as_ptr(),
            rowid,
            1,
            &mut p_blob,
        );
        if rc != ffi::SQLITE_OK || p_blob.is_null() {
            return Err(format!("sqlite3_blob_open(rw) failed (rc={rc}): {}", last_errmsg(db)));
        }
        let n_bytes = ffi::sqlite3_blob_bytes(p_blob) as usize;
        if n_bytes != expected_len {
            ffi::sqlite3_blob_close(p_blob);
            return Err(format!("sqlite3_blob_bytes got {n_bytes}, want {expected_len}"));
        }
        for &(off, patch) in edits {
            let wrc = ffi::sqlite3_blob_write(
                p_blob,
                patch.as_ptr() as *const c_void,
                patch.len() as c_int,
                off as c_int,
            );
            if wrc != ffi::SQLITE_OK {
                let msg = last_errmsg(db);
                ffi::sqlite3_blob_close(p_blob);
                return Err(format!(
                    "sqlite3_blob_write(off={off}, len={}) failed (rc={wrc}): {msg}",
                    patch.len()
                ));
            }
        }
        let crc = ffi::sqlite3_blob_close(p_blob);
        if crc != ffi::SQLITE_OK {
            return Err(format!("sqlite3_blob_close(rw) failed (rc={crc}): {}", last_errmsg(db)));
        }
    }
    Ok(())
}

fn blob_incremental_read(
    conn: &Connection,
    table: &CStr,
    col: &CStr,
    rowid: i64,
    total_len: usize,
) -> Result<Vec<u8>, String> {
    unsafe {
        let db = dbh(conn);
        let mut p_blob: *mut ffi::sqlite3_blob = std::ptr::null_mut();
        let rc = ffi::sqlite3_blob_open(
            db,
            c"main".as_ptr(),
            table.as_ptr(),
            col.as_ptr(),
            rowid,
            0,
            &mut p_blob,
        );
        if rc != ffi::SQLITE_OK || p_blob.is_null() {
            return Err(format!("sqlite3_blob_open(ro) failed (rc={rc}): {}", last_errmsg(db)));
        }
        let mut out = vec![0u8; total_len];
        let chunk = 19_997usize; // unaligned multi-page chunk size
        let mut off = 0usize;
        while off < total_len {
            let take = (total_len - off).min(chunk);
            let rrc = ffi::sqlite3_blob_read(
                p_blob,
                out[off..off + take].as_mut_ptr() as *mut c_void,
                take as c_int,
                off as c_int,
            );
            if rrc != ffi::SQLITE_OK {
                let msg = last_errmsg(db);
                ffi::sqlite3_blob_close(p_blob);
                return Err(format!(
                    "sqlite3_blob_read(off={off}, len={take}) failed (rc={rrc}): {msg}"
                ));
            }
            off += take;
        }
        let crc = ffi::sqlite3_blob_close(p_blob);
        if crc != ffi::SQLITE_OK {
            return Err(format!("sqlite3_blob_close(ro) failed (rc={crc}): {}", last_errmsg(db)));
        }
        Ok(out)
    }
}

// ---------- Built-in SQLite engine probes (crud, speedtest, kvtest, walthread, wal-lifecycle, mmap) ----------

fn compute_crud_canonical_digest(db_path: &Path, aux_path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let conn = Connection::open(db_path).map_err(|e| format!("open {}: {e}", db_path.display()))?;
    let ic: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| format!("integrity_check {}: {e}", db_path.display()))?;
    if ic != "ok" {
        return Err(format!("{}: integrity_check={ic:?}", db_path.display()));
    }
    let fk_errs: i64 = conn
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))
        .map_err(|e| format!("foreign_key_check: {e}"))?;
    if fk_errs != 0 {
        return Err(format!(
            "{}: foreign_key_check found {fk_errs} violations",
            db_path.display()
        ));
    }

    let mut hasher = Sha256::new();
    {
        let mut st = conn
            .prepare(
                "SELECT id, email, tier, balance, json_extract(meta, '$.region'), schema_ver
                 FROM users ORDER BY id",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = st.query([]).map_err(|e| e.to_string())?;
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            let line = format!(
                "u:{}:{}:{}:{}:{}:{}\n",
                r.get::<_, i64>(0).map_err(|e| e.to_string())?,
                r.get::<_, String>(1).map_err(|e| e.to_string())?,
                r.get::<_, String>(2).map_err(|e| e.to_string())?,
                r.get::<_, i64>(3).map_err(|e| e.to_string())?,
                r.get::<_, String>(4).unwrap_or_default(),
                r.get::<_, i64>(5).map_err(|e| e.to_string())?,
            );
            hasher.update(line.as_bytes());
        }
    }
    {
        let mut st = conn
            .prepare("SELECT id, user_id, sku, qty, amount, note FROM orders ORDER BY id")
            .map_err(|e| e.to_string())?;
        let mut rows = st.query([]).map_err(|e| e.to_string())?;
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            let line = format!(
                "o:{}:{}:{}:{}:{}:{}\n",
                r.get::<_, i64>(0).map_err(|e| e.to_string())?,
                r.get::<_, i64>(1).map_err(|e| e.to_string())?,
                r.get::<_, String>(2).map_err(|e| e.to_string())?,
                r.get::<_, i64>(3).map_err(|e| e.to_string())?,
                r.get::<_, i64>(4).map_err(|e| e.to_string())?,
                r.get::<_, String>(5).map_err(|e| e.to_string())?,
            );
            hasher.update(line.as_bytes());
        }
    }
    {
        let mut st = conn
            .prepare("SELECT ns, k, v FROM kv_without_rowid ORDER BY ns, k")
            .map_err(|e| e.to_string())?;
        let mut rows = st.query([]).map_err(|e| e.to_string())?;
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            hasher.update(r.get::<_, String>(0).map_err(|e| e.to_string())?.as_bytes());
            hasher.update(b":");
            hasher.update(r.get::<_, String>(1).map_err(|e| e.to_string())?.as_bytes());
            hasher.update(&r.get::<_, Vec<u8>>(2).map_err(|e| e.to_string())?);
        }
    }
    {
        let blob: Vec<u8> = conn
            .query_row("SELECT payload FROM blobs WHERE id = 1", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        hasher.update(&blob);
    }
    {
        let mut st = conn
            .prepare("SELECT rowid, title, body FROM docs_fts ORDER BY rowid")
            .map_err(|e| e.to_string())?;
        let mut rows = st.query([]).map_err(|e| e.to_string())?;
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            let line = format!(
                "fts:{}:{}:{}\n",
                r.get::<_, i64>(0).map_err(|e| e.to_string())?,
                r.get::<_, String>(1).map_err(|e| e.to_string())?,
                r.get::<_, String>(2).map_err(|e| e.to_string())?,
            );
            hasher.update(line.as_bytes());
        }
    }

    let aux_conn = Connection::open(aux_path).map_err(|e| format!("open aux {}: {e}", aux_path.display()))?;
    let aux_ic: String = aux_conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if aux_ic != "ok" {
        return Err(format!("aux integrity_check={aux_ic:?}"));
    }
    {
        let mut st = aux_conn
            .prepare("SELECT id, user_id, amount, tag FROM archived_orders ORDER BY id")
            .map_err(|e| e.to_string())?;
        let mut rows = st.query([]).map_err(|e| e.to_string())?;
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            let line = format!(
                "aux:{}:{}:{}:{}\n",
                r.get::<_, i64>(0).map_err(|e| e.to_string())?,
                r.get::<_, i64>(1).map_err(|e| e.to_string())?,
                r.get::<_, i64>(2).map_err(|e| e.to_string())?,
                r.get::<_, String>(3).map_err(|e| e.to_string())?,
            );
            hasher.update(line.as_bytes());
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

fn run_crud_case(work_root: &Path, journal: &str, sync_full: bool) -> (Value, Option<String>) {
    let slug = journal.to_ascii_lowercase();
    let name = format!("crud-{slug}");
    let t0 = Instant::now();
    let dir = work_root.join(&name);
    let _ = fs::remove_dir_all(&dir);
    if let Err(e) = fs::create_dir_all(&dir) {
        let msg = e.to_string();
        return (
            json!({"name": name, "kind": "crud", "status": "broken", "reason": msg, "tail": msg, "errors": 1, "tests": 6}),
            None,
        );
    }
    let db_path = dir.join("crud.db");
    let aux_path = dir.join("crud_archive.db");
    let backup_path = dir.join("crud_backup.db");
    let sync_mode = if sync_full { "FULL" } else { "OFF" };

    let step = || -> Result<String, String> {
        // Pre-initialize aux DB with matching journal_mode so ATTACH exercises either
        // rollback master-journal (*-mj*) or dual-WAL multi-file transactions cleanly.
        {
            let aux_init = Connection::open(&aux_path).map_err(|e| format!("open aux: {e}"))?;
            let got_aux_jm: String = aux_init
                .query_row(&format!("PRAGMA journal_mode={slug}"), [], |r| r.get(0))
                .map_err(|e| format!("aux journal_mode={slug}: {e}"))?;
            if !got_aux_jm.eq_ignore_ascii_case(&slug) {
                return Err(format!("aux journal_mode got {got_aux_jm:?}, expected {slug:?}"));
            }
            aux_init
                .execute_batch(
                    "PRAGMA page_size=4096;
                     CREATE TABLE archived_orders(
                         id INTEGER PRIMARY KEY,
                         user_id INTEGER NOT NULL,
                         amount INTEGER NOT NULL,
                         tag TEXT NOT NULL
                     );",
                )
                .map_err(|e| format!("aux schema: {e}"))?;
        }

        let conn = Connection::open(&db_path).map_err(|e| format!("open main: {e}"))?;
        conn.busy_timeout(Duration::from_secs(30))
            .map_err(|e| format!("busy_timeout: {e}"))?;
        let got_jm: String = conn
            .query_row(&format!("PRAGMA journal_mode={slug}"), [], |r| r.get(0))
            .map_err(|e| format!("journal_mode={slug}: {e}"))?;
        if !got_jm.eq_ignore_ascii_case(&slug) {
            return Err(format!("journal_mode got {got_jm:?}, expected {slug:?}"));
        }
        conn.execute_batch(&format!(
            "PRAGMA synchronous={sync_mode};
             PRAGMA page_size=4096;
             PRAGMA foreign_keys=ON;
             PRAGMA temp_store=FILE;"
        ))
        .map_err(|e| format!("pragmas: {e}"))?;

        // 1. Relational CRUD + UNIQUE/CHECK/FK ON UPDATE & ON DELETE CASCADE + UPSERT + JSONB index
        conn.execute_batch(
            "CREATE TABLE users(
                 id INTEGER PRIMARY KEY,
                 email TEXT NOT NULL UNIQUE,
                 tier TEXT NOT NULL,
                 balance INTEGER NOT NULL,
                 meta BLOB NOT NULL
             );
             CREATE INDEX idx_users_tier_bal ON users(tier, balance DESC);
             CREATE INDEX idx_users_region ON users(json_extract(meta, '$.region'));
             CREATE TABLE orders(
                 id INTEGER PRIMARY KEY,
                 user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE ON UPDATE CASCADE,
                 sku TEXT NOT NULL,
                 qty INTEGER NOT NULL CHECK(qty > 0),
                 amount INTEGER NOT NULL,
                 note TEXT NOT NULL
             );
             CREATE INDEX idx_orders_user ON orders(user_id, amount);
             CREATE TABLE kv_without_rowid(
                 ns TEXT NOT NULL,
                 k TEXT NOT NULL,
                 v BLOB NOT NULL,
                 PRIMARY KEY(ns, k)
             ) WITHOUT ROWID;
             CREATE TABLE blobs(id INTEGER PRIMARY KEY, payload BLOB NOT NULL);
             CREATE VIRTUAL TABLE docs_fts USING fts5(title, body, tokenize='unicode61');",
        )
        .map_err(|e| format!("crud schema: {e}"))?;

        let regions = ["ap-southeast-1", "us-east-1", "eu-central-1", "ap-northeast-1"];
        let tiers = ["free", "pro", "enterprise"];
        conn.execute_batch("BEGIN IMMEDIATE;").map_err(|e| e.to_string())?;
        {
            let mut u_stmt = conn
                .prepare("INSERT INTO users(id, email, tier, balance, meta) VALUES(?1, ?2, ?3, ?4, jsonb(?5))")
                .map_err(|e| e.to_string())?;
            for id in 1..=300i64 {
                let email = format!("user-{id:04}@fstest.local");
                let tier = tiers[(id as usize) % tiers.len()];
                let bal = id * 100;
                let meta_json = format!(
                    r#"{{"region":"{}","bucket":{}}}"#,
                    regions[(id as usize) % regions.len()],
                    id % 16
                );
                u_stmt
                    .execute(rusqlite::params![id, email, tier, bal, meta_json])
                    .map_err(|e| format!("insert user {id}: {e}"))?;
            }
            let mut o_stmt = conn
                .prepare("INSERT INTO orders(id, user_id, sku, qty, amount, note) VALUES(?1, ?2, ?3, ?4, ?5, ?6)")
                .map_err(|e| e.to_string())?;
            for oid in 1..=900i64 {
                let uid = ((oid - 1) % 300) + 1;
                let sku = format!("SKU-{:02}", oid % 24);
                let qty = (oid % 5) + 1;
                let amount = qty * (60 + ((oid * 37) % 280));
                let note = format!("ord-{oid:04}-u{uid:03}");
                o_stmt
                    .execute(rusqlite::params![oid, uid, sku, qty, amount, note])
                    .map_err(|e| format!("insert order {oid}: {e}"))?;
            }
        }
        conn.execute_batch("COMMIT;").map_err(|e| e.to_string())?;

        // UPSERT across existing + new UNIQUE emails
        conn.execute_batch("BEGIN IMMEDIATE;").map_err(|e| e.to_string())?;
        for idx in 1..=50i64 {
            let uid = if idx <= 40 { idx * 3 } else { 300 + (idx - 40) };
            let email = format!("user-{uid:04}@fstest.local");
            let meta_json = format!(r#"{{"region":"ap-southeast-1","bucket":{}}}"#, uid % 16);
            conn.execute(
                "INSERT INTO users(id, email, tier, balance, meta)
                 VALUES(?1, ?2, 'pro', 750, jsonb(?3))
                 ON CONFLICT(email) DO UPDATE SET
                     balance = users.balance + excluded.balance,
                     tier = excluded.tier",
                rusqlite::params![uid, email, meta_json],
            )
            .map_err(|e| format!("upsert {uid}: {e}"))?;
        }
        conn.execute_batch("COMMIT;").map_err(|e| e.to_string())?;

        // Constraint enforcement (FK + CHECK must reject invalid writes without corrupting state)
        if conn
            .execute(
                "INSERT INTO orders(id, user_id, sku, qty, amount, note) VALUES(999901, 888888, 'BAD', 1, 10, 'fk')",
                [],
            )
            .is_ok()
        {
            return Err("FOREIGN KEY violation unexpectedly succeeded".into());
        }
        if conn
            .execute(
                "INSERT INTO orders(id, user_id, sku, qty, amount, note) VALUES(999902, 1, 'BAD', 0, 10, 'chk')",
                [],
            )
            .is_ok()
        {
            return Err("CHECK(qty > 0) violation unexpectedly succeeded".into());
        }

        // ON UPDATE CASCADE + ON DELETE CASCADE + JSONB expression index lookup
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             UPDATE users SET id = id + 10000 WHERE id <= 15;
             DELETE FROM users WHERE id < 10000 AND id % 29 = 0;
             COMMIT;",
        )
        .map_err(|e| format!("cascade tx: {e}"))?;
        let cascaded_orders: i64 = conn
            .query_row("SELECT count(*) FROM orders WHERE user_id > 10000", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if cascaded_orders != 45 {
            return Err(format!("ON UPDATE CASCADE expected 45 orders, got {cascaded_orders}"));
        }
        let orphan_orders: i64 = conn
            .query_row(
                "SELECT count(*) FROM orders o LEFT JOIN users u ON u.id = o.user_id WHERE u.id IS NULL",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if orphan_orders != 0 {
            return Err(format!("ON DELETE CASCADE left {orphan_orders} orphan orders"));
        }
        let ap_users: i64 = conn
            .query_row(
                "SELECT count(*) FROM users WHERE json_extract(meta, '$.region') = 'ap-southeast-1'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if ap_users < 70 {
            return Err(format!(
                "JSONB expression index query returned unexpectedly low count {ap_users}"
            ));
        }

        // 2. External temp-file merge-sort query (forces VDBE sorter to spill unlinked etilqs_* onto <mnt>)
        conn.execute_batch("PRAGMA cache_size = 16; PRAGMA temp_store = FILE;")
            .map_err(|e| e.to_string())?;
        let max_oid: i64 = conn
            .query_row("SELECT max(id) FROM orders", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let (top_key, sorted_cnt): (String, i64) = {
            let mut st = conn
                .prepare(
                    "SELECT printf('%06d', o.id) || '-' || hex(zeroblob(1500)) || '-' || o.note AS sort_rec, o.amount
                     FROM orders o
                     JOIN users u ON u.id = o.user_id
                     ORDER BY sort_rec DESC",
                )
                .map_err(|e| format!("prepare external sort: {e}"))?;
            let mut rows = st.query([]).map_err(|e| format!("exec external sort: {e}"))?;
            let mut first_prefix = String::new();
            let mut n = 0i64;
            while let Some(r) = rows.next().map_err(|e| format!("step external sort: {e}"))? {
                if n == 0 {
                    let s: String = r.get(0).map_err(|e| e.to_string())?;
                    first_prefix = s.chars().take(6).collect();
                }
                n += 1;
            }
            (first_prefix, n)
        };
        conn.execute_batch("PRAGMA cache_size = -2000;")
            .map_err(|e| e.to_string())?;
        let expected_prefix = format!("{max_oid:06}");
        if top_key != expected_prefix || sorted_cnt < 800 {
            return Err(format!(
                "external merge-sort mismatch: top_key={top_key:?} (want {expected_prefix:?}), rows={sorted_cnt}"
            ));
        }

        // 3. Cache-spilling explicit BEGIN...ROLLBACK & nested SAVEPOINT...ROLLBACK TO
        let pre_snap: (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM users),
                        (SELECT sum(balance) FROM users),
                        (SELECT count(*) FROM orders),
                        (SELECT sum(amount) FROM orders)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA cache_size = 10;
             BEGIN IMMEDIATE;
             UPDATE users SET balance = -999999, meta = jsonb_object('region', 'rollback', 'pad', hex(zeroblob(1800)));
             DELETE FROM orders WHERE id % 2 = 0;
             INSERT INTO kv_without_rowid(ns, k, v) VALUES('rb', 'spill1', zeroblob(65536));
             INSERT INTO kv_without_rowid(ns, k, v) VALUES('rb', 'spill2', zeroblob(65536));
             ROLLBACK;",
        )
        .map_err(|e| format!("cache-spill rollback: {e}"))?;
        let post_snap: (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM users),
                        (SELECT sum(balance) FROM users),
                        (SELECT count(*) FROM orders),
                        (SELECT sum(amount) FROM orders)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|e| e.to_string())?;
        if pre_snap != post_snap {
            return Err(format!(
                "ROLLBACK state mismatch: before={pre_snap:?}, after={post_snap:?}"
            ));
        }

        let target_uid: i64 = conn
            .query_row("SELECT min(id) FROM users", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        let (orig_bal, orig_u_orders): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT balance FROM users WHERE id = ?1),
                        (SELECT count(*) FROM orders WHERE user_id = ?1)",
                [target_uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             UPDATE users SET balance = balance + 333 WHERE id = {target_uid};
             SAVEPOINT sp_outer;
             DELETE FROM orders WHERE user_id = {target_uid};
             SAVEPOINT sp_inner;
             INSERT INTO kv_without_rowid(ns, k, v) VALUES('sp', 'spill_a', zeroblob(65536));
             INSERT INTO kv_without_rowid(ns, k, v) VALUES('sp', 'spill_b', zeroblob(65536));
             ROLLBACK TO sp_outer;
             RELEASE sp_outer;
             INSERT INTO kv_without_rowid(ns, k, v) VALUES('committed', 'k1', X'CAFEBABE01020304');
             COMMIT;
             PRAGMA cache_size = -2000;"
        ))
        .map_err(|e| format!("nested savepoint tx: {e}"))?;
        let (new_bal, new_u_orders, kv_cnt): (i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT balance FROM users WHERE id = ?1),
                        (SELECT count(*) FROM orders WHERE user_id = ?1),
                        (SELECT count(*) FROM kv_without_rowid)",
                [target_uid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| e.to_string())?;
        if new_bal != orig_bal + 333 || new_u_orders != orig_u_orders || kv_cnt != 1 {
            return Err(format!(
                "SAVEPOINT rollback mismatch: bal={new_bal} (want {}), orders={new_u_orders} (want {orig_u_orders}), kv_cnt={kv_cnt} (want 1)",
                orig_bal + 333
            ));
        }

        // 4. ATTACH DATABASE multi-file atomic CRUD commit & rollback (master-journal *-mj* on rollback modes)
        let aux_q = sql_quote_path(&aux_path);
        conn.execute_batch(&format!(
            "ATTACH DATABASE '{aux_q}' AS aux;
             PRAGMA aux.synchronous={sync_mode};"
        ))
        .map_err(|e| format!("attach aux: {e}"))?;
        let expected_arch: (i64, i64) = conn
            .query_row(
                "SELECT count(*), coalesce(sum(amount), 0) FROM main.orders WHERE amount >= 950",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        if expected_arch.0 == 0 {
            return Err("expected non-zero orders with amount >= 950 for ATTACH test".into());
        }
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             INSERT INTO aux.archived_orders(id, user_id, amount, tag)
                 SELECT id, user_id, amount, 'batch-1' FROM main.orders WHERE amount >= 950;
             DELETE FROM main.orders WHERE amount >= 950;
             COMMIT;",
        )
        .map_err(|e| format!("cross-db commit: {e}"))?;

        // Cross-DB atomic rollback after spilling dirty pages across BOTH databases
        let main_orders_after_arch: i64 = conn
            .query_row("SELECT count(*) FROM main.orders", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA main.cache_size = 10;
             PRAGMA aux.cache_size = 10;
             BEGIN IMMEDIATE;
             DELETE FROM main.orders;
             DELETE FROM aux.archived_orders;
             INSERT INTO main.kv_without_rowid(ns, k, v) VALUES('mj', 'spill', zeroblob(49152));
             INSERT INTO aux.archived_orders(id, user_id, amount, tag) VALUES(999999, 1, 1, hex(zeroblob(24576)));
             ROLLBACK;
             PRAGMA main.cache_size = -2000;
             PRAGMA aux.cache_size = -2000;
             DETACH DATABASE aux;",
        )
        .map_err(|e| format!("cross-db rollback: {e}"))?;
        let main_orders_restored: i64 = conn
            .query_row("SELECT count(*) FROM main.orders", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if main_orders_restored != main_orders_after_arch {
            return Err(format!(
                "cross-db ROLLBACK failed on main.orders: got {main_orders_restored}, want {main_orders_after_arch}"
            ));
        }
        // Ensure no stale master-journal (*-mj*) file leaked in the directory
        if let Ok(rd) = fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let fname = ent.file_name().to_string_lossy().into_owned();
                if fname.contains("-mj") {
                    return Err(format!("leftover master journal file not unlinked: {fname}"));
                }
            }
        }

        // 5. Incremental BLOB random RMW across page boundaries (sqlite3_blob_open/read/write)
        let blob_len = 65_536usize;
        let mut expected_blob = vec![0u8; blob_len];
        for (i, b) in expected_blob.iter_mut().enumerate() {
            *b = ((i * 31 + 17) & 0xff) as u8;
        }
        conn.execute("INSERT INTO blobs(id, payload) VALUES(1, ?1)", [&expected_blob])
            .map_err(|e| format!("insert initial blob: {e}"))?;
        let offsets = [
            0usize, 4088, 4096, 8184, 16372, 24570, 32760, 40952, 49144, 57336, 65420,
        ];
        let mut patches: Vec<(usize, Vec<u8>)> = Vec::new();
        for (p_idx, &off) in offsets.iter().enumerate() {
            let plen = 96usize;
            let mut p = vec![0u8; plen];
            for (k, b) in p.iter_mut().enumerate() {
                *b = ((0xa5usize + p_idx * 43 + k * 7) & 0xff) as u8;
            }
            expected_blob[off..off + plen].copy_from_slice(&p);
            patches.push((off, p));
        }
        let patch_refs: Vec<(usize, &[u8])> = patches.iter().map(|(o, v)| (*o, v.as_slice())).collect();
        blob_rmw_write(&conn, c"blobs", c"payload", 1, blob_len, &patch_refs)?;
        let got_inc = blob_incremental_read(&conn, c"blobs", c"payload", 1, blob_len)?;
        if got_inc != expected_blob {
            return Err("incremental sqlite3_blob_read mismatch after partial page-crossing writes".into());
        }

        // 6. FTS5 full-text CRUD + segment merge + ALTER TABLE + VACUUM INTO backup parity
        conn.execute_batch("BEGIN IMMEDIATE;").map_err(|e| e.to_string())?;
        for doc_id in 1..=120i64 {
            let title = format!("Doc {doc_id:03}");
            let body = if doc_id % 3 == 0 {
                format!("distributed storage transaction durability wal checkpoint item {doc_id}")
            } else {
                format!("posix metadata directory renameunlink consistency item {doc_id}")
            };
            conn.execute(
                "INSERT INTO docs_fts(rowid, title, body) VALUES(?1, ?2, ?3)",
                rusqlite::params![doc_id, title, body],
            )
            .map_err(|e| format!("fts5 insert {doc_id}: {e}"))?;
        }
        conn.execute_batch(
            "UPDATE docs_fts SET body = 'distributed storage transaction updated segment' WHERE rowid <= 15;
             DELETE FROM docs_fts WHERE rowid > 105;
             INSERT INTO docs_fts(docs_fts, rank) VALUES('merge', 16);
             INSERT INTO docs_fts(docs_fts) VALUES('optimize');
             INSERT INTO docs_fts(docs_fts) VALUES('integrity-check');
             COMMIT;",
        )
        .map_err(|e| format!("fts5 update/delete/merge: {e}"))?;
        let fts_hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM docs_fts WHERE docs_fts MATCH 'storage AND transaction'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| format!("fts5 match: {e}"))?;
        if fts_hits != 45 {
            return Err(format!("fts5 MATCH count mismatch: got {fts_hits}, want 45"));
        }

        conn.execute_batch(
            "ALTER TABLE users ADD COLUMN schema_ver INTEGER NOT NULL DEFAULT 2;
             UPDATE users SET schema_ver = 3 WHERE tier = 'enterprise';",
        )
        .map_err(|e| format!("alter table: {e}"))?;

        let backup_q = sql_quote_path(&backup_path);
        conn.execute_batch(&format!("VACUUM INTO '{backup_q}';"))
            .map_err(|e| format!("VACUUM INTO: {e}"))?;
        drop(conn);

        let main_digest = compute_crud_canonical_digest(&db_path, &aux_path)?;
        let backup_digest = compute_crud_canonical_digest(&backup_path, &aux_path)?;
        if main_digest != backup_digest {
            return Err(format!(
                "VACUUM INTO digest mismatch: main={main_digest}, backup={backup_digest}"
            ));
        }
        Ok(main_digest)
    };

    match step() {
        Ok(digest) => {
            info!(
                "sqlite: {name} -> ok (digest={}, {}ms)",
                &digest[..16],
                t0.elapsed().as_millis()
            );
            (
                json!({
                    "name": name,
                    "kind": "crud",
                    "journal": slug,
                    "status": "ok",
                    "errors": 0,
                    "tests": 6,
                    "verifyDigest": digest,
                    "durationMs": t0.elapsed().as_millis() as u64,
                }),
                Some(digest),
            )
        }
        Err(reason) => {
            warn!("sqlite: {name} -> fail ({reason})");
            (
                json!({
                    "name": name,
                    "kind": "crud",
                    "journal": slug,
                    "status": "fail",
                    "errors": 1,
                    "tests": 6,
                    "reason": reason,
                    "tail": reason,
                    "durationMs": t0.elapsed().as_millis() as u64,
                }),
                None,
            )
        }
    }
}

fn run_speedtest_case(
    work_root: &Path,
    journal: &str,
    mmap_size: u64,
    exclusive_lock: bool,
    sync_full: bool,
) -> (Value, Option<String>) {
    use sha2::{Digest, Sha256};
    let slug = journal.to_ascii_lowercase();
    let name = if exclusive_lock {
        format!("speedtest-{slug}-exclusive")
    } else if mmap_size > 0 {
        format!("speedtest-{slug}-mmap")
    } else {
        format!("speedtest-{slug}")
    };
    let t0 = Instant::now();
    let dir = work_root.join(&name);
    let _ = fs::remove_dir_all(&dir);
    if let Err(e) = fs::create_dir_all(&dir) {
        let msg = e.to_string();
        return (
            json!({"name": name, "kind": "speedtest", "status": "broken", "reason": msg, "tail": msg, "errors": 1, "tests": 1}),
            None,
        );
    }
    let db_path = dir.join("speedtest.db");
    let step = || -> Result<String, String> {
        let conn = Connection::open(&db_path).map_err(|e| format!("open: {e}"))?;
        conn.busy_timeout(Duration::from_secs(30))
            .map_err(|e| format!("busy_timeout: {e}"))?;
        if exclusive_lock {
            let lm: String = conn
                .query_row("PRAGMA locking_mode=EXCLUSIVE", [], |r| r.get(0))
                .map_err(|e| format!("locking_mode=EXCLUSIVE: {e}"))?;
            if !lm.eq_ignore_ascii_case("exclusive") {
                return Err(format!("locking_mode got {lm:?}, expected 'exclusive'"));
            }
        }
        let got_jm: String = conn
            .query_row(&format!("PRAGMA journal_mode={slug}"), [], |r| r.get(0))
            .map_err(|e| format!("journal_mode={slug}: {e}"))?;
        if !got_jm.eq_ignore_ascii_case(&slug) {
            return Err(format!("journal_mode got {got_jm:?}, expected {slug:?}"));
        }
        let sync_mode = if sync_full { "FULL" } else { "OFF" };
        conn.execute_batch(&format!("PRAGMA synchronous={sync_mode}; PRAGMA page_size=4096;"))
            .map_err(|e| format!("pragmas: {e}"))?;
        if mmap_size > 0 {
            let _ = conn.query_row(&format!("PRAGMA mmap_size={mmap_size}"), [], |_r| Ok(()));
        }

        // Schema: tables, secondary indexes, WITHOUT ROWID table, and audit trigger
        conn.execute_batch(
            "CREATE TABLE t1(a INTEGER PRIMARY KEY, b INTEGER, c TEXT);
             CREATE INDEX i1b ON t1(b);
             CREATE INDEX i1c ON t1(c);
             CREATE TABLE t2(k TEXT PRIMARY KEY, v BLOB) WITHOUT ROWID;
             CREATE TABLE audit_log(seq INTEGER PRIMARY KEY AUTOINCREMENT, op TEXT, key_id INTEGER);
             CREATE TRIGGER tr_t1_upd AFTER UPDATE ON t1 BEGIN
                 INSERT INTO audit_log(op, key_id) VALUES('upd', NEW.a);
             END;",
        )
        .map_err(|e| format!("schema: {e}"))?;

        // Batch 1: 500 deterministic rows inside transaction
        conn.execute_batch("BEGIN IMMEDIATE;")
            .map_err(|e| format!("begin1: {e}"))?;
        {
            let mut stmt = conn
                .prepare("INSERT INTO t1(a, b, c) VALUES(?1, ?2, ?3)")
                .map_err(|e| e.to_string())?;
            for i in 1..=500i64 {
                let b = (i * 2654435761) & 0xffff;
                let c = format!("row-{i:04}-{b:05}-payload");
                stmt.execute(rusqlite::params![i, b, c])
                    .map_err(|e| format!("insert {i}: {e}"))?;
            }
        }
        conn.execute_batch("COMMIT;").map_err(|e| format!("commit1: {e}"))?;

        // Batch 2: savepoint rollback + partial commit + range updates firing triggers + deletes
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             UPDATE t1 SET b = b + 17, c = c || '-u' WHERE a % 5 = 0;
             SAVEPOINT sp1;
             DELETE FROM t1 WHERE a <= 50;
             INSERT INTO t2(k, v) VALUES('transient', zeroblob(8192));
             ROLLBACK TO sp1;
             RELEASE sp1;
             DELETE FROM t1 WHERE a % 11 = 0;
             REPLACE INTO t1(a, b, c) VALUES(501, 42, 'replaced-tail');
             COMMIT;",
        )
        .map_err(|e| format!("tx2: {e}"))?;

        // Batch 3: WITHOUT ROWID multi-page blobs + VACUUM + reopen check
        conn.execute_batch("BEGIN IMMEDIATE;").map_err(|e| e.to_string())?;
        for k in 0..32u32 {
            let mut blob = vec![0u8; 6000];
            for (idx, byte) in blob.iter_mut().enumerate() {
                *byte = ((k as usize * 31 + idx * 7) & 0xff) as u8;
            }
            conn.execute(
                "INSERT INTO t2(k, v) VALUES(?1, ?2)",
                rusqlite::params![format!("blob-{k:03}"), blob],
            )
            .map_err(|e| format!("insert t2 {k}: {e}"))?;
        }
        conn.execute_batch("COMMIT; VACUUM;")
            .map_err(|e| format!("vacuum: {e}"))?;
        if exclusive_lock && slug == "wal" {
            let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_r| Ok(()));
        }
        drop(conn);

        // Reopen fresh connection to validate persisted image + compute canonical SHA-256
        let conn2 = Connection::open(&db_path).map_err(|e| format!("reopen: {e}"))?;
        let ic: String = conn2
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(|e| format!("integrity_check: {e}"))?;
        if ic != "ok" {
            return Err(format!("integrity_check returned {ic:?}"));
        }
        let mut hasher = Sha256::new();
        {
            let mut st = conn2
                .prepare("SELECT a, b, c FROM t1 ORDER BY a")
                .map_err(|e| e.to_string())?;
            let mut rows = st.query([]).map_err(|e| e.to_string())?;
            while let Some(r) = rows.next().map_err(|e| e.to_string())? {
                let a: i64 = r.get(0).map_err(|e| e.to_string())?;
                let b: i64 = r.get(1).map_err(|e| e.to_string())?;
                let c: String = r.get(2).map_err(|e| e.to_string())?;
                hasher.update(format!("{a}:{b}:{c}\n").as_bytes());
            }
        }
        {
            let mut st = conn2
                .prepare("SELECT k, v FROM t2 ORDER BY k")
                .map_err(|e| e.to_string())?;
            let mut rows = st.query([]).map_err(|e| e.to_string())?;
            while let Some(r) = rows.next().map_err(|e| e.to_string())? {
                let k: String = r.get(0).map_err(|e| e.to_string())?;
                let v: Vec<u8> = r.get(1).map_err(|e| e.to_string())?;
                hasher.update(k.as_bytes());
                hasher.update(&v);
            }
        }
        let trig_cnt: i64 = conn2
            .query_row("SELECT count(*) FROM audit_log", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        hasher.update(trig_cnt.to_le_bytes());
        Ok(hex::encode(hasher.finalize()))
    };

    match step() {
        Ok(digest) => {
            info!(
                "sqlite: {name} -> ok (digest={}, {}ms)",
                &digest[..16],
                t0.elapsed().as_millis()
            );
            (
                json!({
                    "name": name,
                    "kind": "speedtest",
                    "journal": slug,
                    "status": "ok",
                    "errors": 0,
                    "tests": 5,
                    "verifyDigest": digest,
                    "durationMs": t0.elapsed().as_millis() as u64,
                }),
                Some(digest),
            )
        }
        Err(reason) => {
            warn!("sqlite: {name} -> fail ({reason})");
            (
                json!({
                    "name": name,
                    "kind": "speedtest",
                    "journal": slug,
                    "status": "fail",
                    "errors": 1,
                    "tests": 5,
                    "reason": reason,
                    "tail": reason,
                    "durationMs": t0.elapsed().as_millis() as u64,
                }),
                None,
            )
        }
    }
}

fn run_kvtest_case(work_root: &Path, journal: &str, sync_full: bool) -> Value {
    let slug = journal.to_ascii_lowercase();
    let name = format!("kvtest-{slug}");
    let t0 = Instant::now();
    let dir = work_root.join(&name);
    let _ = fs::remove_dir_all(&dir);
    if let Err(e) = fs::create_dir_all(&dir) {
        return json!({"name": name, "kind": "kvtest", "status": "broken", "reason": e.to_string(), "errors": 1, "tests": 1});
    }
    let db_path = dir.join("kvtest.db");
    let step = || -> Result<(), String> {
        let mut model: std::collections::BTreeMap<i64, Vec<u8>> = std::collections::BTreeMap::new();
        // Phase 1: init 64 4KiB..12KiB blobs (overflow pages)
        {
            let conn = Connection::open(&db_path).map_err(|e| format!("open init: {e}"))?;
            conn.busy_timeout(Duration::from_secs(30)).map_err(|e| e.to_string())?;
            conn.execute_batch("PRAGMA page_size=4096; CREATE TABLE kv(k INTEGER PRIMARY KEY, v BLOB NOT NULL);")
                .map_err(|e| format!("create kv: {e}"))?;
            conn.execute_batch("BEGIN IMMEDIATE;").map_err(|e| e.to_string())?;
            for k in 0..64i64 {
                let sz = 4096 + ((k as usize * 397) % 8192);
                let mut blob = vec![0u8; sz];
                for (idx, b) in blob.iter_mut().enumerate() {
                    *b = ((k as usize * 17 + idx * 13) & 0xff) as u8;
                }
                conn.execute("INSERT INTO kv(k, v) VALUES(?1, ?2)", rusqlite::params![k, &blob])
                    .map_err(|e| format!("init k={k}: {e}"))?;
                model.insert(k, blob);
            }
            conn.execute_batch("COMMIT;").map_err(|e| e.to_string())?;
        }

        // Phase 2: reopen with requested journal mode + mmap_size, perform random updates, verify
        let conn = Connection::open(&db_path).map_err(|e| format!("reopen run: {e}"))?;
        conn.busy_timeout(Duration::from_secs(30)).map_err(|e| e.to_string())?;
        let got_jm: String = conn
            .query_row(&format!("PRAGMA journal_mode={slug}"), [], |r| r.get(0))
            .map_err(|e| format!("journal_mode={slug}: {e}"))?;
        if !got_jm.eq_ignore_ascii_case(&slug) {
            return Err(format!("journal_mode={got_jm:?}, expected {slug:?}"));
        }
        let sync_mode = if sync_full { "FULL" } else { "OFF" };
        conn.execute_batch(&format!("PRAGMA synchronous={sync_mode};"))
            .map_err(|e| e.to_string())?;
        let _ = conn.query_row("PRAGMA mmap_size=4194304", [], |_r| Ok(()));

        let mut state = 0x517cc1b727220a95u64;
        for round in 0..96u64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let k = (state % 64) as i64;
            let sz = 1024 + ((state as usize) % 10240);
            let mut blob = vec![0u8; sz];
            for (idx, b) in blob.iter_mut().enumerate() {
                *b = ((round as usize + idx * 29 + k as usize) & 0xff) as u8;
            }
            conn.execute("UPDATE kv SET v = ?2 WHERE k = ?1", rusqlite::params![k, &blob])
                .map_err(|e| format!("update round {round} k={k}: {e}"))?;
            model.insert(k, blob);
        }

        let ic: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(|e| format!("integrity_check: {e}"))?;
        if ic != "ok" {
            return Err(format!("integrity_check failed: {ic}"));
        }

        for (k, want) in &model {
            let got: Vec<u8> = conn
                .query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get(0))
                .map_err(|e| format!("select k={k}: {e}"))?;
            if &got != want {
                return Err(format!(
                    "blob mismatch at k={k} (got {} bytes, want {} bytes)",
                    got.len(),
                    want.len()
                ));
            }
        }
        Ok(())
    };

    match step() {
        Ok(()) => {
            info!("sqlite: {name} -> ok ({}ms)", t0.elapsed().as_millis());
            json!({
                "name": name,
                "kind": "kvtest",
                "journal": slug,
                "status": "ok",
                "errors": 0,
                "tests": 3,
                "durationMs": t0.elapsed().as_millis() as u64,
            })
        }
        Err(reason) => {
            warn!("sqlite: {name} -> fail ({reason})");
            json!({
                "name": name,
                "kind": "kvtest",
                "journal": slug,
                "status": "fail",
                "errors": 1,
                "tests": 3,
                "reason": reason,
                "tail": reason,
                "durationMs": t0.elapsed().as_millis() as u64,
            })
        }
    }
}

fn run_walthread_case(work_root: &Path, sync_full: bool) -> Value {
    let name = "walthread-concurrent";
    let t0 = Instant::now();
    let dir = work_root.join(name);
    let _ = fs::remove_dir_all(&dir);
    if let Err(e) = fs::create_dir_all(&dir) {
        let msg = e.to_string();
        return json!({"name": name, "kind": "walthread", "status": "broken", "reason": msg, "tail": msg, "errors": 1, "tests": 1});
    }
    let db_path = dir.join("walthread.db");
    let sync_mode = if sync_full { "NORMAL" } else { "OFF" };
    let step = || -> Result<u64, String> {
        {
            let conn = Connection::open(&db_path).map_err(|e| format!("open: {e}"))?;
            conn.busy_timeout(Duration::from_secs(30)).map_err(|e| e.to_string())?;
            let jm: String = conn
                .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
                .map_err(|e| format!("WAL pragma: {e}"))?;
            if !jm.eq_ignore_ascii_case("wal") {
                return Err(format!("journal_mode={jm:?}, expected 'wal'"));
            }
            conn.execute_batch(&format!(
                "PRAGMA synchronous={sync_mode};
                 CREATE TABLE accounts(id INTEGER PRIMARY KEY, bal INTEGER NOT NULL, ver INTEGER NOT NULL);
                 BEGIN IMMEDIATE;",
            ))
            .map_err(|e| e.to_string())?;
            // Zero-sum invariant across 32 accounts: total balance == 32_000 at every committed snapshot!
            for id in 0..32i64 {
                conn.execute("INSERT INTO accounts(id, bal, ver) VALUES(?1, 1000, 0)", [id])
                    .map_err(|e| e.to_string())?;
            }
            conn.execute_batch("COMMIT;").map_err(|e| e.to_string())?;
        }

        let err_slot: Mutex<Option<String>> = Mutex::new(None);
        let ops_done = AtomicU64::new(0);
        let done = AtomicBool::new(false);

        std::thread::scope(|s| {
            // 2 concurrent writer threads transferring balance atomically (preserving sum == 32_000)
            for wid in 0..2u64 {
                let db_path = &db_path;
                let err_slot = &err_slot;
                let ops_done = &ops_done;
                s.spawn(move || {
                    let res = (|| -> Result<(), String> {
                        let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
                        conn.busy_timeout(Duration::from_secs(30)).map_err(|e| e.to_string())?;
                        let _ = conn.execute_batch(&format!("PRAGMA synchronous={sync_mode};"));
                        for step_idx in 0..80u64 {
                            let a = ((wid * 13 + step_idx * 7) % 32) as i64;
                            let mut b = ((wid * 19 + step_idx * 11 + 1) % 32) as i64;
                            if a == b {
                                b = (a + 1) % 32;
                            }
                            let delta = ((step_idx % 25) + 1) as i64;
                            conn.execute_batch("BEGIN IMMEDIATE;").map_err(|e| e.to_string())?;
                            conn.execute(
                                "UPDATE accounts SET bal = bal - ?1, ver = ver + 1 WHERE id = ?2",
                                rusqlite::params![delta, a],
                            )
                            .map_err(|e| e.to_string())?;
                            conn.execute(
                                "UPDATE accounts SET bal = bal + ?1, ver = ver + 1 WHERE id = ?2",
                                rusqlite::params![delta, b],
                            )
                            .map_err(|e| e.to_string())?;
                            conn.execute_batch("COMMIT;").map_err(|e| e.to_string())?;
                            ops_done.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(())
                    })();
                    if let Err(e) = res {
                        let mut g = err_slot.lock().unwrap();
                        if g.is_none() {
                            *g = Some(format!("writer {wid}: {e}"));
                        }
                    }
                });
            }

            // 4 concurrent snapshot reader threads asserting sum(bal) == 32_000 at all times
            for rid in 0..4u64 {
                let db_path = &db_path;
                let err_slot = &err_slot;
                let ops_done = &ops_done;
                let done = &done;
                s.spawn(move || {
                    let res = (|| -> Result<(), String> {
                        let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
                        conn.busy_timeout(Duration::from_secs(30)).map_err(|e| e.to_string())?;
                        let mut reads = 0u64;
                        while !done.load(Ordering::Relaxed) || reads < 20 {
                            let sum: i64 = conn
                                .query_row("SELECT coalesce(sum(bal), 0) FROM accounts", [], |r| r.get(0))
                                .map_err(|e| e.to_string())?;
                            if sum != 32_000 {
                                return Err(format!("snapshot isolation violation: sum(bal)={sum} != 32000"));
                            }
                            reads += 1;
                            ops_done.fetch_add(1, Ordering::Relaxed);
                            if done.load(Ordering::Relaxed) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Ok(())
                    })();
                    if let Err(e) = res {
                        let mut g = err_slot.lock().unwrap();
                        if g.is_none() {
                            *g = Some(format!("reader {rid}: {e}"));
                        }
                    }
                });
            }

            // Checkpointer thread cycling PASSIVE / FULL / RESTART / TRUNCATE while readers/writers run
            {
                let db_path = &db_path;
                let err_slot = &err_slot;
                let done = &done;
                let ops_done = &ops_done;
                s.spawn(move || {
                    let ckpt_modes = ["PASSIVE", "FULL", "RESTART", "TRUNCATE"];
                    if let Ok(conn) = Connection::open(db_path) {
                        let _ = conn.busy_timeout(Duration::from_millis(250));
                        for idx in 0..12usize {
                            if ops_done.load(Ordering::Relaxed) >= 160 {
                                break;
                            }
                            let mode = ckpt_modes[idx % ckpt_modes.len()];
                            let _ = conn.query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |_r| Ok(()));
                            std::thread::sleep(Duration::from_millis(12));
                        }
                    } else {
                        let mut g = err_slot.lock().unwrap();
                        if g.is_none() {
                            *g = Some("checkpointer failed to open db".into());
                        }
                    }
                    // Wait until both writers finish (160 txns) or an error occurs, then release readers
                    let wait_t0 = Instant::now();
                    while wait_t0.elapsed() < Duration::from_secs(60) {
                        if err_slot.lock().unwrap().is_some() {
                            break;
                        }
                        // Writers together execute 160 txns; check accounts sum(ver) == 320
                        if let Ok(c) = Connection::open(db_path) {
                            let _ = c.busy_timeout(Duration::from_secs(5));
                            if let Ok(vsum) = c.query_row("SELECT sum(ver) FROM accounts", [], |r| r.get::<_, i64>(0))
                                && vsum >= 320
                            {
                                break;
                            }
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    done.store(true, Ordering::Relaxed);
                });
            }
        });

        if let Some(e) = err_slot.into_inner().unwrap() {
            return Err(e);
        }

        let conn = Connection::open(&db_path).map_err(|e| e.to_string())?;
        conn.busy_timeout(Duration::from_secs(30)).map_err(|e| e.to_string())?;
        let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_r| Ok(()));
        let ic: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if ic != "ok" {
            return Err(format!("post-walthread integrity_check: {ic}"));
        }
        let (sum_bal, sum_ver): (i64, i64) = conn
            .query_row("SELECT sum(bal), sum(ver) FROM accounts", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(|e| e.to_string())?;
        if sum_bal != 32_000 || sum_ver != 320 {
            return Err(format!(
                "final invariant mismatch: sum(bal)={sum_bal} (want 32000), sum(ver)={sum_ver} (want 320)"
            ));
        }
        Ok(ops_done.load(Ordering::Relaxed))
    };

    match step() {
        Ok(ops) => {
            info!(
                "sqlite: {name} -> ok ({ops} concurrent ops, {}ms)",
                t0.elapsed().as_millis()
            );
            json!({
                "name": name,
                "kind": "walthread",
                "status": "ok",
                "errors": 0,
                "tests": ops,
                "durationMs": t0.elapsed().as_millis() as u64,
            })
        }
        Err(reason) => {
            warn!("sqlite: {name} -> fail ({reason})");
            json!({
                "name": name,
                "kind": "walthread",
                "status": "fail",
                "errors": 1,
                "tests": 1,
                "reason": reason,
                "tail": reason,
                "durationMs": t0.elapsed().as_millis() as u64,
            })
        }
    }
}

// ---------- WAL lifecycle edge probes (TRUNCATE 0-byte reset & mode transitions) ----------

fn run_wal_checkpoint_truncate_case(work_root: &Path, sync_full: bool) -> Value {
    let name = "wal-checkpoint-truncate-cycle";
    let t0 = Instant::now();
    let dir = work_root.join(name);
    let _ = fs::remove_dir_all(&dir);
    if let Err(e) = fs::create_dir_all(&dir) {
        let msg = e.to_string();
        return json!({"name": name, "kind": "wal-lifecycle", "status": "broken", "reason": msg, "tail": msg, "errors": 1, "tests": 12});
    }
    let db_path = dir.join("wal_trunc.db");
    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    let sync_mode = if sync_full { "FULL" } else { "OFF" };

    let step = || -> Result<u64, String> {
        let conn = Connection::open(&db_path).map_err(|e| format!("open: {e}"))?;
        conn.busy_timeout(Duration::from_secs(30)).map_err(|e| e.to_string())?;
        let jm: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(|e| format!("WAL pragma: {e}"))?;
        if !jm.eq_ignore_ascii_case("wal") {
            return Err(format!("journal_mode={jm:?}, expected 'wal'"));
        }
        // Disable auto-checkpoint so we control RESTART/TRUNCATE boundaries deterministically
        conn.execute_batch(&format!(
            "PRAGMA synchronous={sync_mode};
             PRAGMA wal_autocheckpoint=0;
             PRAGMA page_size=4096;
             CREATE TABLE slots(id INTEGER PRIMARY KEY, round INTEGER NOT NULL, payload BLOB NOT NULL);"
        ))
        .map_err(|e| format!("init schema: {e}"))?;

        let mut expected_round = vec![0i64; 24];
        for round in 1..=12i64 {
            conn.execute_batch("BEGIN IMMEDIATE;").map_err(|e| e.to_string())?;
            for s in 0..8usize {
                let slot_id = ((round as usize * 5 + s) % 24) as i64;
                let mut payload = vec![0u8; 5000];
                for (i, b) in payload.iter_mut().enumerate() {
                    *b = ((round as usize * 37 + slot_id as usize * 13 + i) & 0xff) as u8;
                }
                conn.execute(
                    "INSERT INTO slots(id, round, payload) VALUES(?1, ?2, ?3)
                     ON CONFLICT(id) DO UPDATE SET round = excluded.round, payload = excluded.payload",
                    rusqlite::params![slot_id, round, payload],
                )
                .map_err(|e| format!("round {round} upsert {slot_id}: {e}"))?;
                expected_round[slot_id as usize] = round;
            }
            conn.execute_batch("COMMIT;").map_err(|e| e.to_string())?;

            let wal_len_before = fs::metadata(&wal_path)
                .map(|m| m.len())
                .map_err(|e| format!("round {round} stat -wal before checkpoint: {e}"))?;
            if wal_len_before == 0 {
                return Err(format!(
                    "round {round}: expected non-empty -wal before checkpoint, got 0 bytes"
                ));
            }

            if round % 2 == 1 {
                // RESTART resets WAL write cursor to frame 1 (offset 0) without truncating file size
                let (busy, log_frames, ckpt_frames): (i64, i64, i64) = conn
                    .query_row("PRAGMA wal_checkpoint(RESTART)", [], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .map_err(|e| format!("round {round} RESTART: {e}"))?;
                if busy != 0 || log_frames != ckpt_frames {
                    return Err(format!(
                        "round {round} RESTART incomplete: busy={busy}, log={log_frames}, ckpt={ckpt_frames}"
                    ));
                }
            } else {
                // TRUNCATE calls ftruncate(wal_fd, 0); next round immediately rewrites offset 0
                let (busy, _, _): (i64, i64, i64) = conn
                    .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .map_err(|e| format!("round {round} TRUNCATE: {e}"))?;
                if busy != 0 {
                    return Err(format!("round {round} TRUNCATE returned busy={busy}"));
                }
                let wal_len_after = fs::metadata(&wal_path)
                    .map(|m| m.len())
                    .map_err(|e| format!("round {round} stat -wal after TRUNCATE: {e}"))?;
                if wal_len_after != 0 {
                    return Err(format!(
                        "round {round}: -wal not truncated to 0 bytes after wal_checkpoint(TRUNCATE): size={wal_len_after}"
                    ));
                }
            }
        }
        drop(conn);

        // Reopen fresh connection to verify no stale page-cache / WAL salt mismatch occurred
        let conn2 = Connection::open(&db_path).map_err(|e| format!("reopen: {e}"))?;
        let ic: String = conn2
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if ic != "ok" {
            return Err(format!("integrity_check after TRUNCATE cycles: {ic}"));
        }
        for (slot_id, &want_r) in expected_round.iter().enumerate() {
            if want_r == 0 {
                continue;
            }
            let (got_r, got_blob): (i64, Vec<u8>) = conn2
                .query_row(
                    "SELECT round, payload FROM slots WHERE id = ?1",
                    [slot_id as i64],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|e| format!("verify slot {slot_id}: {e}"))?;
            if got_r != want_r || got_blob.len() != 5000 {
                return Err(format!("slot {slot_id} round mismatch: got {got_r}, want {want_r}"));
            }
            for (i, &b) in got_blob.iter().enumerate() {
                let want_b = ((want_r as usize * 37 + slot_id * 13 + i) & 0xff) as u8;
                if b != want_b {
                    return Err(format!(
                        "slot {slot_id} byte {i} corrupted: got {b:#x}, want {want_b:#x}"
                    ));
                }
            }
        }
        Ok(12)
    };

    match step() {
        Ok(tests) => {
            info!("sqlite: {name} -> ok ({}ms)", t0.elapsed().as_millis());
            json!({
                "name": name,
                "kind": "wal-lifecycle",
                "status": "ok",
                "errors": 0,
                "tests": tests,
                "durationMs": t0.elapsed().as_millis() as u64,
            })
        }
        Err(reason) => {
            warn!("sqlite: {name} -> fail ({reason})");
            json!({
                "name": name,
                "kind": "wal-lifecycle",
                "status": "fail",
                "errors": 1,
                "tests": 12,
                "reason": reason,
                "tail": reason,
                "durationMs": t0.elapsed().as_millis() as u64,
            })
        }
    }
}

fn run_wal_mode_transitions_case(work_root: &Path, sync_full: bool) -> Value {
    let name = "wal-journal-mode-transitions";
    let t0 = Instant::now();
    let dir = work_root.join(name);
    let _ = fs::remove_dir_all(&dir);
    if let Err(e) = fs::create_dir_all(&dir) {
        let msg = e.to_string();
        return json!({"name": name, "kind": "wal-lifecycle", "status": "broken", "reason": msg, "tail": msg, "errors": 1, "tests": 8});
    }
    let db_path = dir.join("mode_trans.db");
    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
    let sync_mode = if sync_full { "FULL" } else { "OFF" };
    let transitions = ["delete", "wal", "delete", "wal", "truncate", "wal", "persist", "delete"];

    let step = || -> Result<u64, String> {
        {
            let conn = Connection::open(&db_path).map_err(|e| format!("open init: {e}"))?;
            conn.execute_batch(&format!(
                "PRAGMA synchronous={sync_mode};
                 PRAGMA page_size=4096;
                 CREATE TABLE seq_log(step INTEGER PRIMARY KEY, mode TEXT NOT NULL, data BLOB NOT NULL);"
            ))
            .map_err(|e| format!("init: {e}"))?;
        }

        for (idx, &mode) in transitions.iter().enumerate() {
            let step_no = (idx + 1) as i64;
            let conn = Connection::open(&db_path).map_err(|e| format!("open step {step_no}: {e}"))?;
            conn.busy_timeout(Duration::from_secs(30)).map_err(|e| e.to_string())?;
            let got_jm: String = conn
                .query_row(&format!("PRAGMA journal_mode={mode}"), [], |r| r.get(0))
                .map_err(|e| format!("step {step_no} journal_mode={mode}: {e}"))?;
            if !got_jm.eq_ignore_ascii_case(mode) {
                return Err(format!(
                    "step {step_no}: got journal_mode {got_jm:?}, expected {mode:?}"
                ));
            }
            conn.execute_batch(&format!("PRAGMA synchronous={sync_mode};"))
                .map_err(|e| e.to_string())?;

            // When leaving WAL mode, SQLite checkpoints and unlinks -wal and -shm immediately
            if mode != "wal" && (wal_path.exists() || shm_path.exists()) {
                return Err(format!(
                    "step {step_no} ({mode}): stale WAL sidecars still present after leaving WAL (wal={}, shm={})",
                    wal_path.exists(),
                    shm_path.exists()
                ));
            }

            let mut blob = vec![0u8; 6144];
            for (i, b) in blob.iter_mut().enumerate() {
                *b = ((step_no as usize * 53 + i * 11) & 0xff) as u8;
            }
            conn.execute_batch("BEGIN IMMEDIATE;").map_err(|e| e.to_string())?;
            conn.execute(
                "INSERT INTO seq_log(step, mode, data) VALUES(?1, ?2, ?3)",
                rusqlite::params![step_no, mode, blob],
            )
            .map_err(|e| format!("step {step_no} insert: {e}"))?;
            if step_no > 1 {
                conn.execute("UPDATE seq_log SET mode = mode || '+ok' WHERE step = ?1", [step_no - 1])
                    .map_err(|e| format!("step {step_no} update prior: {e}"))?;
            }
            conn.execute_batch("COMMIT;").map_err(|e| e.to_string())?;

            if mode == "wal" && (!wal_path.exists() || !shm_path.exists()) {
                return Err(format!(
                    "step {step_no} (wal): expected active -wal and -shm while open (wal={}, shm={})",
                    wal_path.exists(),
                    shm_path.exists()
                ));
            }
            drop(conn);
        }

        let verify_conn = Connection::open(&db_path).map_err(|e| format!("verify open: {e}"))?;
        let ic: String = verify_conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if ic != "ok" {
            return Err(format!("post-transitions integrity_check: {ic}"));
        }
        let cnt: i64 = verify_conn
            .query_row("SELECT count(*) FROM seq_log", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if cnt != transitions.len() as i64 {
            return Err(format!("seq_log row count got {cnt}, want {}", transitions.len()));
        }
        Ok(transitions.len() as u64)
    };

    match step() {
        Ok(tests) => {
            info!("sqlite: {name} -> ok ({}ms)", t0.elapsed().as_millis());
            json!({
                "name": name,
                "kind": "wal-lifecycle",
                "status": "ok",
                "errors": 0,
                "tests": tests,
                "durationMs": t0.elapsed().as_millis() as u64,
            })
        }
        Err(reason) => {
            warn!("sqlite: {name} -> fail ({reason})");
            json!({
                "name": name,
                "kind": "wal-lifecycle",
                "status": "fail",
                "errors": 1,
                "tests": transitions.len(),
                "reason": reason,
                "tail": reason,
                "durationMs": t0.elapsed().as_millis() as u64,
            })
        }
    }
}

// ---------- Cross-process O_RDWR MAP_SHARED + fcntl WAL-index lock probe ----------

const SHM_PROBE_SIZE: usize = 32_768; // SQLite WALINDEX_PGSZ (32 KiB)
const SHM_PARENT_MAGIC: &[u8; 32] = b"FSTEST-SQLITE-SHM-PARENT-V1-0001";
const SHM_CHILD_MAGIC: &[u8; 32] = b"FSTEST-SQLITE-SHM-CHILD--V1-0002";

fn fcntl_setlk(fd: c_int, lock_type: impl Into<i32>, start: i64, len: i64) -> Result<(), std::io::Error> {
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = lock_type.into() as _;
    fl.l_whence = libc::SEEK_SET as _;
    fl.l_start = start;
    fl.l_len = len;
    let rc = unsafe { libc::fcntl(fd, libc::F_SETLK, &fl) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn shm_child_main(args: &[String]) -> i32 {
    use std::os::unix::ffi::OsStrExt;
    let Some(path_str) = args.first() else {
        eprintln!("__sqlite-shm-child: missing <shm_path>");
        return 2;
    };
    let path = Path::new(path_str);
    let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
        eprintln!("__sqlite-shm-child: invalid path");
        return 2;
    };
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        eprintln!(
            "__sqlite-shm-child: open O_RDWR failed: {}",
            std::io::Error::last_os_error()
        );
        return 1;
    }
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            SHM_PROBE_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if addr == libc::MAP_FAILED || addr.is_null() {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        eprintln!("__sqlite-shm-child: mmap(PROT_READ|PROT_WRITE, MAP_SHARED) failed: {err}");
        return 1;
    }

    let step = (|| -> Result<(), String> {
        let slice = unsafe { std::slice::from_raw_parts_mut(addr as *mut u8, SHM_PROBE_SIZE) };
        if &slice[0..32] != SHM_PARENT_MAGIC {
            return Err(format!(
                "child MAP_SHARED did not observe parent header magic (got {:?})",
                String::from_utf8_lossy(&slice[0..32])
            ));
        }
        // Parent holds F_WRLCK on byte 120 (SQLite WAL_WRITE_LOCK): child F_WRLCK must contend!
        match fcntl_setlk(fd, libc::F_WRLCK, 120, 1) {
            Ok(()) => {
                return Err(
                    "child fcntl(F_SETLK, F_WRLCK, off=120) unexpectedly succeeded while parent held exclusive lock"
                        .into(),
                );
            }
            Err(e) => {
                let raw = e.raw_os_error().unwrap_or(0);
                if raw != libc::EAGAIN && raw != libc::EACCES {
                    return Err(format!(
                        "child fcntl(F_WRLCK, off=120) failed with unexpected errno {raw} ({e}); expected EAGAIN/EACCES"
                    ));
                }
            }
        }
        // Parent holds shared F_RDLCK on byte 124 (WAL_READ_LOCK(1)): child shared F_RDLCK must succeed!
        fcntl_setlk(fd, libc::F_RDLCK, 124, 1)
            .map_err(|e| format!("child shared fcntl(F_RDLCK, off=124) failed: {e}"))?;
        // Unheld byte 125 (WAL_READ_LOCK(2)): child exclusive F_WRLCK must succeed!
        fcntl_setlk(fd, libc::F_WRLCK, 125, 1)
            .map_err(|e| format!("child exclusive fcntl(F_WRLCK, off=125) failed: {e}"))?;

        // Write child response into both page 0 (64..96) and page 1 (4096..4192) via MAP_SHARED
        slice[64..96].copy_from_slice(SHM_CHILD_MAGIC);
        for i in 0..96usize {
            slice[4096 + i] = ((0x5au8 as usize + i * 19) & 0xff) as u8;
        }
        let mrc = unsafe { libc::msync(addr, SHM_PROBE_SIZE, libc::MS_SYNC) };
        if mrc < 0 {
            return Err(format!(
                "child msync(MS_SYNC) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let _ = fcntl_setlk(fd, libc::F_UNLCK, 124, 2);
        Ok(())
    })();

    unsafe {
        libc::munmap(addr, SHM_PROBE_SIZE);
        libc::close(fd);
    }
    match step {
        Ok(()) => {
            println!("SHM_CHILD_OK");
            0
        }
        Err(msg) => {
            eprintln!("__sqlite-shm-child: {msg}");
            1
        }
    }
}

fn run_mmap_shared_rw_multiprocess_case(work_root: &Path, exe: &Path) -> Value {
    use std::os::unix::ffi::OsStrExt;
    let name = "mmap-shared-rw-multiprocess";
    let t0 = Instant::now();
    let dir = work_root.join(name);
    let _ = fs::remove_dir_all(&dir);
    if let Err(e) = fs::create_dir_all(&dir) {
        let msg = e.to_string();
        return json!({"name": name, "kind": "mmap", "status": "broken", "reason": msg, "tail": msg, "errors": 1, "tests": 4});
    }
    let shm_path = dir.join("walindex.db-shm");

    let step = || -> Result<(), String> {
        let cpath = CString::new(shm_path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        let fd = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC,
                0o644 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(format!(
                "open {}: {}",
                shm_path.display(),
                std::io::Error::last_os_error()
            ));
        }
        if unsafe { libc::ftruncate(fd, SHM_PROBE_SIZE as libc::off_t) } < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(format!("ftruncate {}: {err}", shm_path.display()));
        }
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SHM_PROBE_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if addr == libc::MAP_FAILED || addr.is_null() {
            let err = std::io::Error::last_os_error();
            let raw = err.raw_os_error().unwrap_or(0);
            unsafe { libc::close(fd) };
            if raw == libc::ENODEV {
                return Err(format!(
                    "{}: O_RDWR MAP_SHARED failed with ENODEV (FUSE mount likely uses FOPEN_DIRECT_IO without FUSE_DIRECT_IO_ALLOW_MMAP)",
                    shm_path.display()
                ));
            }
            return Err(format!(
                "mmap(PROT_READ|PROT_WRITE, MAP_SHARED) {}: {err}",
                shm_path.display()
            ));
        }

        let inner_res = (|| -> Result<(), String> {
            let slice = unsafe { std::slice::from_raw_parts_mut(addr as *mut u8, SHM_PROBE_SIZE) };
            slice[0..32].copy_from_slice(SHM_PARENT_MAGIC);
            std::sync::atomic::fence(Ordering::SeqCst);

            // Acquire SQLite's exact WAL-index byte-range locks (offsets 120 and 124)
            fcntl_setlk(fd, libc::F_WRLCK, 120, 1)
                .map_err(|e| format!("parent fcntl(F_WRLCK, off=120) failed: {e}"))?;
            fcntl_setlk(fd, libc::F_RDLCK, 124, 1)
                .map_err(|e| format!("parent fcntl(F_RDLCK, off=124) failed: {e}"))?;

            let mut cmd = Command::new(exe);
            cmd.arg("__sqlite-shm-child").arg(&shm_path);
            let ex = exec_with_timeout(cmd, Duration::from_secs(30));
            if let Some(se) = ex.spawn_err {
                return Err(format!("spawn __sqlite-shm-child: {se}"));
            }
            if ex.timed_out {
                return Err("__sqlite-shm-child timed out after 30s".into());
            }
            if ex.code != Some(0) || !ex.stdout.contains("SHM_CHILD_OK") {
                return Err(format!(
                    "__sqlite-shm-child failed (code={:?}): {}{}",
                    ex.code,
                    ex.stderr.trim(),
                    if ex.stdout.trim().is_empty() { "" } else { " " }
                ));
            }

            // 1. Verify parent's live MAP_SHARED mapping directly sees child's writes
            std::sync::atomic::fence(Ordering::SeqCst);
            let live_hdr = unsafe { std::slice::from_raw_parts(addr as *const u8, SHM_PROBE_SIZE) };
            if &live_hdr[64..96] != SHM_CHILD_MAGIC {
                return Err(format!(
                    "parent MAP_SHARED did not observe child write at 64..96 (got {:?})",
                    String::from_utf8_lossy(&live_hdr[64..96])
                ));
            }
            for i in 0..96usize {
                let want = ((0x5au8 as usize + i * 19) & 0xff) as u8;
                if live_hdr[4096 + i] != want {
                    return Err(format!(
                        "parent MAP_SHARED mismatch at byte {}: got {:#x}, want {want:#x}",
                        4096 + i,
                        live_hdr[4096 + i]
                    ));
                }
            }

            // 2. Verify pread() on a fresh descriptor sees coherent MAP_SHARED pages
            let rfd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY) };
            if rfd < 0 {
                return Err(format!(
                    "reopen O_RDONLY {}: {}",
                    shm_path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            let mut pbuf0 = [0u8; 96];
            let mut pbuf1 = [0u8; 96];
            let n0 = unsafe { libc::pread(rfd, pbuf0.as_mut_ptr() as *mut c_void, pbuf0.len(), 0) };
            let n1 = unsafe { libc::pread(rfd, pbuf1.as_mut_ptr() as *mut c_void, pbuf1.len(), 4096) };
            unsafe { libc::close(rfd) };
            if n0 != 96 || n1 != 96 {
                return Err(format!("pread returned short counts n0={n0}, n1={n1}"));
            }
            if &pbuf0[0..32] != SHM_PARENT_MAGIC || &pbuf0[64..96] != SHM_CHILD_MAGIC {
                return Err("pread() page 0 incoherent with MAP_SHARED writes".into());
            }
            for (i, &b) in pbuf1.iter().enumerate() {
                let want = ((0x5au8 as usize + i * 19) & 0xff) as u8;
                if b != want {
                    return Err(format!("pread() page 1 byte {i} incoherent with MAP_SHARED write"));
                }
            }
            Ok(())
        })();

        let _ = fcntl_setlk(fd, libc::F_UNLCK, 120, 8);
        unsafe {
            libc::munmap(addr, SHM_PROBE_SIZE);
            libc::close(fd);
        }
        inner_res
    };

    match step() {
        Ok(()) => {
            info!("sqlite: {name} -> ok ({}ms)", t0.elapsed().as_millis());
            json!({
                "name": name,
                "kind": "mmap",
                "status": "ok",
                "errors": 0,
                "tests": 4,
                "durationMs": t0.elapsed().as_millis() as u64,
            })
        }
        Err(reason) => {
            warn!("sqlite: {name} -> fail ({reason})");
            json!({
                "name": name,
                "kind": "mmap",
                "status": "fail",
                "errors": 1,
                "tests": 4,
                "reason": reason,
                "tail": reason,
                "durationMs": t0.elapsed().as_millis() as u64,
            })
        }
    }
}

fn mmap_shared_check(path: &Path, require_sqlite_magic: bool) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;
    let meta = fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    let size = meta.len() as usize;
    if size == 0 {
        return Err(format!("{}: file is empty", path.display()));
    }
    let cpath = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return Err(format!("open {}: {}", path.display(), std::io::Error::last_os_error()));
    }
    let addr = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
    unsafe { libc::close(fd) };
    if addr == libc::MAP_FAILED || addr.is_null() {
        let err = std::io::Error::last_os_error();
        let raw = err.raw_os_error().unwrap_or(0);
        if raw == libc::ENODEV {
            return Err(format!(
                "{}: MAP_SHARED failed with ENODEV (FUSE mount likely uses FOPEN_DIRECT_IO without FUSE_DIRECT_IO_ALLOW_MMAP)",
                path.display()
            ));
        }
        return Err(format!("mmap(MAP_SHARED) {}: {err}", path.display()));
    }
    let res = if require_sqlite_magic {
        if size < 16 {
            Err(format!("{}: size {size} < 16", path.display()))
        } else {
            let hdr = unsafe { std::slice::from_raw_parts(addr as *const u8, 15) };
            if hdr == b"SQLite format 3" {
                Ok(size as u64)
            } else {
                Err(format!(
                    "{}: MAP_SHARED header mismatch {:?}",
                    path.display(),
                    String::from_utf8_lossy(hdr)
                ))
            }
        }
    } else {
        Ok(size as u64)
    };
    unsafe {
        libc::munmap(addr, size);
    }
    res
}

fn run_mmap_probe_case(work_root: &Path) -> Value {
    let name = "mmap-probe-wal";
    let t0 = Instant::now();
    let dir = work_root.join(name);
    let _ = fs::remove_dir_all(&dir);
    if let Err(e) = fs::create_dir_all(&dir) {
        let msg = e.to_string();
        return json!({"name": name, "kind": "mmap", "status": "broken", "reason": msg, "tail": msg, "errors": 1, "tests": 1});
    }
    let db_path = dir.join("mmap-probe.db");
    let step = || -> Result<Vec<Value>, String> {
        let conn = Connection::open(&db_path).map_err(|e| format!("open: {e}"))?;
        let jm: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(|e| format!("WAL pragma: {e}"))?;
        if !jm.eq_ignore_ascii_case("wal") {
            return Err(format!("journal_mode={jm:?}, expected 'wal'"));
        }
        let _ = conn.query_row("PRAGMA mmap_size=4194304", [], |_r| Ok(()));
        conn.execute_batch("CREATE TABLE t(x BLOB); INSERT INTO t VALUES(zeroblob(65536));")
            .map_err(|e| format!("populate: {e}"))?;
        // Force WAL checkpoint so main db file has full header + pages, while keeping conn open so -shm stays live
        let _ = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_r| Ok(()));
        let mut probed = Vec::new();
        let main_sz = mmap_shared_check(&db_path, true)?;
        probed.push(json!({"path": db_path.display().to_string(), "size": main_sz, "ok": true}));
        let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
        if shm_path.is_file() && fs::metadata(&shm_path).map(|m| m.len() > 0).unwrap_or(false) {
            let shm_sz = mmap_shared_check(&shm_path, false)?;
            probed.push(json!({"path": shm_path.display().to_string(), "size": shm_sz, "ok": true}));
        }
        drop(conn);
        Ok(probed)
    };

    match step() {
        Ok(probed) => {
            info!("sqlite: {name} -> ok ({}ms)", t0.elapsed().as_millis());
            json!({
                "name": name,
                "kind": "mmap",
                "status": "ok",
                "errors": 0,
                "tests": probed.len(),
                "mmapProbes": probed,
                "durationMs": t0.elapsed().as_millis() as u64,
            })
        }
        Err(reason) => {
            warn!("sqlite: {name} -> fail ({reason})");
            json!({
                "name": name,
                "kind": "mmap",
                "status": "fail",
                "errors": 1,
                "tests": 1,
                "reason": reason,
                "tail": reason,
                "durationMs": t0.elapsed().as_millis() as u64,
            })
        }
    }
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
    for p in &args.probes {
        if !matches!(
            p.as_str(),
            "mptest" | "crud" | "speedtest" | "kvtest" | "walthread" | "wal-lifecycle" | "mmap" | "all"
        ) {
            return Err(format!(
                "unknown sqlite probe {p:?} (expected mptest, crud, speedtest, kvtest, walthread, wal-lifecycle, mmap, or all)"
            ));
        }
    }
    let want_probe = |name: &str| args.probes.iter().any(|p| p == name || p == "all");
    let matches_filter = |case_name: &str| args.filter.is_empty() || args.filter.iter().any(|f| case_name.contains(f));
    let sync_on = args.sync_enabled();

    let exe = std::env::current_exe().map_err(|e| format!("cannot resolve fstest binary: {e}"))?;
    let sqlite_version = Connection::open_in_memory().ok().and_then(|c| {
        c.query_row("select sqlite_version()", [], |r| r.get::<_, String>(0))
            .ok()
    });

    let work_root = mountpoint.join(&args.work_dir);
    fs::create_dir_all(&work_root).map_err(|e| format!("{}: {e}", work_root.display()))?;
    let tmp_dir = work_root.join("tmp");
    fs::create_dir_all(&tmp_dir).map_err(|e| format!("{}: {e}", tmp_dir.display()))?;

    info!(
        "sqlite: probes=[{}] mptest_scripts={} sync={} on {} with {} jobs",
        args.probes.join(","),
        selected.len(),
        sync_on,
        mountpoint.display(),
        args.jobs
    );
    let started = SystemTime::now();
    let selected_names: Vec<String> = selected.iter().map(|j| j.name.clone()).collect();

    let items = {
        let _temp_guard = SqliteTempDirGuard::activate(&tmp_dir);

        let mut items = if selected.is_empty() {
            Vec::new()
        } else {
            run_parallel(selected.len(), args.jobs, |i| {
                let it0 = Instant::now();
                let job = &selected[i];
                let stem = &job.name;
                let script = &job.script;
                let sub = work_root.join(stem);
                let _ = fs::remove_dir_all(&sub);
                if let Err(e) = fs::create_dir_all(&sub) {
                    let msg = format!("create {}: {e}", sub.display());
                    return json!({ "name": stem, "kind": "mptest", "status": "broken", "error": msg, "tail": msg });
                }
                let db = sub.join(format!("{stem}.db"));
                let mut cmd = Command::new(&exe);
                cmd.arg("__sqlite-mptest").arg(&db).arg(script);
                cmd.env("SQLITE_TMPDIR", &tmp_dir).env("TMPDIR", &tmp_dir);
                if let Some(jm) = &job.journalmode {
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
                if sync_on {
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
                        let _ = it.next();
                        let _ = it.next();
                        let _ = it.next();
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
                    } else if let Some(pos) = line.find(":FATAL: ")
                        && failures.len() < 20
                    {
                        failures.push((
                            line[..pos].split('.').last().unwrap_or("").to_string(),
                            format!("[fatal] {}", &line[pos + 8..]),
                        ));
                    }
                }
                let status = if ex.spawn_err.is_some() {
                    "broken"
                } else if ex.timed_out {
                    "timedout"
                } else if !summary_seen {
                    "broken"
                } else if errors > 0 {
                    "fail"
                } else {
                    "ok"
                };
                if status != "ok" {
                    warn!("sqlite: {stem} ({script}) -> {status} ({errors} errors out of {tests} tests)");
                }
                info!(
                    "sqlite: {} -> {} ({} errors out of {} tests, {}m{:02}s)",
                    stem,
                    status,
                    errors,
                    tests,
                    it0.elapsed().as_secs() / 60,
                    it0.elapsed().as_secs() % 60
                );
                let mut item = json!({
                    "name": stem,
                    "kind": "mptest",
                    "script": script,
                    "journal": job.journalmode.as_deref().unwrap_or("default"),
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
            })
        };

        if want_probe("crud") {
            let mut crud_ref: Option<(String, String)> = None;
            for (jm, cname) in [("delete", "crud-delete"), ("wal", "crud-wal")] {
                if !matches_filter(cname) {
                    continue;
                }
                let (mut item, digest_opt) = run_crud_case(&work_root, jm, sync_on);
                if let Some(d) = digest_opt {
                    if let Some((ref first_name, ref first_d)) = crud_ref {
                        if &d != first_d {
                            let msg = format!(
                                "cross-journal CRUD SHA-256 digest mismatch vs {first_name}: got {d}, want {first_d}"
                            );
                            item["status"] = json!("fail");
                            item["errors"] = json!(1);
                            item["reason"] = json!(msg);
                            item["tail"] = json!(msg);
                        }
                    } else {
                        crud_ref = Some((cname.to_string(), d));
                    }
                }
                items.push(item);
            }
        }

        if want_probe("speedtest") {
            let mut ref_digest: Option<(String, String)> = None;
            for (jm, mmap_sz, excl, cname) in [
                ("delete", 0u64, false, "speedtest-delete"),
                ("truncate", 0u64, false, "speedtest-truncate"),
                ("persist", 0u64, false, "speedtest-persist"),
                ("wal", 4_194_304u64, false, "speedtest-wal-mmap"),
                ("wal", 0u64, true, "speedtest-wal-exclusive"),
            ] {
                if !matches_filter(cname) {
                    continue;
                }
                let (mut item, digest_opt) = run_speedtest_case(&work_root, jm, mmap_sz, excl, sync_on);
                if let Some(d) = digest_opt {
                    if let Some((ref first_name, ref first_d)) = ref_digest {
                        if &d != first_d {
                            let msg = format!(
                                "cross-journal SHA-256 digest mismatch vs {first_name}: got {d}, want {first_d}"
                            );
                            item["status"] = json!("fail");
                            item["errors"] = json!(1);
                            item["reason"] = json!(msg);
                            item["tail"] = json!(msg);
                        }
                    } else {
                        ref_digest = Some((cname.to_string(), d));
                    }
                }
                items.push(item);
            }
        }

        if want_probe("kvtest") {
            for (jm, cname) in [("wal", "kvtest-wal"), ("delete", "kvtest-delete")] {
                if matches_filter(cname) {
                    items.push(run_kvtest_case(&work_root, jm, sync_on));
                }
            }
        }

        if want_probe("walthread") && matches_filter("walthread-concurrent") {
            items.push(run_walthread_case(&work_root, sync_on));
        }

        if want_probe("wal-lifecycle") {
            if matches_filter("wal-checkpoint-truncate-cycle") {
                items.push(run_wal_checkpoint_truncate_case(&work_root, sync_on));
            }
            if matches_filter("wal-journal-mode-transitions") {
                items.push(run_wal_mode_transitions_case(&work_root, sync_on));
            }
        }

        if want_probe("mmap") {
            if matches_filter("mmap-probe-wal") {
                items.push(run_mmap_probe_case(&work_root));
            }
            if matches_filter("mmap-shared-rw-multiprocess") {
                items.push(run_mmap_shared_rw_multiprocess_case(&work_root, &exe));
            }
        }

        items
    };

    if items.is_empty() {
        if !args.keep {
            remove_dir_all_bounded(&work_root, Duration::from_secs(60));
        }
        return Err("no sqlite probes/scripts matched selection (check --probes/--script/--filter)".into());
    }

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
        "sqlite: done in {}m{:02}s — {} items: {} ok {} failed {} broken {} timed out; {} errors out of {} tests",
        started.elapsed().map(|d| d.as_secs()).unwrap_or(0) / 60,
        started.elapsed().map(|d| d.as_secs()).unwrap_or(0) % 60,
        items.len(),
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
            "probes": args.probes,
            "scripts": selected_names,
            "jobs": args.jobs,
            "timeout_sec": args.timeout,
            "repeat": args.repeat,
            "journalmode": args.journalmode,
            "vfs": args.vfs,
            "sync": sync_on,
        },
        "status": if run_ok { "ok" } else { "failed" },
        "results": {
            "summary": {
                "scripts": items.len(),
                "ok": ok_n, "failed": fail_n, "broken": broken_n, "timedout": timedout_n,
                "errors": t_errors, "tests": t_tests,
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
    fn default_args_enable_sync_and_all_seven_probe_families() {
        let cli = TestCli::parse_from(["test"]);
        assert!(cli.args.sync_enabled());
        assert_eq!(
            cli.args.probes,
            vec![
                "mptest",
                "crud",
                "speedtest",
                "kvtest",
                "walthread",
                "wal-lifecycle",
                "mmap",
            ]
        );
        let jobs = resolve_selection(&cli.args).expect("resolve default mptest selection");
        let names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "config01",
                "config02",
                "crash01",
                "multiwrite01",
                "multiwrite01-wal",
                "crash01-wal",
            ]
        );
        assert_eq!(jobs[4].journalmode.as_deref(), Some("wal"));
        assert_eq!(jobs[5].journalmode.as_deref(), Some("wal"));
    }

    #[test]
    fn explicit_journalmode_deduplicates_wal_aliases() {
        let cli = TestCli::parse_from(["test", "--journalmode", "truncate", "--no-sync"]);
        assert!(!cli.args.sync_enabled());
        let jobs = resolve_selection(&cli.args).expect("resolve explicit journalmode");
        let names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, vec!["config01", "config02", "crash01", "multiwrite01"]);
        assert!(jobs.iter().all(|j| j.journalmode.as_deref() == Some("truncate")));
    }

    #[test]
    fn crud_and_wal_lifecycle_probes_pass_and_match_across_journals() {
        let tmp = std::env::temp_dir().join(format!("fstest-sqlite-ut-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let (item_del, d_del) = run_crud_case(&tmp, "delete", false);
        let (item_wal, d_wal) = run_crud_case(&tmp, "wal", false);
        assert_eq!(item_del["status"], "ok", "crud-delete failed: {item_del}");
        assert_eq!(item_wal["status"], "ok", "crud-wal failed: {item_wal}");
        assert_eq!(d_del, d_wal, "crud-delete and crud-wal canonical digests must match");

        let ckpt = run_wal_checkpoint_truncate_case(&tmp, false);
        assert_eq!(ckpt["status"], "ok", "wal-checkpoint-truncate-cycle failed: {ckpt}");

        let trans = run_wal_mode_transitions_case(&tmp, false);
        assert_eq!(trans["status"], "ok", "wal-journal-mode-transitions failed: {trans}");

        let _ = fs::remove_dir_all(&tmp);
    }
}
