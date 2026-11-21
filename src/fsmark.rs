// Port of `fs_mark` 3.3 (https://github.com/josefbacik/fs_mark):
// synchronous/asynchronous file-creation rate and per-syscall microsecond benchmark.

use crate::common::{SplitMix64, fsync_raw, host_name, iso8601, sync_filesystem};
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

    /// number of files to create per iteration per thread
    #[arg(short = 'n', long, default_value_t = 1000)]
    pub files: u32,

    /// size in bytes of each file
    #[arg(short = 's', long, default_value_t = 50 * 1024)]
    pub file_size: u64,

    /// bytes per write() call
    #[arg(short = 'w', long, default_value_t = 16 * 1024)]
    pub write_size: usize,

    /// number of iterations (implies keeping files across iterations)
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

    /// keep files after completion
    #[arg(short = 'k', long)]
    pub keep_files: bool,

    /// run until the filesystem is full (implies keeping files)
    #[arg(short = 'F', long)]
    pub fill_fs: bool,

    /// fill write buffer with deterministic pseudo-random bytes instead of zeros
    #[arg(long)]
    pub incompressible: bool,

    /// append the fs_mark-format text report to this log file
    #[arg(short = 'l', long)]
    pub log_file: Option<PathBuf>,

    /// include per-system-call min/avg/max stats in the text report
    #[arg(short = 'v', long)]
    pub verbose_stats: bool,

    /// debug logging
    #[arg(long)]
    pub verbose: bool,

    /// write the enriched JSON result to this file
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
    incompressible: bool,
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
                return Err(
                    "Must specify more than 1 subdirectory with -D for files-per-subdir to make sense"
                        .into(),
                );
            }
            dir_policy = DirPolicy::RoundRobin;
            n.max(1)
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
    } else if !threads.is_multiple_of(num_dirs) {
        return Err(format!(
            "Threads ({threads}) must be an even multiple of directories ({num_dirs}) and <= {MAX_THREADS}"
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
        incompressible: args.incompressible,
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

#[derive(Default)]
struct Row {
    file_count: u64,
    files_per_sec: f64,
    app_overhead_usec: i64,
    creat: Stat,
    write: Stat,
    write_avg_acc: u64,
    fsync: Stat,
    sync_usec: u64,
    close: Stat,
    unlink: Stat,
}

enum RunErr {
    SpaceFull(PathBuf),
    Io(io::Error),
}

fn map_io_err(e: io::Error, dir: &Path, fill_fs: bool) -> RunErr {
    if fill_fs && matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT)) {
        RunErr::SpaceFull(dir.to_path_buf())
    } else {
        RunErr::Io(e)
    }
}

fn mkdir_eexist(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o777).create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

fn statfs_vals(dir: &Path) -> io::Result<(u64, u64, u64)> {
    let c = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("path contains NUL"))?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    let unit = if st.f_frsize > 0 {
        st.f_frsize as u64
    } else {
        st.f_bsize as u64
    };
    #[cfg(not(target_os = "linux"))]
    let unit = st.f_bsize as u64;
    Ok((st.f_blocks as u64, st.f_bavail as u64, unit.max(512)))
}

fn free_bytes(dir: &Path) -> io::Result<u64> {
    let (blocks, bavail, unit) = statfs_vals(dir)?;
    if blocks == 0 {
        return Ok(u64::MAX);
    }
    Ok(bavail.saturating_mul(unit))
}

fn fs_use_pct(dir: &Path) -> io::Result<u32> {
    let (blocks, bavail, _) = statfs_vals(dir)?;
    if blocks == 0 {
        return Ok(0);
    }
    let used = blocks.saturating_sub(bavail);
    Ok((100u64.saturating_mul(used) / blocks) as u32)
}

struct ThreadState {
    dir: PathBuf,
    rng: SplitMix64,
    current_subdir: usize,
    last_mkdir_subdir: Option<usize>,
    start_sec_time: u64,
    files_in_subdir: usize,
}

fn setup_file_name(p: &Params, st: &mut ThreadState) -> io::Result<(PathBuf, String)> {
    let sec_time = unix_secs();
    if st.start_sec_time == 0 {
        st.start_sec_time = sec_time;
    }
    let target_dir = match p.dir_policy {
        DirPolicy::NoSubdirs => st.dir.clone(),
        DirPolicy::RoundRobin => {
            if st.files_in_subdir >= p.num_per_subdir {
                st.current_subdir += 1;
                st.files_in_subdir = 0;
            }
            st.current_subdir %= p.num_subdirs;
            st.files_in_subdir += 1;
            let d = st.dir.join(format!("{:02x}", st.current_subdir));
            if st.last_mkdir_subdir != Some(st.current_subdir) {
                mkdir_eexist(&d)?;
                st.last_mkdir_subdir = Some(st.current_subdir);
            }
            d
        }
        DirPolicy::TimeHash => {
            if sec_time - st.start_sec_time > p.secs_per_dir {
                st.current_subdir = (st.current_subdir + 1) % p.num_subdirs;
                st.start_sec_time = sec_time;
            }
            let d = st.dir.join(format!("{:02x}", st.current_subdir));
            if st.last_mkdir_subdir != Some(st.current_subdir) {
                mkdir_eexist(&d)?;
                st.last_mkdir_subdir = Some(st.current_subdir);
            }
            d
        }
    };

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
            let val = st.rng.next_u64();
            let c = b'0' + (val & 0x7f) as u8;
            if c.is_ascii_uppercase() || c.is_ascii_digit() {
                name.push(c as char);
                break;
            }
        }
    }
    Ok((target_dir, name))
}

fn write_file(
    f: &mut File,
    buf: &[u8],
    sz: u64,
    write_stat: &mut Stat,
    write_avg_acc: &mut u64,
) -> io::Result<()> {
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
    if calls > 0 {
        *write_avg_acc += local / calls;
    }
    Ok(())
}

fn run_thread(p: &Params, tid: usize, io_buffer: &[u8]) -> Result<Row, RunErr> {
    let dir = p.dirs[tid % p.dirs.len()].clone();
    mkdir_eexist(&dir).map_err(|e| map_io_err(e, &dir, p.fill_fs))?;
    let threads_per_dir = (p.threads / p.dirs.len().max(1)).max(1) as u64;
    let bytes_needed = p
        .file_size
        .saturating_mul(p.num_files as u64)
        .saturating_mul(threads_per_dir);
    if free_bytes(&dir).map_err(RunErr::Io)? < bytes_needed {
        return Err(RunErr::SpaceFull(dir));
    }

    let num_subdirs = p.num_subdirs.max(1);
    let mut st = ThreadState {
        dir: dir.clone(),
        rng: SplitMix64::new(unix_nanos() as u64 ^ (tid as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)),
        current_subdir: ((unix_secs() as usize).wrapping_add(tid)) % num_subdirs,
        last_mkdir_subdir: None,
        start_sec_time: 0,
        files_in_subdir: 0,
    };
    let mut row = Row::default();
    let mut names: Vec<PathBuf> = Vec::with_capacity(p.num_files as usize);
    let loop_start = Instant::now();
    for _ in 0..p.num_files {
        let (target_dir, fname) =
            setup_file_name(p, &mut st).map_err(|e| map_io_err(e, &dir, p.fill_fs))?;
        let path = target_dir.join(&fname);
        let t0 = Instant::now();
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o666)
            .open(&path)
            .map_err(|e| map_io_err(e, &dir, p.fill_fs))?;
        row.creat.add(us(t0.elapsed()));
        names.push(path);
        write_file(&mut f, io_buffer, p.file_size, &mut row.write, &mut row.write_avg_acc)
            .map_err(|e| map_io_err(e, &dir, p.fill_fs))?;
        if p.sync_bits & FSYNC_BEFORE_CLOSE != 0 {
            let t0 = Instant::now();
            fsync_raw(&f).map_err(|e| map_io_err(e, &dir, p.fill_fs))?;
            row.fsync.add(us(t0.elapsed()));
        }
        let t0 = Instant::now();
        drop(f);
        row.close.add(us(t0.elapsed()));
    }
    if p.sync_bits & FSYNC_SYNC_SYSCALL != 0 {
        let t0 = Instant::now();
        sync_filesystem(&dir);
        row.sync_usec = us(t0.elapsed());
    }
    let post_fsync = |row: &mut Row, path: &Path| -> Result<(), RunErr> {
        let t0 = Instant::now();
        let f = File::open(path).map_err(|e| map_io_err(e, &dir, p.fill_fs))?;
        fsync_raw(&f).map_err(|e| map_io_err(e, &dir, p.fill_fs))?;
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
    if p.sync_bits & FSYNC_FIRST_FILE != 0 && !names.is_empty() {
        let t0 = Instant::now();
        let f = File::open(&names[0]).map_err(|e| map_io_err(e, &dir, p.fill_fs))?;
        fsync_raw(&f).map_err(|e| map_io_err(e, &dir, p.fill_fs))?;
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
    let total_ops =
        row.creat.total + row.write.total + row.fsync.total + row.sync_usec + row.close.total;
    row.app_overhead_usec = loop_usecs as i64 - total_ops as i64;
    row.file_count = p.num_files as u64;
    row.files_per_sec = p.num_files as f64 / (loop_usecs.max(1) as f64 / 1e6);
    Ok(row)
}

#[derive(Default)]
struct Agg {
    file_count: u64,
    files_per_sec: f64,
    app_overhead_usec: i64,
    creat: Stat,
    creat_avg: u64,
    write: Stat,
    write_avg: u64,
    fsync: Stat,
    fsync_avg: u64,
    sync_avg: u64,
    close: Stat,
    close_avg: u64,
    unlink: Stat,
    unlink_avg: u64,
}

fn aggregate(rows: &[Row], per_thread_files: u64) -> Agg {
    let mut a = Agg::default();
    if rows.is_empty() {
        return a;
    }
    let mut write_avg_sum = 0u64;
    let mut sync_sum = 0u64;
    for r in rows {
        a.file_count += r.file_count;
        a.files_per_sec += r.files_per_sec;
        a.app_overhead_usec += r.app_overhead_usec;
        a.creat.merge(&r.creat);
        a.write.merge(&r.write);
        a.fsync.merge(&r.fsync);
        a.close.merge(&r.close);
        a.unlink.merge(&r.unlink);
        write_avg_sum += r.write_avg_acc / per_thread_files.max(1);
        sync_sum += r.sync_usec;
    }
    let nthreads = rows.len() as u64;
    let total_files = a.file_count.max(1);
    a.creat_avg = a.creat.total / total_files;
    a.write_avg = write_avg_sum / nthreads;
    a.fsync_avg = a.fsync.total / total_files;
    a.sync_avg = sync_sum / nthreads;
    a.close_avg = a.close.total / total_files;
    a.unlink_avg = a.unlink.total / total_files;
    a
}

fn ctime_now() -> String {
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
    if p.verbose_stats {
        format!(
            "{:6} {:>12} {:>12} {:>12.1} {:>16} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
            df_full,
            files_written,
            p.file_size,
            a.files_per_sec,
            a.app_overhead_usec,
            a.creat.min,
            a.creat_avg,
            a.creat.max,
            a.write.min,
            a.write_avg,
            a.write.max,
            a.fsync.min,
            a.fsync_avg,
            a.fsync.max,
            0,
            a.sync_avg,
            0,
            a.close.min,
            a.close_avg,
            a.close.max,
            a.unlink.min,
            a.unlink_avg,
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
    let n = (p.num_files as u64).max(1);
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
    let p = resolve(args, mountpoint)?;
    let mut io_buffer = vec![0u8; p.io_size];
    if p.incompressible {
        SplitMix64::new(0x4653_4D41_524B_3333).fill_bytes(&mut io_buffer);
    }

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
        "fsmark: dirs={} threads={} files={} size={}B io={}B sync={} subdirs={} policy={} keep_files={} iterations={}",
        p.dirs.len(),
        p.threads,
        p.num_files,
        p.file_size,
        p.io_size,
        p.sync_type,
        p.num_subdirs,
        dir_policy_name(p.dir_policy),
        p.keep_files,
        p.loop_count.max(1),
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
    let mut first_failure: Option<String> = None;
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
                    let msg = format!("iteration {} failed: {e}", loops_done + 1);
                    warn!("{msg}");
                    if first_failure.is_none() {
                        first_failure = Some(msg);
                    }
                    status = "failed";
                }
            }
        }
        if rows.is_empty() {
            break;
        }
        let a = aggregate(&rows, p.num_files as u64);
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
            "creatUsec": {"min": a.creat.min, "avg": a.creat_avg, "max": a.creat.max},
            "writeUsec": {"min": a.write.min, "avg": a.write_avg, "max": a.write.max},
            "fsyncUsec": {"min": a.fsync.min, "avg": a.fsync_avg, "max": a.fsync.max},
            "syncUsec": a.sync_avg,
            "closeUsec": {"min": a.close.min, "avg": a.close_avg, "max": a.close.max},
            "unlinkUsec": {"min": a.unlink.min, "avg": a.unlink_avg, "max": a.unlink.max},
            "thread": tj,
        }));
        rates.push(a.files_per_sec as u64);
        rates_sum += a.files_per_sec as u64;
        loops_done += 1;
        if fs_full || status == "failed" || !(p.fill_fs || p.loop_count > loops_done) {
            break;
        }
    }

    if !p.keep_files && p.num_subdirs > 0 {
        for d in &p.dirs {
            for s in 0..p.num_subdirs {
                let _ = fs::remove_dir(d.join(format!("{s:02x}")));
            }
        }
    }

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

    let mut report = json!({
        "suite": "fsmark",
        "fsMarkVersion": FS_MARK_VERSION,
        "host": host_name(),
        "date": iso8601(suite_start),
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
            "incompressible": p.incompressible,
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
    if let Some(f) = first_failure {
        report["failure"] = json!(f);
    }
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
    fn aggregate_divides_per_syscall_averages_by_total_files_not_single_thread() {
        let r1 = Row {
            file_count: 10,
            files_per_sec: 100.0,
            app_overhead_usec: 50,
            creat: Stat { min: 10, max: 30, total: 200 },
            write: Stat { min: 5, max: 15, total: 100 },
            write_avg_acc: 100,
            fsync: Stat { min: 40, max: 80, total: 600 },
            sync_usec: 1000,
            close: Stat { min: 2, max: 6, total: 40 },
            unlink: Stat { min: 3, max: 9, total: 60 },
        };
        let r2 = Row {
            file_count: 10,
            files_per_sec: 100.0,
            app_overhead_usec: 50,
            creat: Stat { min: 20, max: 40, total: 300 },
            write: Stat { min: 5, max: 15, total: 100 },
            write_avg_acc: 100,
            fsync: Stat { min: 50, max: 90, total: 700 },
            sync_usec: 1400,
            close: Stat { min: 4, max: 8, total: 60 },
            unlink: Stat { min: 5, max: 11, total: 80 },
        };
        let agg = aggregate(&[r1, r2], 10);
        assert_eq!(agg.file_count, 20);
        assert_eq!(agg.creat_avg, 25); // (200 + 300) / 20, NOT 50!
        assert_eq!(agg.fsync_avg, 65); // (600 + 700) / 20, NOT 130!
        assert_eq!(agg.sync_avg, 1200); // (1000 + 1400) / 2
        assert_eq!(agg.close_avg, 5);
        assert_eq!(agg.unlink_avg, 7);
    }

    #[test]
    fn fsmark_multithread_subdirs_runs_and_cleans_up_when_not_keeping_files() {
        let dir = std::env::temp_dir().join(format!("fstest-fsmark-unit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let cli = TestCli::parse_from([
            "fsmark",
            "-n",
            "20",
            "-s",
            "4096",
            "-S",
            "1",
            "-t",
            "2",
            "-D",
            "4",
            "-N",
            "5",
            "--incompressible",
            "-v",
        ]);
        let rep = run(&dir, &cli.args).expect("fsmark run");
        assert_eq!(rep["status"], json!("ok"));
        assert_eq!(rep["filesWritten"], json!(40));
        assert!(fs::read_dir(&dir).unwrap().next().is_none());

        let _ = fs::remove_dir_all(&dir);
    }
}
