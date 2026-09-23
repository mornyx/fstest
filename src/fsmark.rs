// Behavior-level port of fs_mark 3.3 (https://github.com/josefbacik/fs_mark).
// Per-system-call microsecond timing, sync-method matrix, subdirectory policies,
// file naming, aggregation and text report formats mirror the C implementation.

use crate::smallfile::fsync_raw;
use log::{info, warn};
use serde_json::{Map, Value, json};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const FS_MARK_VERSION: &str = "3.3";
const MAX_FILES: u32 = 1_000_000;
const MAX_THREADS: usize = 64;
const MAX_IO_BUFFER_SIZE: usize = 1024 * 1024;
const MAX_NAME_PATH: usize = 1000;
const MAX_FILENAME_SIZE: usize = 128;
const DEFAULT_SECS_PER_DIR: u64 = 180;

const FSYNC_BEFORE_CLOSE: u32 = 0x1;
const FSYNC_SYNC_SYSCALL: u32 = 0x2;
const FSYNC_FIRST_FILE: u32 = 0x4;
const FSYNC_POST_REVERSE: u32 = 0x8;
const FSYNC_POST_IN_ORDER: u32 = 0x10;

fn sync_method_bits(t: u32) -> u32 {
    match t {
        0 => 0,
        1 => FSYNC_BEFORE_CLOSE,
        2 => FSYNC_SYNC_SYSCALL | FSYNC_FIRST_FILE,
        3 => FSYNC_POST_REVERSE,
        4 => FSYNC_POST_REVERSE | FSYNC_SYNC_SYSCALL,
        5 => FSYNC_POST_IN_ORDER,
        6 => FSYNC_POST_IN_ORDER | FSYNC_SYNC_SYSCALL,
        _ => unreachable!("validated at CLI"),
    }
}

fn sync_method_name(t: u32) -> &'static str {
    [
        "NO SYNC: Test does not issue sync() or fsync() calls.",
        "INBAND FSYNC: fsync() per file in write loop.",
        "SYSTEM SYNC/SINGLE FSYNC: Issue sync() after main write loop and 1 file fsync() per subdirectory.",
        "POST REVERSE: Reopen and fsync() each file in reverse order after main write loop.",
        "SYNC POST REVERSE: Issue sync() and then reopen and fsync() each file in reverse order after main write loop.",
        "POST: Reopen and fsync() each file in order after main write loop.",
        "SYNC POST: Issue sync() and then reopen and fsync() each file in order after main write loop.",
    ][t as usize]
}

#[derive(Clone, Copy, PartialEq)]
enum DirPolicy {
    NoSubdirs,
    RoundRobin,
    TimeHash,
}

fn dir_policy_name(p: DirPolicy) -> &'static str {
    match p {
        DirPolicy::NoSubdirs => "No subdirectories",
        DirPolicy::RoundRobin => "Round Robin between directories",
        DirPolicy::TimeHash => "Time based hash between directories",
    }
}

#[derive(clap::Args)]
pub struct Args {
    /// additional test directory (repeatable; the mountpoint is always the first)
    #[arg(short = 'd', long)]
    pub dir: Vec<PathBuf>,

    /// number of files to create per iteration
    #[arg(short = 'n', long, default_value_t = 1000)]
    pub files: u32,

    /// size in bytes of each file
    #[arg(short = 's', long, default_value_t = 50 * 1024)]
    pub file_size: u64,

    /// bytes per write() call
    #[arg(short = 'w', long, default_value_t = 16 * 1024)]
    pub write_size: usize,

    /// number of iterations (implies keeping files)
    #[arg(short = 'L', long, default_value_t = 0)]
    pub iterations: u32,

    /// sync method 0..6 (see fs_mark -S)
    #[arg(short = 'S', long, default_value_t = 1, value_parser = sync_method_parser)]
    pub sync_method: u32,

    /// number of subdirectories (>= 2)
    #[arg(short = 'D', long)]
    pub subdirs: Option<usize>,

    /// files per subdirectory in round-robin mode (requires --subdirs)
    #[arg(short = 'N', long)]
    pub files_per_subdir: Option<usize>,

    /// total filename length in bytes
    #[arg(short = 'p', long, default_value_t = 40)]
    pub name_len: usize,

    /// random characters at the end of each filename
    #[arg(short = 'r', long, default_value_t = 24)]
    pub rand_len: usize,

    /// number of threads
    #[arg(short = 't', long, default_value_t = 1)]
    pub threads: usize,

    /// keep files after each iteration
    #[arg(short = 'k', long)]
    pub keep_files: bool,

    /// run until the filesystem is full (implies keeping files)
    #[arg(short = 'F', long)]
    pub fill_fs: bool,

    /// append the fs_mark-format text report to this log file (fs_mark itself
    /// always wrote fs_log.txt in the CWD; fstest only writes it on request)
    #[arg(short = 'l', long)]
    pub log_file: Option<PathBuf>,

    /// include per-system-call min/avg/max stats in the text report
    #[arg(long)]
    pub verbose_stats: bool,

    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

fn sync_method_parser(s: &str) -> Result<u32, String> {
    let v: u32 = s.parse().map_err(|_| "expected 0..=6".to_string())?;
    if v > 6 {
        Err("sync method must be 0..=6".into())
    } else {
        Ok(v)
    }
}

struct Params {
    dirs: Vec<PathBuf>,
    threads: usize,
    num_files: u32,
    file_size: u64,
    io_size: usize,
    loop_count: u32,
    sync_type: u32,
    sync_bits: u32,
    dir_policy: DirPolicy,
    num_subdirs: usize,
    num_per_subdir: usize,
    secs_per_dir: u64,
    name_len: usize,
    rand_len: usize,
    keep_files: bool,
    fill_fs: bool,
    log_file: Option<PathBuf>,
    verbose_stats: bool,
}

fn resolve(args: &Args, mountpoint: &Path) -> Result<Params, String> {
    let mut dirs = vec![mountpoint.to_path_buf()];
    dirs.extend(args.dir.iter().cloned());
    if dirs.len() > MAX_THREADS {
        return Err(format!("Max number of threads (and directories) is {MAX_THREADS}"));
    }
    for d in &dirs {
        if d.as_os_str().len() >= MAX_NAME_PATH {
            return Err(format!(
                "{} directory pathname too long (must be less than {MAX_NAME_PATH} bytes)",
                d.display()
            ));
        }
    }
    if args.files == 0 || args.files > MAX_FILES {
        return Err(format!("Max files is {MAX_FILES}"));
    }
    if args.write_size == 0 || args.write_size > MAX_IO_BUFFER_SIZE {
        return Err(format!("MAX IO buffer size is {MAX_IO_BUFFER_SIZE}"));
    }
    if args.name_len > MAX_FILENAME_SIZE {
        return Err(format!("Max filename size is {MAX_FILENAME_SIZE}"));
    }
    if args.rand_len >= args.name_len {
        return Err("random name length must be less than total name length".into());
    }
    let mut dir_policy = DirPolicy::NoSubdirs;
    let num_subdirs = match args.subdirs {
        Some(n) => {
            if n < 2 {
                return Err("Number of subdirs needs to be greater than 1".into());
            }
            dir_policy = DirPolicy::TimeHash;
            n
        }
        None => 0,
    };
    let num_per_subdir = match args.files_per_subdir {
        Some(n) => {
            if num_subdirs == 0 {
                return Err("Must specify more than 1 subdirectory with -D for files-per-subdir to make sense".into());
            }
            dir_policy = DirPolicy::RoundRobin;
            n
        }
        None => 0,
    };
    let mut threads = args.threads;
    if threads == 0 || threads > MAX_THREADS {
        return Err(format!("Max threads is {MAX_THREADS}"));
    }
    let num_dirs = dirs.len();
    if num_dirs > threads {
        threads = num_dirs;
    } else if threads % num_dirs != 0 {
        return Err(format!(
            "Threads ({threads}) must be an even multiple the number of directories ({num_dirs}) and less than {MAX_THREADS}"
        ));
    }
    let keep_files = args.keep_files || args.iterations > 0 || args.fill_fs;
    Ok(Params {
        dirs,
        threads,
        num_files: args.files,
        file_size: args.file_size,
        io_size: args.write_size,
        loop_count: args.iterations,
        sync_type: args.sync_method,
        sync_bits: sync_method_bits(args.sync_method),
        dir_policy,
        num_subdirs,
        num_per_subdir,
        secs_per_dir: DEFAULT_SECS_PER_DIR,
        name_len: args.name_len,
        rand_len: args.rand_len,
        keep_files,
        fill_fs: args.fill_fs,
        log_file: args.log_file.clone(),
        verbose_stats: args.verbose_stats,
    })
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn us(d: std::time::Duration) -> u64 {
    d.as_micros() as u64
}

// fs_mark reports min==0 as "unset" and carries that convention into aggregation
#[derive(Default, Clone, Copy)]
struct Stat {
    min: u64,
    max: u64,
    total: u64,
}

impl Stat {
    fn add(&mut self, v: u64) {
        self.total += v;
        if v > self.max {
            self.max = v;
        }
        if self.min == 0 || v < self.min {
            self.min = v;
        }
    }
    fn merge(&mut self, o: &Stat) {
        self.total += o.total;
        if o.max > self.max {
            self.max = o.max;
        }
        if self.min == 0 || (o.min > 0 && o.min < self.min) {
            self.min = o.min;
        }
    }
}

struct Row {
    file_count: u64,
    files_per_sec: f64,
    app_overhead_usec: i64,
    creat: Stat,
    write: Stat,
    write_avg_acc: u64, // fs_mark accumulates the per-file average of write() times, then divides by files
    fsync: Stat,
    sync_usec: u64,
    close: Stat,
    unlink: Stat,
}

impl Default for Row {
    fn default() -> Self {
        Row {
            file_count: 0,
            files_per_sec: 0.0,
            app_overhead_usec: 0,
            creat: Stat::default(),
            write: Stat::default(),
            write_avg_acc: 0,
            fsync: Stat::default(),
            sync_usec: 0,
            close: Stat::default(),
            unlink: Stat::default(),
        }
    }
}

enum RunErr {
    SpaceFull(PathBuf),
    Io(io::Error),
}

fn mkdir_eexist(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o777).create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

fn statfs_vals(dir: &Path) -> io::Result<(u64, u64, u64)> {
    let c = CString::new(dir.as_os_str().as_bytes()).map_err(|_| io::Error::other("path contains NUL"))?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((st.f_blocks as u64, st.f_bavail as u64, st.f_bsize as u64))
}

fn free_bytes(dir: &Path) -> io::Result<u64> {
    let (_, bavail, bsize) = statfs_vals(dir)?;
    Ok(bavail * bsize)
}

fn fs_use_pct(dir: &Path) -> io::Result<u32> {
    let (blocks, bavail, _) = statfs_vals(dir)?;
    Ok((100 * (blocks - bavail) / blocks) as u32)
}

// port of setup_file_name(): lowercase-hex timestamp for the sequential part
// (least significant digits kept, padded with '~' when short), then random
// uppercase-letter/digit characters; plus the subdirectory policy selection
fn setup_file_name(p: &Params, st: &mut ThreadState) -> io::Result<(PathBuf, String)> {
    let sec_time = unix_secs();
    if st.start_sec_time == 0 {
        st.start_sec_time = sec_time;
    }
    let subdir = match p.dir_policy {
        DirPolicy::NoSubdirs => String::new(),
        DirPolicy::RoundRobin => {
            if st.files_in_subdir >= p.num_per_subdir {
                st.current_subdir += 1;
                st.files_in_subdir = 0;
            }
            st.current_subdir %= p.num_subdirs;
            st.files_in_subdir += 1;
            format!("{:02x}", st.current_subdir)
        }
        DirPolicy::TimeHash => {
            if sec_time - st.start_sec_time > p.secs_per_dir {
                st.current_subdir = (st.current_subdir + 1) % p.num_subdirs;
                st.start_sec_time = sec_time;
            }
            format!("{:02x}", st.current_subdir)
        }
    };
    let target_dir = PathBuf::from(format!("{}/{}", st.dir.display(), subdir));
    mkdir_eexist(&target_dir)?;
    let seq_len = p.name_len - p.rand_len;
    let seq_name = format!("{sec_time:x}");
    let mut name = String::with_capacity(p.name_len);
    let skip = seq_name.len() as isize - seq_len as isize;
    if skip > 0 {
        name.push_str(&seq_name[skip as usize..]);
    } else {
        name.push_str(&seq_name);
        for _ in 0..(seq_len - seq_name.len()) {
            name.push('~');
        }
    }
    for _ in 0..p.rand_len {
        loop {
            let val = st.next_rand();
            let c = b'0' + (val & 0x7f) as u8;
            if c.is_ascii_uppercase() || c.is_ascii_digit() {
                name.push(c as char);
                break;
            }
        }
    }
    Ok((target_dir, name))
}

struct ThreadState {
    dir: PathBuf,
    rng: u64,
    current_subdir: usize,
    start_sec_time: u64,
    files_in_subdir: usize,
}

impl ThreadState {
    fn next_rand(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        z
    }
}

fn write_file(f: &mut File, buf: &[u8], sz: u64, write_stat: &mut Stat, write_avg_acc: &mut u64) -> io::Result<()> {
    let mut left = sz as usize;
    let mut calls = 0u64;
    let mut local = 0u64;
    while left > 0 {
        let n = buf.len().min(left);
        let t0 = Instant::now();
        f.write_all(&buf[..n])?;
        let delta = us(t0.elapsed());
        local += delta;
        write_stat.add(delta);
        calls += 1;
        left -= n;
    }
    *write_avg_acc += local / calls.max(1);
    Ok(())
}

fn run_thread(p: &Params, dir_index: usize, io_buffer: &[u8]) -> Result<Row, RunErr> {
    let dir = p.dirs[dir_index % p.dirs.len()].clone();
    // fs_mark's setup() (mkdir) runs before do_run()'s check_space (statfs), and
    // the latter needs the directory to exist
    mkdir_eexist(&dir).map_err(RunErr::Io)?;
    let bytes_per_loop = p.file_size * p.num_files as u64;
    if free_bytes(&dir).map_err(RunErr::Io)? < bytes_per_loop {
        return Err(RunErr::SpaceFull(dir));
    }
    // fs_mark re-forks its children every iteration, so all state below is
    // per-iteration and file counts are never cumulative inside a row
    let num_subdirs = p.num_subdirs.max(1);
    let mut st = ThreadState {
        dir,
        rng: unix_nanos() as u64 ^ (dir_index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        current_subdir: (unix_secs() % num_subdirs as u64) as usize,
        start_sec_time: 0,
        files_in_subdir: 0,
    };
    let mut row = Row::default();
    let mut names: Vec<PathBuf> = Vec::with_capacity(p.num_files as usize);
    let loop_start = Instant::now();
    for _ in 0..p.num_files {
        let (target_dir, fname) = setup_file_name(p, &mut st).map_err(RunErr::Io)?;
        let path = target_dir.join(&fname);
        names.push(path.clone());
        let t0 = Instant::now();
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o666)
            .open(&path)
            .map_err(RunErr::Io)?;
        row.creat.add(us(t0.elapsed()));
        write_file(&mut f, io_buffer, p.file_size, &mut row.write, &mut row.write_avg_acc).map_err(RunErr::Io)?;
        if p.sync_bits & FSYNC_BEFORE_CLOSE != 0 {
            let t0 = Instant::now();
            fsync_raw(&f).map_err(RunErr::Io)?;
            row.fsync.add(us(t0.elapsed()));
        }
        let t0 = Instant::now();
        drop(f);
        row.close.add(us(t0.elapsed()));
    }
    if p.sync_bits & FSYNC_SYNC_SYSCALL != 0 {
        let t0 = Instant::now();
        unsafe { libc::sync() };
        row.sync_usec = us(t0.elapsed());
    }
    let post_fsync = |row: &mut Row, path: &Path| -> Result<(), RunErr> {
        let t0 = Instant::now();
        let f = File::open(path).map_err(RunErr::Io)?;
        fsync_raw(&f).map_err(RunErr::Io)?;
        drop(f);
        row.fsync.add(us(t0.elapsed()));
        Ok(())
    };
    if p.sync_bits & FSYNC_POST_IN_ORDER != 0 {
        for path in &names {
            post_fsync(&mut row, path)?;
        }
    }
    if p.sync_bits & FSYNC_POST_REVERSE != 0 {
        for path in names.iter().rev() {
            post_fsync(&mut row, path)?;
        }
    }
    if p.sync_bits & FSYNC_FIRST_FILE != 0 {
        // fs_mark adds this single open/fsync/close to the fsync total without
        // feeding its min/max trackers
        let t0 = Instant::now();
        let f = File::open(&names[0]).map_err(RunErr::Io)?;
        fsync_raw(&f).map_err(RunErr::Io)?;
        drop(f);
        row.fsync.total += us(t0.elapsed());
    }
    let loop_usecs = us(loop_start.elapsed());
    if !p.keep_files {
        for path in &names {
            let t0 = Instant::now();
            fs::remove_file(path).map_err(RunErr::Io)?;
            row.unlink.add(us(t0.elapsed()));
        }
    }
    let total_ops = row.creat.total + row.write.total + row.fsync.total + row.sync_usec + row.close.total;
    row.app_overhead_usec = loop_usecs as i64 - total_ops as i64;
    row.file_count = p.num_files as u64;
    row.files_per_sec = p.num_files as f64 / (loop_usecs as f64 / 1e6);
    Ok(row)
}

#[derive(Default)]
struct Agg {
    file_count: u64,
    files_per_sec: f64,
    app_overhead_usec: i64,
    creat: Stat,
    write: Stat,
    write_avg: u64,
    fsync: Stat,
    sync_total: u64,
    close: Stat,
    unlink: Stat,
}

fn aggregate(rows: &[Row], threads: usize) -> Agg {
    let mut a = Agg::default();
    for r in rows {
        a.file_count += r.file_count;
        a.files_per_sec += r.files_per_sec;
        a.app_overhead_usec += r.app_overhead_usec;
        a.creat.merge(&r.creat);
        a.write.merge(&r.write);
        a.fsync.merge(&r.fsync);
        a.close.merge(&r.close);
        a.unlink.merge(&r.unlink);
        a.write_avg += r.write_avg_acc;
        a.sync_total += r.sync_usec;
    }
    if threads > 1 {
        // fs_mark recomputes the average of per-thread averages
        a.write_avg /= threads as u64;
    }
    a
}

fn ctime_now() -> String {
    // mirror ctime(3): local time, space-padded day-of-month
    let t = unix_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    let wdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{} {} {:2} {:02}:{:02}:{:02} {}",
        wdays[tm.tm_wday as usize % 7],
        months[tm.tm_mon as usize % 12],
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        tm.tm_year + 1900
    )
}

fn header_lines(p: &Params) -> Vec<String> {
    let mut v = vec![format!(
        "# fstest fsmark: version {FS_MARK_VERSION} (fs_mark port), {} thread(s) starting at {}",
        p.threads,
        ctime_now()
    )];
    v.push(format!("#\tSync method: {}", sync_method_name(p.sync_type)));
    if p.num_subdirs > 1 {
        let (n, what) = if p.dir_policy == DirPolicy::RoundRobin {
            (p.num_per_subdir, "files per subdirectory")
        } else {
            (p.secs_per_dir as usize, "seconds per subdirectory")
        };
        v.push(format!(
            "#\tDirectories:  {} across {} subdirectories with {} {}.",
            dir_policy_name(p.dir_policy),
            p.num_subdirs,
            n,
            what
        ));
    } else {
        v.push("#\tDirectories:  no subdirectories used".to_string());
    }
    v.push(format!(
        "#\tFile names: {} bytes long, ({} initial bytes of time stamp with {} random bytes at end of name)",
        p.name_len,
        p.name_len - p.rand_len,
        p.rand_len
    ));
    v.push(format!(
        "#\tFiles info: size {} bytes, written with an IO size of {} bytes per write",
        p.file_size, p.io_size
    ));
    v.push(
        "#\tApp overhead is time in microseconds spent in the test not doing file writing related system calls."
            .to_string(),
    );
    v.push(String::new());
    if p.verbose_stats {
        v.push("#\tAll system call times are reported in microseconds.".to_string());
        v.push(String::new());
        v.push(format!(
            "{:6} {:>12} {:>12} {:>12} {:>16} {:>26} {:>26} {:>26} {:>26} {:>26} {:>26}",
            "FSUse%",
            "Count",
            "Size",
            "Files/sec",
            "App Overhead",
            "CREAT (Min/Avg/Max)",
            "WRITE (Min/Avg/Max)",
            "FSYNC (Min/Avg/Max)",
            "SYNC (Min/Avg/Max)",
            "CLOSE (Min/Avg/Max)",
            "UNLINK (Min/Avg/Max)"
        ));
    } else {
        v.push(format!(
            "{:6} {:>12} {:>12} {:>12} {:>16}",
            "FSUse%", "Count", "Size", "Files/sec", "App Overhead"
        ));
    }
    v
}

fn iter_line(p: &Params, a: &Agg, files_written: u64, df_full: u32) -> String {
    let n = p.num_files as u64;
    if p.verbose_stats {
        format!(
            "{:6} {:>12} {:>12} {:>12.1} {:>16} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
            df_full,
            files_written,
            p.file_size,
            a.files_per_sec,
            a.app_overhead_usec,
            a.creat.min,
            a.creat.total / n,
            a.creat.max,
            a.write.min,
            a.write_avg / n,
            a.write.max,
            a.fsync.min,
            a.fsync.total / n,
            a.fsync.max,
            0, // fs_mark never fills min/max sync
            a.sync_total,
            0,
            a.close.min,
            a.close.total / n,
            a.close.max,
            a.unlink.min,
            a.unlink.total / n,
            a.unlink.max
        )
    } else {
        format!(
            "{:6} {:>12} {:>12} {:>12.1} {:>16}",
            df_full, files_written, p.file_size, a.files_per_sec, a.app_overhead_usec
        )
    }
}

fn row_json(r: &Row, p: &Params) -> Value {
    let n = p.num_files as u64;
    json!({
        "fileCount": r.file_count,
        "filesPerSec": r.files_per_sec,
        "appOverheadUsec": r.app_overhead_usec,
        "creatUsec": {"min": r.creat.min, "avg": r.creat.total / n, "max": r.creat.max},
        "writeUsec": {"min": r.write.min, "avg": r.write_avg_acc / n, "max": r.write.max},
        "fsyncUsec": {"min": r.fsync.min, "avg": r.fsync.total / n, "max": r.fsync.max},
        "syncUsec": r.sync_usec,
        "closeUsec": {"min": r.close.min, "avg": r.close.total / n, "max": r.close.max},
        "unlinkUsec": {"min": r.unlink.min, "avg": r.unlink.total / n, "max": r.unlink.max},
    })
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    // fs_mark mkdirs its test directories itself (parent must exist), so there is
    // no pre-existence check on the mount point here
    let p = resolve(args, mountpoint)?;
    let io_buffer = vec![0u8; p.io_size];
    let mut log_out: Option<File> =
        p.log_file
            .as_ref()
            .and_then(|path| match OpenOptions::new().append(true).create(true).open(path) {
                Ok(f) => Some(f),
                Err(e) => {
                    warn!("cannot open log file {}: {e}", path.display());
                    None
                }
            });
    info!(
        "fsmark: dirs={} threads={} files={} size={}B io={}B sync={} subdirs={} policy={} keep_files={} iterations_requested={}",
        p.dirs.len(),
        p.threads,
        p.num_files,
        p.file_size,
        p.io_size,
        p.sync_type,
        p.num_subdirs,
        dir_policy_name(p.dir_policy),
        p.keep_files,
        if p.loop_count > 0 {
            p.loop_count.to_string()
        } else {
            "1".to_string()
        }
    );
    for line in header_lines(&p) {
        info!("{line}");
        if let Some(f) = log_out.as_mut() {
            let _ = writeln!(f, "{line}");
        }
    }
    let suite_start = SystemTime::now();
    let mut files_written = 0u64;
    let mut rates: Vec<u64> = Vec::new();
    let mut rates_sum = 0u64;
    let mut iteration_rows: Vec<Value> = Vec::new();
    let mut status = "ok";
    let mut fs_full = false;
    let mut loops_done = 0u32;
    loop {
        let results: Vec<Result<Row, RunErr>> = thread::scope(|s| {
            let mut joins = Vec::with_capacity(p.threads);
            let pp = &p;
            for t in 0..p.threads {
                let buf = &io_buffer;
                joins.push(s.spawn(move || run_thread(pp, t, buf)));
            }
            joins.into_iter().map(|j| j.join().unwrap()).collect()
        });
        let mut rows = Vec::with_capacity(p.threads);
        for r in results {
            match r {
                Ok(row) => rows.push(row),
                Err(RunErr::SpaceFull(d)) => {
                    warn!(
                        "Insufficient free space in {} to create {} new files, exiting",
                        d.display(),
                        p.num_files
                    );
                    fs_full = true;
                }
                Err(RunErr::Io(e)) => {
                    warn!("iteration {} failed: {e}", loops_done + 1);
                    status = "failed";
                }
            }
        }
        if rows.is_empty() {
            break;
        }
        let a = aggregate(&rows, p.threads);
        files_written += a.file_count;
        let df_full = fs_use_pct(&p.dirs[0]).unwrap_or(0);
        let line = iter_line(&p, &a, files_written, df_full);
        info!("{line}");
        if let Some(f) = log_out.as_mut() {
            let _ = writeln!(f, "{line}");
        }
        let mut tj = Map::new();
        for (i, r) in rows.iter().enumerate() {
            tj.insert(i.to_string(), row_json(r, &p));
        }
        iteration_rows.push(json!({
            "iteration": loops_done + 1,
            "fsUsePct": df_full,
            "filesWritten": files_written,
            "filesPerSec": a.files_per_sec,
            "appOverheadUsec": a.app_overhead_usec,
            "creatUsec": {"min": a.creat.min, "avg": a.creat.total / p.num_files as u64, "max": a.creat.max},
            "writeUsec": {"min": a.write.min, "avg": a.write_avg / p.num_files as u64, "max": a.write.max},
            "fsyncUsec": {"min": a.fsync.min, "avg": a.fsync.total / p.num_files as u64, "max": a.fsync.max},
            "syncUsec": a.sync_total,
            "closeUsec": {"min": a.close.min, "avg": a.close.total / p.num_files as u64, "max": a.close.max},
            "unlinkUsec": {"min": a.unlink.min, "avg": a.unlink.total / p.num_files as u64, "max": a.unlink.max},
            "thread": tj,
        }));
        rates.push(a.files_per_sec as u64);
        rates_sum += a.files_per_sec as u64;
        loops_done += 1;
        if fs_full || status == "failed" || !(p.fill_fs || p.loop_count > loops_done) {
            break;
        }
    }
    // fs_mark truncates per-iteration rates to integers, sorts descending and
    // indexes percentiles directly out of that array; a fully failed run has no
    // rates at all and reports zeros
    let (average, p50, p90, p99) = if rates.is_empty() {
        (0.0, 0, 0, 0)
    } else {
        rates.sort_unstable_by(|a, b| b.cmp(a));
        let n = rates.len();
        let idx = |f: usize| rates[f.min(n - 1)];
        (
            rates_sum as f64 / n as f64,
            idx(n / 2),
            idx(n * 9 / 10),
            idx(n * 99 / 100),
        )
    };
    info!("Average Files/sec: {average:12.1}");
    info!("p50 Files/sec: {p50}");
    info!("p90 Files/sec: {p90}");
    info!("p99 Files/sec: {p99}");
    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    let report = json!({
        "suite": "fsmark",
        "fsMarkVersion": FS_MARK_VERSION,
        "host": host,
        "date": crate::smallfile::iso8601(suite_start),
        "top": mountpoint.display().to_string(),
        "dirs": p.dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>(),
        "params": {
            "threads": p.threads,
            "files_per_iteration": p.num_files,
            "file_size": p.file_size,
            "io_size": p.io_size,
            "iterations_requested": p.loop_count,
            "iterations_done": loops_done,
            "sync_method": {"id": p.sync_type, "name": sync_method_name(p.sync_type)},
            "subdirs": {"count": p.num_subdirs, "policy": dir_policy_name(p.dir_policy), "files_per_subdir": p.num_per_subdir, "secs_per_subdir": p.secs_per_dir},
            "name_len": p.name_len,
            "rand_len": p.rand_len,
            "keep_files": p.keep_files,
            "fill_fs": p.fill_fs,
            "log_file": p.log_file.as_ref().map(|l| l.display().to_string()),
        },
        "status": status,
        "fsFull": fs_full,
        "filesWritten": files_written,
        "iterations": iteration_rows,
        "summary": {
            "averageFilesPerSec": average,
            "p50FilesPerSec": p50,
            "p90FilesPerSec": p90,
            "p99FilesPerSec": p99,
        },
    });
    Ok(report)
}
