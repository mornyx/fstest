// Single-node threaded port of mdtest (https://github.com/hpc/ior, GPL-2.0).

use crate::common::{fsync_raw, iso8601, suffixed, sync_filesystem};
use log::{info, warn};
use serde_json::{Map, Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TEST_DIR: &str = "test-dir";
const DEFAULT_ITEMS: u64 = 1000;

#[derive(Clone, Copy, PartialEq)]
enum Op {
    DirCreate,
    DirStat,
    DirRename,
    DirRemove,
    FileCreate,
    FileStat,
    FileRead,
    FileRemove,
    TreeCreate,
    TreeRemove,
}

const OPS: [(Op, &str); 10] = [
    (Op::DirCreate, "Directory creation"),
    (Op::DirStat, "Directory stat"),
    (Op::DirRename, "Directory rename"),
    (Op::DirRemove, "Directory removal"),
    (Op::FileCreate, "File creation"),
    (Op::FileStat, "File stat"),
    (Op::FileRead, "File read"),
    (Op::FileRemove, "File removal"),
    (Op::TreeCreate, "Tree creation"),
    (Op::TreeRemove, "Tree removal"),
];

fn op_index(op: Op) -> usize {
    OPS.iter().position(|(o, _)| *o == op).unwrap()
}

#[derive(clap::Args)]
pub struct Args {
    /// number of threads (mdtest ranks)
    #[arg(long, default_value_t = 1)]
    pub threads: u32,

    /// branching factor of the hierarchical directory structure
    #[arg(short = 'b', long, default_value_t = 1)]
    pub branch_factor: u32,

    /// depth of the hierarchical directory structure
    #[arg(short = 'z', long, default_value_t = 0)]
    pub depth: i32,

    /// number of items per directory in the tree
    #[arg(short = 'I', long, default_value_t = 0)]
    pub items_per_dir: u64,

    /// every thread will create/stat/read/remove this many items (defaults to 1000 if neither -n nor -I is set)
    #[arg(short = 'n', long, default_value_t = 0)]
    pub items: u64,

    /// number of iterations
    #[arg(short = 'i', long, default_value_t = 1)]
    pub iterations: u32,

    /// unique working directory per thread
    #[arg(short = 'u', long)]
    pub unique_dir: bool,

    /// test files only (no directories)
    #[arg(short = 'F', long)]
    pub files_only: bool,

    /// test directories only (no files)
    #[arg(short = 'D', long)]
    pub dirs_only: bool,

    /// only create files/dirs
    #[arg(short = 'C', long)]
    pub create_only: bool,

    /// only stat files/dirs
    #[arg(short = 'T', long)]
    pub stat_only: bool,

    /// only read files
    #[arg(short = 'E', long)]
    pub read_only: bool,

    /// only remove files or directories left behind by previous runs
    #[arg(short = 'r', long)]
    pub remove_only: bool,

    /// enable the directory rename phase
    #[arg(short = 'U', long)]
    pub rename_dirs: bool,

    /// files/dirs only at the leaf level of the tree
    #[arg(short = 'L', long)]
    pub leaf_only: bool,

    /// bytes to write to each file after it is created
    #[arg(short = 'w', long, default_value_t = 0)]
    pub write_bytes: u64,

    /// bytes to read from each file
    #[arg(short = 'e', long, default_value_t = 0)]
    pub read_bytes: u64,

    /// fsync each file after writing
    #[arg(short = 'y', long)]
    pub sync_file: bool,

    /// sync filesystem after each phase (included in the timing)
    #[arg(short = 'Y', long)]
    pub sync_after_phase: bool,

    /// rank stride between phases for item access (1 avoids client caches)
    #[arg(short = 'N', long, default_value_t = 0)]
    pub nstride: u32,

    /// random access order for the stat phase
    #[arg(short = 'R', long)]
    pub random: bool,

    /// random seed for -R and data pattern
    #[arg(long, default_value_t = 0)]
    pub random_seed: u64,

    /// stonewall timer in seconds (file creation phase)
    #[arg(short = 'W', long, default_value_t = 0)]
    pub stonewall: u64,

    /// no barriers between phases
    #[arg(short = 'B', long)]
    pub no_barriers: bool,

    /// pre-iteration delay in seconds
    #[arg(short = 'p', long, default_value_t = 0)]
    pub pre_delay: u64,

    /// verify the data read back (requires read phase and --read-bytes > 0)
    #[arg(short = 'X', long)]
    pub verify_read: bool,

    /// verbose debug logging
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// also write the JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

struct Params {
    threads: u32,
    branch_factor: u32,
    depth: i32,
    items_per_dir: u64,
    items: u64,
    items_per_tree: u64,
    leaf_dirs: u64,
    iterations: u32,
    unique_dir: bool,
    dirs_only: bool,
    files_only: bool,
    create_only: bool,
    stat_only: bool,
    read_only: bool,
    remove_only: bool,
    rename_dirs: bool,
    leaf_only: bool,
    write_bytes: u64,
    read_bytes: u64,
    sync_file: bool,
    sync_after_phase: bool,
    nstride: u32,
    random: bool,
    random_seed: u64,
    stonewall: u64,
    barriers: bool,
    pre_delay: u64,
    verify_read: bool,
    num_dirs_in_tree: u64,
    directory_loops: u32,
    top: PathBuf,
}

impl Params {
    fn testdir(&self, iteration: usize, dir_iter: usize) -> PathBuf {
        self.top.join(format!("{TEST_DIR}.{iteration}-{dir_iter}"))
    }

    fn shifted(&self, rank: u32, phase: u32) -> u32 {
        (rank + phase * self.nstride) % self.threads
    }

    fn base_tree_name(&self, target: u32) -> String {
        if self.unique_dir {
            format!("mdtest_tree.{target}")
        } else {
            "mdtest_tree".to_string()
        }
    }

    fn tree_root(&self, iteration: usize, dir_iter: usize, target: u32) -> PathBuf {
        let base = self.base_tree_name(target);
        self.testdir(iteration, dir_iter).join(format!("{base}.0"))
    }

    // Reconstruct path from flat item index: `item_num / items_per_dir` gives the
    // containing directory number; ancestors walk up via `(parent - 1) / branch_factor`.
    fn item_path(&self, base: &str, dirs: bool, prefix: &str, item_num: u64) -> PathBuf {
        let kind = if dirs { "dir" } else { "file" };
        let mut item = format!("{kind}.{prefix}{item_num}");
        let mut parent = item_num / self.items_per_dir;
        if parent > 0 {
            item = format!("{base}.{parent}/{item}");
            while parent > self.branch_factor as u64 {
                parent = (parent - 1) / self.branch_factor as u64;
                item = format!("{base}.{parent}/{item}");
            }
        }
        PathBuf::from(item)
    }

    fn leaf_offset(&self) -> u64 {
        if self.leaf_only {
            self.items_per_dir * (self.num_dirs_in_tree - self.leaf_dirs)
        } else {
            0
        }
    }
}

#[derive(Default, Clone, Copy, Debug)]
struct OpStat {
    time: f64,
    time_before_barrier: f64,
    rate: f64,
    items: u64,
}

struct Progress {
    cur_idx: u64,
    skip_until: u64,
    max_total: Option<u64>,
    total_done: u64,
    items_per_dir: u64,
    stone_wall_timer: u64,
    stonewall_hit: bool,
    start: Instant,
    io_errors: u64,
}

impl Progress {
    fn reset(&mut self, skip_until: u64, max_total: Option<u64>, stone_wall_timer: u64) {
        self.cur_idx = 0;
        self.skip_until = skip_until;
        self.max_total = max_total;
        self.total_done = skip_until;
        self.stone_wall_timer = stone_wall_timer;
        self.stonewall_hit = false;
        self.start = Instant::now();
    }

    fn stopped(&self) -> bool {
        self.stonewall_hit || self.max_total.is_some_and(|m| self.cur_idx >= m)
    }
}

struct RankCtx<'a> {
    p: &'a Params,
    rank: u32,
    barrier: &'a Barrier,
}

impl RankCtx<'_> {
    fn prefix(&self, phase: u32) -> String {
        format!("mdtest.{}.", self.p.shifted(self.rank, phase))
    }

    fn phase_prepare(&self) {
        if self.p.barriers {
            self.barrier.wait();
        }
    }

    fn phase_end(&self) {
        if self.p.sync_after_phase {
            sync_filesystem(&self.p.top);
        }
        if self.p.barriers {
            self.barrier.wait();
        }
    }

    fn create_remove_items(
        &self,
        depth: i32,
        dirs: bool,
        create: bool,
        base_name: &str,
        path: &Path,
        dir_num: u64,
        progress: &mut Progress,
    ) {
        if progress.stopped() {
            return;
        }
        let ipd = progress.items_per_dir;
        if depth == 0 {
            if !self.p.leaf_only || self.p.depth == 0 {
                self.item_range(dirs, create, path, 0, progress);
            }
            if self.p.depth > 0 && !progress.stopped() {
                self.create_remove_items(1, dirs, create, base_name, path, 1, progress);
            }
        } else if depth <= self.p.depth {
            for i in 0..self.p.branch_factor as u64 {
                if progress.stopped() {
                    return;
                }
                let sub = path.join(format!("{base_name}.{}", dir_num + i));
                if !self.p.leaf_only || depth == self.p.depth {
                    self.item_range(dirs, create, &sub, (dir_num + i) * ipd, progress);
                }
                if !progress.stopped() {
                    self.create_remove_items(
                        depth + 1,
                        dirs,
                        create,
                        base_name,
                        &sub,
                        (dir_num + i) * self.p.branch_factor as u64 + 1,
                        progress,
                    );
                }
            }
        }
    }

    fn item_range(&self, dirs: bool, create: bool, path: &Path, start: u64, progress: &mut Progress) {
        let prefix = if create { self.prefix(0) } else { self.prefix(3) };
        for i in 0..progress.items_per_dir {
            if let Some(limit) = progress.max_total
                && progress.cur_idx >= limit
            {
                return;
            }
            if progress.stonewall_hit {
                return;
            }
            let global_idx = progress.cur_idx;
            progress.cur_idx += 1;
            if global_idx < progress.skip_until {
                continue;
            }

            let item_num = start + i;
            let p = if dirs {
                path.join(format!("dir.{prefix}{item_num}"))
            } else {
                path.join(format!("file.{prefix}{item_num}"))
            };
            if dirs {
                let r = if create { fs::create_dir(&p) } else { fs::remove_dir(&p) };
                if let Err(e) = r {
                    progress.io_errors += 1;
                    warn!("unable to process directory {}: {e}", p.display());
                }
            } else if create {
                match OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o644)
                    .open(&p)
                {
                    Ok(mut f) => {
                        if self.p.write_bytes > 0 {
                            let buf = content_pattern(
                                self.p.random_seed,
                                self.p.shifted(self.rank, 0),
                                item_num,
                                self.p.write_bytes as usize,
                            );
                            if let Err(e) = f.write_all(&buf) {
                                progress.io_errors += 1;
                                warn!("unable to write {}: {e}", p.display());
                            }
                        }
                        if self.p.sync_file
                            && let Err(e) = fsync_raw(&f)
                        {
                            progress.io_errors += 1;
                            warn!("unable to fsync {}: {e}", p.display());
                        }
                    }
                    Err(e) => {
                        progress.io_errors += 1;
                        warn!("unable to create {}: {e}", p.display());
                    }
                }
            } else if let Err(e) = fs::remove_file(&p) {
                progress.io_errors += 1;
                warn!("unable to remove file {}: {e}", p.display());
            }
            progress.total_done += 1;
            if progress.stone_wall_timer > 0
                && progress.start.elapsed().as_secs_f64() >= progress.stone_wall_timer as f64
            {
                progress.stonewall_hit = true;
                return;
            }
        }
    }

    fn stat_phase(
        &self,
        dirs: bool,
        tree_root: &Path,
        base: &str,
        stop: u64,
        rand_order: Option<&[u64]>,
        io_errors: &mut u64,
    ) {
        let offset = self.p.leaf_offset();
        let prefix = self.prefix(1);
        for i in 0..stop {
            let item_num = match rand_order {
                Some(arr) => arr[i as usize],
                None => i,
            } + offset;
            let rel = self.p.item_path(base, dirs, &prefix, item_num);
            if let Err(e) = fs::metadata(tree_root.join(&rel)) {
                *io_errors += 1;
                warn!("unable to stat {}: {e}", rel.display());
            }
        }
    }

    fn read_phase(&self, tree_root: &Path, base: &str, stop: u64, io_errors: &mut u64, verify_errors: &mut u64) {
        let offset = self.p.leaf_offset();
        let prefix = self.prefix(2);
        let mut scratch = vec![0u8; self.p.read_bytes as usize];
        for i in 0..stop {
            let item_num = i + offset;
            let rel = self.p.item_path(base, false, &prefix, item_num);
            match File::open(tree_root.join(&rel)) {
                Ok(mut f) => {
                    if self.p.read_bytes > 0 {
                        match f.read_exact(&mut scratch) {
                            Ok(()) => {
                                if self.p.verify_read
                                    && scratch
                                        != content_pattern(
                                            self.p.random_seed,
                                            self.p.shifted(self.rank, 2),
                                            item_num,
                                            scratch.len(),
                                        )
                                {
                                    *verify_errors += 1;
                                }
                            }
                            Err(e) => {
                                if self.p.verify_read && e.kind() == io::ErrorKind::UnexpectedEof {
                                    *verify_errors += 1;
                                } else {
                                    *io_errors += 1;
                                }
                                warn!("unable to read {}: {e}", rel.display());
                            }
                        }
                    }
                }
                Err(e) => {
                    *io_errors += 1;
                    warn!("unable to open {}: {e}", rel.display());
                }
            }
        }
    }

    // Rotating rename chain matching `rename_dir_test()`: item 0 moves to `*-XX`,
    // each subsequent item takes its predecessor's name, and the final step moves
    // `*-XX` onto item `stop - 1`'s original name so all original names are restored.
    fn rename_phase(&self, dirs: bool, tree_root: &Path, base: &str, stop: u64, io_errors: &mut u64) {
        if stop <= 1 {
            return;
        }
        let offset = self.p.leaf_offset();
        let prefix = self.prefix(1);
        let mut last: Option<PathBuf> = None;
        let mut first_renamed: Option<PathBuf> = None;
        for i in 0..stop {
            let item_num = i + offset;
            let path = tree_root.join(self.p.item_path(base, dirs, &prefix, item_num));
            let (src, target) = if i == 0 {
                let t = suffixed(&path, "-XX");
                first_renamed = Some(t.clone());
                (path.clone(), t)
            } else if i == stop - 1 {
                (first_renamed.clone().unwrap(), last.clone().unwrap())
            } else {
                (path.clone(), last.clone().unwrap())
            };
            if let Err(e) = fs::rename(&src, &target) {
                *io_errors += 1;
                warn!("unable to rename {}: {e}", src.display());
            }
            if i != stop - 1 {
                last = Some(path);
            }
        }
    }
}

pub(crate) fn content_pattern(seed: u64, writer: u32, item: u64, len: usize) -> Vec<u8> {
    let mut state = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((writer as u64) << 32)
        .wrapping_add(item.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        v.extend_from_slice(&z.to_le_bytes());
    }
    v.truncate(len);
    v
}

fn shuffled(n: u64, seed: u64) -> Vec<u64> {
    let mut arr: Vec<u64> = (0..n).collect();
    let mut state = seed | 1;
    let mut i = n;
    while i > 1 {
        i -= 1;
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let k = (state >> 33) as u64 % (i + 1);
        arr.swap(k as usize, i as usize);
    }
    arr
}

fn checked_pow_u64(base: u32, exp: u32) -> Result<u64, String> {
    (base as u64)
        .checked_pow(exp)
        .ok_or_else(|| "directory tree size overflows u64; reduce --branch-factor or --depth".to_string())
}

// Never early-returns across rank barriers; errors are recorded in `io_errors`.
fn create_remove_tree(
    p: &Params,
    create: bool,
    parent: &Path,
    base_name: &str,
    depth: i32,
    dir_num: u64,
    io_errors: &mut u64,
) {
    if depth == 0 {
        let root = parent.join(format!("{base_name}.0"));
        if create && let Err(e) = fs::create_dir(&root) {
            *io_errors += 1;
            warn!("unable to create tree dir {}: {e}", root.display());
            return;
        }
        create_remove_tree(p, create, &root, base_name, 1, 1, io_errors);
        if !create && let Err(e) = fs::remove_dir(&root) {
            *io_errors += 1;
            warn!("unable to remove tree dir {}: {e}", root.display());
        }
    } else if depth <= p.depth {
        for i in 0..p.branch_factor as u64 {
            let sub = parent.join(format!("{base_name}.{}", dir_num + i));
            if create && let Err(e) = fs::create_dir(&sub) {
                *io_errors += 1;
                warn!("unable to create tree dir {}: {e}", sub.display());
                continue;
            }
            create_remove_tree(
                p,
                create,
                &sub,
                base_name,
                depth + 1,
                p.branch_factor as u64 * (dir_num + i) + 1,
                io_errors,
            );
            if !create && let Err(e) = fs::remove_dir(&sub) {
                *io_errors += 1;
                warn!("unable to remove tree dir {}: {e}", sub.display());
            }
        }
    }
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let t_suite = Instant::now();
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let threads = args.threads.max(1);
    let iterations = args.iterations.max(1);
    let mut create_only = args.create_only;
    let mut stat_only = args.stat_only;
    let mut read_only = args.read_only;
    let mut remove_only = args.remove_only;
    let mut rename_dirs = args.rename_dirs;
    if !create_only && !stat_only && !read_only && !remove_only && !rename_dirs {
        create_only = true;
        stat_only = true;
        read_only = true;
        remove_only = true;
        rename_dirs = true;
    }
    let mut dirs_only = args.dirs_only;
    let mut files_only = args.files_only;
    if !dirs_only && !files_only {
        dirs_only = true;
        files_only = true;
    }
    let barriers = !args.no_barriers;
    if args.stonewall > 0 && (args.branch_factor > 1 || !barriers) {
        return Err("stonewall timer only works with branch factor <= 1 and with barriers".into());
    }
    if !barriers && args.nstride != 0 {
        return Err("no-barriers is not compatible with nstride (races between phases)".into());
    }
    if args.depth < 0 {
        return Err("depth must be >= 0".into());
    }
    if args.branch_factor < 1 && args.depth > 0 {
        return Err("branch factor must be >= 1 when depth > 0".into());
    }
    let raw_items = if args.items == 0 && args.items_per_dir == 0 {
        DEFAULT_ITEMS
    } else {
        args.items
    };
    if raw_items > 0 && args.items_per_dir > 0 && args.unique_dir {
        return Err("with --unique-dir, specify either --items or --items-per-dir, not both".into());
    }
    if args.verify_read && !read_only {
        return Err("verify-read requires the read phase (--read-only)".into());
    }
    if args.verify_read && args.read_bytes == 0 {
        return Err("verify-read requires read bytes > 0".into());
    }
    if create_only && read_only && args.read_bytes > args.write_bytes {
        return Err("read bytes must be <= write bytes when writing and reading".into());
    }

    let leaf_dirs = if args.depth <= 0 || args.branch_factor <= 1 {
        1
    } else {
        checked_pow_u64(args.branch_factor, args.depth as u32)?
    };
    let num_dirs_in_tree = if args.depth <= 0 || args.branch_factor < 1 {
        1
    } else if args.branch_factor == 1 {
        args.depth as u64 + 1
    } else {
        let top_pow = checked_pow_u64(args.branch_factor, args.depth as u32 + 1)?;
        (top_pow - 1) / (args.branch_factor as u64 - 1)
    };
    let active_dirs_per_tree = if args.leaf_only { leaf_dirs } else { num_dirs_in_tree };

    let (items, items_per_dir, directory_loops) = if args.items_per_dir > 0 {
        let per_tree = args
            .items_per_dir
            .checked_mul(active_dirs_per_tree)
            .ok_or_else(|| "items per tree overflows u64".to_string())?;
        if raw_items == 0 {
            (per_tree, args.items_per_dir, 1u32)
        } else {
            if raw_items % per_tree != 0 {
                return Err(format!(
                    "items ({raw_items}) must be a multiple of items per tree ({per_tree})"
                ));
            }
            let loops =
                u32::try_from(raw_items / per_tree).map_err(|_| "directory loop count overflows u32".to_string())?;
            (raw_items, args.items_per_dir, loops.max(1))
        }
    } else {
        let ipd = raw_items / active_dirs_per_tree;
        let items = ipd * active_dirs_per_tree;
        (items, ipd, 1u32)
    };
    if items_per_dir == 0 {
        return Err("computed zero items per directory; increase --items or set --items-per-dir".into());
    }
    let items_per_tree = items / directory_loops as u64;

    let random_seed = if args.random && args.random_seed == 0 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    } else {
        args.random_seed
    };
    let p = Params {
        threads,
        branch_factor: args.branch_factor,
        depth: args.depth,
        items_per_dir,
        items,
        items_per_tree,
        leaf_dirs,
        iterations,
        unique_dir: args.unique_dir,
        dirs_only,
        files_only,
        create_only,
        stat_only,
        read_only,
        remove_only,
        rename_dirs,
        leaf_only: args.leaf_only,
        write_bytes: args.write_bytes,
        read_bytes: args.read_bytes,
        sync_file: args.sync_file,
        sync_after_phase: args.sync_after_phase,
        nstride: args.nstride,
        random: args.random,
        random_seed,
        stonewall: args.stonewall,
        barriers,
        pre_delay: args.pre_delay,
        verify_read: args.verify_read,
        num_dirs_in_tree,
        directory_loops,
        top: mountpoint.to_path_buf(),
    };
    info!(
        "mdtest: top={} threads={} items={} items_per_dir={} depth={} branch={} dirs_in_tree={} unique={} loops={}",
        p.top.display(),
        p.threads,
        p.items,
        p.items_per_dir,
        p.depth,
        p.branch_factor,
        p.num_dirs_in_tree,
        p.unique_dir,
        p.directory_loops
    );
    let barrier = Arc::new(Barrier::new(threads as usize));
    let stonewall_max: Arc<Vec<AtomicU64>> = Arc::new((0..p.iterations).map(|_| AtomicU64::new(0)).collect());
    let all_stats: Arc<Mutex<Vec<Vec<[OpStat; 10]>>>> = Arc::new(Mutex::new(
        (0..p.iterations)
            .map(|_| (0..threads).map(|_| [OpStat::default(); 10]).collect())
            .collect(),
    ));
    let rank_errors: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(vec![(0, 0); threads as usize]));

    thread::scope(|s| {
        let mut joins = Vec::new();
        for rank in 0..threads {
            let barrier = barrier.clone();
            let stats = all_stats.clone();
            let errors = rank_errors.clone();
            let sw_max = stonewall_max.clone();
            let pp = &p;
            joins.push(s.spawn(move || {
                run_rank(pp, rank, &barrier, &sw_max, &stats, &errors);
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
    });

    let all_stats = Arc::try_unwrap(all_stats).unwrap().into_inner().unwrap();
    let rank_errors = Arc::try_unwrap(rank_errors).unwrap().into_inner().unwrap();
    let io_errors: u64 = rank_errors.iter().map(|(io_e, _)| *io_e).sum();
    let verification_errors: u64 = rank_errors.iter().map(|(_, ve)| *ve).sum();
    if io_errors > 0 || verification_errors > 0 {
        warn!("mdtest finished with io_errors={io_errors} verification_errors={verification_errors}");
    }

    let mut iterations_json = Vec::new();
    for (it, per_rank) in all_stats.iter().enumerate() {
        let mut ops = Map::new();
        for (oi, (_, name)) in OPS.iter().enumerate() {
            let mut ranks = Map::new();
            for (r, st) in per_rank.iter().enumerate() {
                if st[oi].items == 0 {
                    continue;
                }
                ranks.insert(
                    r.to_string(),
                    json!({
                        "time": st[oi].time,
                        "timeBeforeBarrier": st[oi].time_before_barrier,
                        "rate": st[oi].rate,
                        "items": st[oi].items,
                    }),
                );
            }
            if !ranks.is_empty() {
                ops.insert(name.to_string(), json!({ "rank": ranks }));
            }
        }
        iterations_json.push(json!({ "iteration": it + 1, "results": ops }));
    }

    let mut summary = Map::new();
    for (oi, (_, name)) in OPS.iter().enumerate() {
        let mut rank_means = Vec::new();
        for r in 0..threads as usize {
            let rates: Vec<f64> = all_stats
                .iter()
                .filter(|per_rank| per_rank[r][oi].items > 0)
                .map(|per_rank| per_rank[r][oi].rate)
                .collect();
            if !rates.is_empty() {
                rank_means.push(rates.iter().sum::<f64>() / rates.len() as f64);
            }
        }
        if !rank_means.is_empty() {
            let sum: f64 = rank_means.iter().sum();
            summary.insert(
                name.to_string(),
                json!({
                    "totalRate": sum,
                    "minRate": rank_means.iter().cloned().fold(f64::INFINITY, f64::min),
                    "maxRate": rank_means.iter().cloned().fold(0.0, f64::max),
                    "meanRatePerRank": sum / rank_means.len() as f64,
                }),
            );
            info!(
                "{name:24}: mean rate/rank {:10.1} ops/s, sum {:10.1} ops/s",
                sum / rank_means.len() as f64,
                sum
            );
        }
    }

    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string());
    let failed = io_errors > 0 || verification_errors > 0;
    Ok(json!({
        "suite": "mdtest",
        "host": host,
        "date": iso8601(SystemTime::now()),
        "durationMs": t_suite.elapsed().as_millis() as u64,
        "top": mountpoint.display().to_string(),
        "params": {
            "threads": p.threads, "items": p.items, "items_per_dir": p.items_per_dir,
            "depth": p.depth, "branch_factor": p.branch_factor, "dirs_in_tree": p.num_dirs_in_tree,
            "iterations": p.iterations, "unique_dir_per_task": p.unique_dir,
            "write_bytes": p.write_bytes, "read_bytes": p.read_bytes,
            "sync_file": p.sync_file, "sync_after_phase": p.sync_after_phase,
            "nstride": p.nstride, "random": p.random, "random_seed": p.random_seed,
            "stonewall_sec": p.stonewall, "barriers": p.barriers,
            "files_only": p.files_only, "dirs_only": p.dirs_only, "leaf_only": p.leaf_only,
        },
        "status": if failed { "failed" } else { "ok" },
        "ioErrors": io_errors,
        "verificationErrors": verification_errors,
        "iterations": iterations_json,
        "summary": summary,
    }))
}

fn run_rank(
    p: &Params,
    rank: u32,
    barrier: &Barrier,
    stonewall_max: &[AtomicU64],
    stats: &Arc<Mutex<Vec<Vec<[OpStat; 10]>>>>,
    errors: &Arc<Mutex<Vec<(u64, u64)>>>,
) {
    let default_rand_order = if p.random {
        Some(shuffled(p.items_per_tree, p.random_seed + rank as u64))
    } else {
        None
    };
    let ctx = RankCtx { p, rank, barrier };
    let mut io_errors = 0u64;
    let mut verification_errors = 0u64;

    for iteration in 0..p.iterations as usize {
        let mut res = [OpStat::default(); 10];
        let mut progress = Progress {
            cur_idx: 0,
            skip_until: 0,
            max_total: None,
            total_done: 0,
            items_per_dir: p.items_per_dir,
            stone_wall_timer: 0,
            stonewall_hit: false,
            start: Instant::now(),
            io_errors: 0,
        };

        if p.create_only {
            if rank == 0 {
                for dir_iter in 0..p.directory_loops as usize {
                    if let Err(e) = fs::create_dir(p.testdir(iteration, dir_iter))
                        && e.kind() != io::ErrorKind::AlreadyExists
                    {
                        io_errors += 1;
                        warn!("unable to create testdir: {e}");
                    }
                }
            }
            barrier.wait();
            let t0 = Instant::now();
            let tree_base = p.base_tree_name(rank);
            for dir_iter in 0..p.directory_loops as usize {
                let testdir = p.testdir(iteration, dir_iter);
                if p.unique_dir || rank == 0 {
                    create_remove_tree(p, true, &testdir, &tree_base, 0, 0, &mut io_errors);
                }
            }
            barrier.wait();
            let elapsed = t0.elapsed().as_secs_f64();
            if p.unique_dir || rank == 0 {
                let total_tree_dirs = p.num_dirs_in_tree * p.directory_loops as u64;
                set_stat(&mut res, Op::TreeCreate, total_tree_dirs, elapsed, elapsed);
            }
        }

        let root =
            |phase: u32, dir_iter: usize| -> PathBuf { p.tree_root(iteration, dir_iter, p.shifted(rank, phase)) };
        let base = |phase: u32| -> String { p.base_tree_name(p.shifted(rank, phase)) };

        if p.dirs_only {
            if p.pre_delay > 0 {
                thread::sleep(Duration::from_secs(p.pre_delay));
            }
            if p.create_only {
                ctx.phase_prepare();
                let t0 = Instant::now();
                progress.reset(0, None, 0);
                let b0 = base(0);
                for dir_iter in 0..p.directory_loops as usize {
                    ctx.create_remove_items(0, true, true, &b0, &root(0, dir_iter), 0, &mut progress);
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::DirCreate, p.items, t0.elapsed().as_secs_f64(), tbb);
            }
            if p.stat_only {
                ctx.phase_prepare();
                let t0 = Instant::now();
                let b1 = base(1);
                for dir_iter in 0..p.directory_loops as usize {
                    ctx.stat_phase(
                        true,
                        &root(1, dir_iter),
                        &b1,
                        p.items_per_tree,
                        default_rand_order.as_deref(),
                        &mut io_errors,
                    );
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::DirStat, p.items, t0.elapsed().as_secs_f64(), tbb);
            }
            if p.rename_dirs && p.items_per_tree > 1 {
                ctx.phase_prepare();
                let t0 = Instant::now();
                let b1 = base(1);
                for dir_iter in 0..p.directory_loops as usize {
                    ctx.rename_phase(true, &root(1, dir_iter), &b1, p.items_per_tree, &mut io_errors);
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::DirRename, p.items, t0.elapsed().as_secs_f64(), tbb);
            }
            if p.remove_only {
                ctx.phase_prepare();
                let t0 = Instant::now();
                progress.reset(0, None, 0);
                let b3 = base(3);
                for dir_iter in 0..p.directory_loops as usize {
                    ctx.create_remove_items(0, true, false, &b3, &root(3, dir_iter), 0, &mut progress);
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::DirRemove, p.items, t0.elapsed().as_secs_f64(), tbb);
            }
        }

        if p.files_only {
            if p.pre_delay > 0 && !p.dirs_only {
                thread::sleep(Duration::from_secs(p.pre_delay));
            }
            let mut effective_items = p.items;
            if p.create_only {
                ctx.phase_prepare();
                let t0 = Instant::now();
                progress.reset(0, None, p.stonewall);
                let b0 = base(0);
                for dir_iter in 0..p.directory_loops as usize {
                    ctx.create_remove_items(0, false, true, &b0, &root(0, dir_iter), 0, &mut progress);
                }
                let done = progress.total_done.min(p.items);
                if p.stonewall > 0 {
                    stonewall_max[iteration].fetch_max(done, Ordering::SeqCst);
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::FileCreate, done, t0.elapsed().as_secs_f64(), tbb);
                if p.stonewall > 0 {
                    effective_items = stonewall_max[iteration].load(Ordering::SeqCst);
                    if done < p.items {
                        info!(
                            "thread {rank} hit stonewall after {done} of {} items (wear-out target {effective_items})",
                            p.items
                        );
                    }
                    // Top up slower ranks to `effective_items` so shifted stat/read/remove phases never ENOENT.
                    if done < effective_items {
                        progress.reset(done, Some(effective_items), 0);
                        for dir_iter in 0..p.directory_loops as usize {
                            ctx.create_remove_items(0, false, true, &b0, &root(0, dir_iter), 0, &mut progress);
                        }
                    }
                    barrier.wait();
                }
            }
            if p.stat_only && effective_items > 0 {
                ctx.phase_prepare();
                let t0 = Instant::now();
                let b1 = base(1);
                let mut rem = effective_items;
                for dir_iter in 0..p.directory_loops as usize {
                    let take = rem.min(p.items_per_tree);
                    rem -= take;
                    if take == 0 {
                        break;
                    }
                    let custom_rand = if p.random && take < p.items_per_tree {
                        Some(shuffled(take, p.random_seed + rank as u64))
                    } else {
                        None
                    };
                    let order = custom_rand.as_deref().or(default_rand_order.as_deref());
                    ctx.stat_phase(false, &root(1, dir_iter), &b1, take, order, &mut io_errors);
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::FileStat, effective_items, t0.elapsed().as_secs_f64(), tbb);
            }
            if p.read_only && effective_items > 0 {
                ctx.phase_prepare();
                let t0 = Instant::now();
                let b2 = base(2);
                let mut rem = effective_items;
                for dir_iter in 0..p.directory_loops as usize {
                    let take = rem.min(p.items_per_tree);
                    rem -= take;
                    if take == 0 {
                        break;
                    }
                    ctx.read_phase(&root(2, dir_iter), &b2, take, &mut io_errors, &mut verification_errors);
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::FileRead, effective_items, t0.elapsed().as_secs_f64(), tbb);
            }
            if p.remove_only && effective_items > 0 {
                ctx.phase_prepare();
                let t0 = Instant::now();
                progress.reset(0, Some(effective_items), 0);
                let b3 = base(3);
                for dir_iter in 0..p.directory_loops as usize {
                    ctx.create_remove_items(0, false, false, &b3, &root(3, dir_iter), 0, &mut progress);
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(
                    &mut res,
                    Op::FileRemove,
                    effective_items,
                    t0.elapsed().as_secs_f64(),
                    tbb,
                );
            }
        }

        barrier.wait();
        if p.remove_only {
            let t0 = Instant::now();
            let tree_base = p.base_tree_name(rank);
            for dir_iter in 0..p.directory_loops as usize {
                let testdir = p.testdir(iteration, dir_iter);
                if p.unique_dir || rank == 0 {
                    create_remove_tree(p, false, &testdir, &tree_base, 0, 0, &mut io_errors);
                }
            }
            barrier.wait();
            if rank == 0 {
                for dir_iter in 0..p.directory_loops as usize {
                    if let Err(e) = fs::remove_dir(p.testdir(iteration, dir_iter)) {
                        io_errors += 1;
                        warn!("unable to remove testdir: {e}");
                    }
                }
            }
            barrier.wait();
            let elapsed = t0.elapsed().as_secs_f64();
            if p.unique_dir || rank == 0 {
                let total_tree_dirs = p.num_dirs_in_tree * p.directory_loops as u64;
                set_stat(&mut res, Op::TreeRemove, total_tree_dirs, elapsed, elapsed);
            }
        }

        io_errors += progress.io_errors;
        stats.lock().unwrap()[iteration][rank as usize] = res;
    }

    errors.lock().unwrap()[rank as usize] = (io_errors, verification_errors);
}

fn set_stat(res: &mut [OpStat; 10], op: Op, items: u64, t: f64, tbb: f64) {
    let i = op_index(op);
    res[i].time = t;
    res[i].time_before_barrier = tbb;
    res[i].items = items;
    res[i].rate = if t > 0.0 { items as f64 / t } else { 0.0 };
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

    fn parse_args(itr: &[&str]) -> Args {
        let mut full = vec!["mdtest"];
        full.extend_from_slice(itr);
        TestCli::parse_from(full).args
    }

    fn make_temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "fstest-mdtest-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn zero_arg_default_and_unique_dir_hierarchical_verify() {
        let dir = make_temp_dir("unique");
        let default_res = run(&dir, &parse_args(&["-n", "20"])).unwrap();
        assert_eq!(default_res["status"], "ok");
        assert_eq!(default_res["ioErrors"], 0);
        assert!(default_res["summary"].get("Directory read").is_none());

        let unique_res = run(
            &dir,
            &parse_args(&[
                "--threads",
                "3",
                "-u",
                "-z",
                "2",
                "-b",
                "2",
                "-I",
                "4",
                "-N",
                "1",
                "-w",
                "128",
                "-e",
                "128",
                "-X",
                "-y",
                "-R",
            ]),
        )
        .unwrap();
        assert_eq!(unique_res["status"], "ok");
        assert_eq!(unique_res["ioErrors"], 0);
        assert_eq!(unique_res["verificationErrors"], 0);
        // 7 dirs in tree * 4 items/dir = 28 items per rank
        assert_eq!(
            unique_res["iterations"][0]["results"]["File creation"]["rank"]["0"]["items"],
            28
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_directory_loops_and_failure_on_missing_stat_or_repeat_create() {
        let dir = make_temp_dir("loops");
        let loops_res = run(&dir, &parse_args(&["--threads", "2", "-n", "12", "-I", "4", "-R"])).unwrap();
        assert_eq!(loops_res["status"], "ok");
        assert_eq!(loops_res["ioErrors"], 0);

        let missing = run(&dir, &parse_args(&["--threads", "2", "-T", "-n", "10"])).unwrap();
        assert_eq!(missing["status"], "failed");
        assert!(missing["ioErrors"].as_u64().unwrap() > 0);

        let c1 = run(&dir, &parse_args(&["--threads", "2", "-C", "-F", "-n", "6"])).unwrap();
        assert_eq!(c1["status"], "ok");
        let c2 = run(&dir, &parse_args(&["--threads", "2", "-C", "-F", "-n", "6"])).unwrap();
        assert_eq!(c2["status"], "failed");
        let rm = run(&dir, &parse_args(&["--threads", "2", "-r", "-F", "-n", "6"])).unwrap();
        assert_eq!(rm["status"], "ok");
        let _ = fs::remove_dir_all(&dir);
    }
}
