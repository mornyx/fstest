// Behavior-level port of mdtest from https://github.com/hpc/ior (LLNL, GPL-2.0).
// MPI ranks become threads, MPI_Barrier becomes std::sync::Barrier; the directory
// tree layout, item numbering/path reconstruction, phase-shifted item prefixes
// and trees (nstride), per-rank rates and the min/max/mean/sum aggregation across
// ranks mirror the C implementation.

use crate::smallfile::iso8601;
use log::{info, warn};
use serde_json::{Map, Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TEST_DIR: &str = "test-dir";

#[derive(Clone, Copy, PartialEq)]
enum Op {
    DirCreate,
    DirStat,
    DirRead,
    DirRename,
    DirRemove,
    FileCreate,
    FileStat,
    FileRead,
    FileRemove,
    TreeCreate,
    TreeRemove,
}

const OPS: [(Op, &str); 11] = [
    (Op::DirCreate, "Directory creation"),
    (Op::DirStat, "Directory stat"),
    (Op::DirRead, "Directory read"),
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
    /// number of threads (mdtest "tasks"/ranks)
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

    /// every thread will creat/stat/read/remove this many items
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

    /// files only at the leaf level of the tree
    #[arg(short = 'L', long)]
    pub leaf_only: bool,

    /// bytes to write to each file after it is created
    #[arg(short = 'w', long, default_value_t = 0)]
    pub write_bytes: u64,

    /// bytes to read from each file
    #[arg(short = 'e', long, default_value_t = 0)]
    pub read_bytes: u64,

    /// sync each file after writing
    #[arg(short = 'y', long)]
    pub sync_file: bool,

    /// call sync() after each phase (included in the timing)
    #[arg(short = 'Y', long)]
    pub sync_after_phase: bool,

    /// rank stride between phases for item access (1 avoids client caches)
    #[arg(short = 'N', long, default_value_t = 0)]
    pub nstride: u32,

    /// random access order for the stat phase
    #[arg(short = 'R', long)]
    pub random: bool,

    /// random seed for -R
    #[arg(long, default_value_t = 0)]
    pub random_seed: u64,

    /// stonewall timer in seconds (file creation phase only)
    #[arg(short = 'W', long, default_value_t = 0)]
    pub stonewall: u64,

    /// no barriers between phases
    #[arg(short = 'B', long)]
    pub no_barriers: bool,

    /// pre-iteration delay in seconds
    #[arg(short = 'p', long, default_value_t = 0)]
    pub pre_delay: u64,

    /// verify the data read back (requires --read-only and --read-bytes)
    #[arg(short = 'X', long)]
    pub verify_read: bool,

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
    // phase-shifted target rank: with nstride=1 each phase accesses items a
    // different thread created (the client-cache avoidance trick)
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
        let testdir = self.testdir(iteration, dir_iter);
        if self.unique_dir {
            testdir.join(format!("mdtest_tree.{target}.0"))
        } else {
            testdir.join("mdtest_tree.0")
        }
    }
    // port of mdtest's path reconstruction: the item number encodes its
    // directory (item_num / items_per_dir); parents are derived by walking up
    // with (parent-1)/branch_factor
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
            self.items_per_dir * (self.num_dirs_in_tree - self.branch_factor.pow(self.depth as u32) as u64)
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
    items_start: u64,
    items_done: u64,
    items_per_dir: u64,
    stone_wall_timer: u64,
    start: Instant,
}

struct RankCtx<'a> {
    p: &'a Params,
    rank: u32,
    barrier: &'a Barrier,
    rand_order: Option<Vec<u64>>,
}

impl RankCtx<'_> {
    // items created by phase-0 target, removed by phase-3 target, etc.
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
            unsafe { libc::sync() };
        }
        if self.p.barriers {
            self.barrier.wait();
        }
    }
    fn stop_items(&self) -> u64 {
        if self.p.directory_loops != 1 {
            self.p.items_per_dir
        } else {
            self.p.items
        }
    }

    // recursive item placement, port of create_remove_items(): directory number
    // dir_num holds items [dir_num*ipd, (dir_num+1)*ipd); children of dir d are
    // numbered d*branch+1 .. (d+1)*branch
    fn create_remove_items(
        &self,
        depth: i32,
        dirs: bool,
        create: bool,
        path: &Path,
        dir_num: u64,
        progress: &mut Progress,
    ) -> io::Result<()> {
        let ipd = progress.items_per_dir;
        if depth == 0 {
            if !self.p.leaf_only || self.p.depth == 0 {
                self.item_range(dirs, create, path, 0, progress)?;
            }
            if self.p.depth > 0 {
                self.create_remove_items(1, dirs, create, path, 1, progress)?;
            }
        } else if depth <= self.p.depth {
            for i in 0..self.p.branch_factor as u64 {
                let sub = path.join(format!("mdtest_tree.{}", dir_num + i));
                if !self.p.leaf_only || depth == self.p.depth {
                    self.item_range(dirs, create, &sub, (dir_num + i) * ipd, progress)?;
                }
                self.create_remove_items(
                    depth + 1,
                    dirs,
                    create,
                    &sub,
                    (dir_num + i) * self.p.branch_factor as u64 + 1,
                    progress,
                )?;
            }
        }
        Ok(())
    }

    fn item_range(&self, dirs: bool, create: bool, path: &Path, start: u64, progress: &mut Progress) -> io::Result<()> {
        let prefix = if create { self.prefix(0) } else { self.prefix(3) };
        let mut i = progress.items_start;
        while i < progress.items_per_dir {
            let item_num = start + i;
            let p = if dirs {
                path.join(format!("dir.{prefix}{item_num}"))
            } else {
                path.join(format!("file.{prefix}{item_num}"))
            };
            if dirs {
                let r = if create { fs::create_dir(&p) } else { fs::remove_dir(&p) };
                warn_item(&r, "directory", &p);
            } else if create {
                match OpenOptions::new().write(true).create(true).mode(0o644).open(&p) {
                    Ok(mut f) => {
                        if self.p.write_bytes > 0 {
                            let buf = content_pattern(
                                self.p.random_seed,
                                self.p.shifted(self.rank, 0),
                                item_num,
                                self.p.write_bytes as usize,
                            );
                            if let Err(e) = f.write_all(&buf) {
                                warn!("unable to write {}: {e}", p.display());
                            }
                        }
                        if self.p.sync_file {
                            if let Err(e) = crate::smallfile::fsync_raw(&f) {
                                warn!("unable to fsync {}: {e}", p.display());
                            }
                        }
                    }
                    Err(e) => warn!("unable to create {}: {e}", p.display()),
                }
            } else {
                warn_item(&fs::remove_file(&p), "file", &p);
            }
            i += 1;
            progress.items_done = i;
            if progress.stone_wall_timer > 0
                && progress.start.elapsed().as_secs_f64() > progress.stone_wall_timer as f64
            {
                return Ok(());
            }
        }
        progress.items_done = progress.items_per_dir;
        Ok(())
    }

    fn stat_phase(&self, dirs: bool, tree_root: &Path, base: &str) -> io::Result<()> {
        let stop = self.stop_items();
        let offset = self.p.leaf_offset();
        let prefix = self.prefix(1);
        for i in 0..stop {
            let item_num = match &self.rand_order {
                Some(arr) => arr[i as usize],
                None => i,
            } + offset;
            let rel = self.p.item_path(base, dirs, &prefix, item_num);
            if let Err(e) = fs::metadata(tree_root.join(rel)) {
                warn!("unable to stat item {item_num}: {e}");
            }
        }
        Ok(())
    }

    fn read_phase(&self, tree_root: &Path, base: &str) -> io::Result<u64> {
        let stop = self.stop_items();
        let offset = self.p.leaf_offset();
        let prefix = self.prefix(2);
        let mut errors = 0u64;
        let mut scratch = vec![0u8; self.p.read_bytes as usize];
        for i in 0..stop {
            let item_num = i + offset;
            let rel = self.p.item_path(base, false, &prefix, item_num);
            match File::open(tree_root.join(rel)) {
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
                                    errors += 1;
                                }
                            }
                            Err(e) => warn!("unable to read item {item_num}: {e}"),
                        }
                    }
                }
                Err(e) => warn!("unable to open item {item_num}: {e}"),
            }
        }
        Ok(errors)
    }

    // rotating rename chain exactly like rename_dir_test(): item 0 renames to
    // *-XX, each item takes its predecessor's name, and the *last* rename moves
    // *-XX onto item n-2's original name so every original name exists again
    fn rename_phase(&self, dirs: bool, tree_root: &Path, base: &str) -> io::Result<()> {
        let stop = self.stop_items();
        if stop == 1 {
            return Ok(());
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
                warn!("unable to rename {}: {e}", src.display());
            }
            if i != stop - 1 {
                last = Some(path);
            }
        }
        Ok(())
    }
}

fn suffixed(p: &Path, s: &str) -> PathBuf {
    let mut o = p.as_os_str().to_os_string();
    o.push(s);
    o.into()
}

// mdtest warns and keeps going when a single item op fails; a dead thread would
// corrupt the phase barriers for everyone
fn warn_item(r: &io::Result<()>, kind: &str, p: &Path) {
    if let Err(e) = r {
        warn!("unable to process {kind} {}: {e}", p.display());
    }
}

// deterministic per-(seed, writer-rank, item) content; mdtest's dataPacketType
// byte layouts are not replicated, but verify-read is self-consistent
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

// skeleton of mdtest_tree.<n> directories, port of create_remove_directory_tree();
// removal happens children-first inside the recursion
fn create_remove_tree(p: &Params, create: bool, parent: &Path, depth: i32, dir_num: u64) -> io::Result<()> {
    if depth == 0 {
        let root = parent.join("mdtest_tree.0");
        if create {
            fs::create_dir(&root)?;
        }
        create_remove_tree(p, create, &root, 1, 1)?;
        if !create {
            let _ = fs::remove_dir(&root);
        }
        Ok(())
    } else if depth <= p.depth {
        for i in 0..p.branch_factor as u64 {
            let sub = parent.join(format!("mdtest_tree.{}", dir_num + i));
            if create {
                fs::create_dir(&sub)?;
            }
            create_remove_tree(p, create, &sub, depth + 1, p.branch_factor as u64 * (dir_num + i) + 1)?;
            if !create {
                let _ = fs::remove_dir(&sub);
            }
        }
        Ok(())
    } else {
        Ok(())
    }
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let threads = args.threads.max(1);
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
    if args.items > 0 && args.items_per_dir > 0 {
        if args.unique_dir {
            return Err("specify either items or items per directory, not both".into());
        }
        if args.items % args.items_per_dir != 0 {
            return Err("items must be a multiple of items per directory".into());
        }
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
    // port of the tree/items math in mdtest_run()
    let num_dirs_in_tree = if args.depth <= 0 || args.branch_factor < 1 {
        1
    } else if args.branch_factor == 1 {
        args.depth as u64 + 1
    } else {
        (args.branch_factor.pow(args.depth as u32 + 1) - 1) as u64 / (args.branch_factor as u64 - 1)
    };
    let (items, items_per_dir) = if args.items_per_dir > 0 {
        let items = if args.items == 0 {
            if args.leaf_only {
                args.items_per_dir * args.branch_factor.pow(args.depth.max(0) as u32) as u64
            } else {
                args.items_per_dir * num_dirs_in_tree
            }
        } else {
            args.items
        };
        (items, args.items_per_dir)
    } else {
        let leaf_dirs = args.branch_factor.pow(args.depth.max(0) as u32) as u64;
        let ipd = if args.leaf_only {
            if args.branch_factor <= 1 {
                args.items
            } else {
                args.items / leaf_dirs
            }
        } else {
            args.items / num_dirs_in_tree
        };
        let items = if args.leaf_only {
            ipd * leaf_dirs
        } else {
            ipd * num_dirs_in_tree
        };
        (items, ipd)
    };
    if items_per_dir == 0 {
        return Err("computed zero items per directory; set --items or --items-per-dir".into());
    }
    // computed from the user-provided --items BEFORE the tree math derives it,
    // exactly the ordering in mdtest_run()
    let directory_loops = if args.items > 0 && args.items_per_dir > 0 && !args.unique_dir {
        (args.items / args.items_per_dir) as u32
    } else {
        1
    };
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
        iterations: args.iterations,
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
    let all_stats: Arc<Mutex<Vec<Vec<[OpStat; 11]>>>> = Arc::new(Mutex::new(
        (0..p.iterations)
            .map(|_| (0..threads).map(|_| [OpStat::default(); 11]).collect())
            .collect(),
    ));
    let total_errors: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(vec![0; threads as usize]));
    thread::scope(|s| {
        let mut joins = Vec::new();
        for rank in 0..threads {
            let barrier = barrier.clone();
            let stats = all_stats.clone();
            let errors = total_errors.clone();
            let pp = &p;
            joins.push(s.spawn(move || {
                if let Err(e) = run_rank(pp, rank, &barrier, &stats, &errors) {
                    warn!("thread {rank} failed: {e}");
                }
            }));
        }
        for j in joins {
            j.join().unwrap();
        }
    });
    let all_stats = Arc::try_unwrap(all_stats).unwrap().into_inner().unwrap();
    let total_errors = Arc::try_unwrap(total_errors).unwrap().into_inner().unwrap();
    let errors: u64 = total_errors.iter().sum();
    if errors > 0 {
        warn!("verification errors on read: {errors}; take performance values with care");
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
                        "time": st[oi].time, "timeBeforeBarrier": st[oi].time_before_barrier,
                        "rate": st[oi].rate, "items": st[oi].items,
                    }),
                );
            }
            if !ranks.is_empty() {
                ops.insert(name.to_string(), json!({ "rank": ranks }));
            }
        }
        iterations_json.push(json!({ "iteration": it + 1, "results": ops }));
    }
    // summary: per-rank rate averaged over iterations, then min/max/mean/sum
    // across ranks (mdtest's summarize_results semantics)
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
    let report = json!({
        "suite": "mdtest",
        "host": host,
        "date": iso8601(SystemTime::now()),
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
        "status": if errors > 0 { "failed" } else { "ok" },
        "verificationErrors": errors,
        "iterations": iterations_json,
        "summary": summary,
    });
    if let Some(path) = &args.json {
        if let Err(e) = fs::write(path, serde_json::to_vec_pretty(&report).unwrap()) {
            warn!("failed to write {}: {e}", path.display());
        }
    }
    Ok(report)
}

fn run_rank(
    p: &Params,
    rank: u32,
    barrier: &Barrier,
    stats: &Arc<Mutex<Vec<Vec<[OpStat; 11]>>>>,
    errors: &Arc<Mutex<Vec<u64>>>,
) -> io::Result<()> {
    let rand_order = if p.random {
        Some(shuffled(p.items, p.random_seed + rank as u64))
    } else {
        None
    };
    let ctx = RankCtx {
        p,
        rank,
        barrier,
        rand_order,
    };
    let mut verification_errors = 0u64;
    for iteration in 0..p.iterations as usize {
        let mut res = [OpStat::default(); 11];
        let mut progress = Progress {
            items_start: 0,
            items_done: 0,
            items_per_dir: p.items_per_dir,
            stone_wall_timer: 0,
            start: Instant::now(),
        };
        if p.create_only {
            for dir_iter in 0..p.directory_loops as usize {
                let _ = fs::create_dir(p.testdir(iteration, dir_iter)); // EEXIST tolerated
            }
            barrier.wait();
            let t0 = Instant::now();
            for dir_iter in 0..p.directory_loops as usize {
                let testdir = p.testdir(iteration, dir_iter);
                if p.unique_dir {
                    // each thread builds its own mdtest_tree.<rank> skeleton
                    let base = testdir.join(format!("mdtest_tree.{rank}"));
                    let _ = fs::create_dir(&base);
                    create_remove_tree(p, true, &base, 0, 0)?;
                } else if rank == 0 {
                    create_remove_tree(p, true, &testdir, 0, 0)?;
                }
            }
            barrier.wait();
            let elapsed = t0.elapsed().as_secs_f64();
            if p.unique_dir || rank == 0 {
                let i = op_index(Op::TreeCreate);
                res[i].time = elapsed;
                res[i].time_before_barrier = elapsed;
                res[i].rate = p.num_dirs_in_tree as f64 / elapsed;
                res[i].items = p.num_dirs_in_tree;
            }
        }
        // phase-shifted trees/prefixes: create(0) stat(1) read(2) remove(3)
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
                for dir_iter in 0..p.directory_loops as usize {
                    progress.items_start = 0;
                    ctx.create_remove_items(0, true, true, &root(0, dir_iter), 0, &mut progress)?;
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::DirCreate, p.items, t0.elapsed().as_secs_f64(), tbb);
            }
            if p.stat_only {
                ctx.phase_prepare();
                let t0 = Instant::now();
                for dir_iter in 0..p.directory_loops as usize {
                    ctx.stat_phase(true, &root(1, dir_iter), &base(1))?;
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::DirStat, p.items, t0.elapsed().as_secs_f64(), tbb);
            }
            if p.read_only {
                // mdtest records the phase but performs no operations for dirs
                ctx.phase_prepare();
                let t0 = Instant::now();
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::DirRead, p.items, t0.elapsed().as_secs_f64(), tbb);
            }
            if p.rename_dirs && p.items > 1 {
                ctx.phase_prepare();
                let t0 = Instant::now();
                for dir_iter in 0..p.directory_loops as usize {
                    ctx.rename_phase(true, &root(1, dir_iter), &base(1))?;
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::DirRename, p.items, t0.elapsed().as_secs_f64(), tbb);
            }
            if p.remove_only {
                ctx.phase_prepare();
                let t0 = Instant::now();
                for dir_iter in 0..p.directory_loops as usize {
                    progress.items_start = 0;
                    ctx.create_remove_items(0, true, false, &root(3, dir_iter), 0, &mut progress)?;
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
            if p.create_only {
                ctx.phase_prepare();
                let t0 = Instant::now();
                progress.stone_wall_timer = p.stonewall;
                progress.start = Instant::now();
                for dir_iter in 0..p.directory_loops as usize {
                    progress.items_start = if dir_iter == 0 { 0 } else { progress.items_done };
                    ctx.create_remove_items(0, false, true, &root(0, dir_iter), 0, &mut progress)?;
                }
                progress.stone_wall_timer = 0;
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                let done = progress.items_done.min(p.items);
                set_stat(&mut res, Op::FileCreate, done, t0.elapsed().as_secs_f64(), tbb);
                if done < p.items {
                    info!("thread {rank} hit stonewall after {done} of {} items", p.items);
                }
            }
            if p.stat_only {
                ctx.phase_prepare();
                let t0 = Instant::now();
                for dir_iter in 0..p.directory_loops as usize {
                    ctx.stat_phase(false, &root(1, dir_iter), &base(1))?;
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::FileStat, p.items, t0.elapsed().as_secs_f64(), tbb);
            }
            if p.read_only {
                ctx.phase_prepare();
                let t0 = Instant::now();
                for dir_iter in 0..p.directory_loops as usize {
                    verification_errors += ctx.read_phase(&root(2, dir_iter), &base(2))?;
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::FileRead, p.items, t0.elapsed().as_secs_f64(), tbb);
            }
            if p.remove_only {
                ctx.phase_prepare();
                let t0 = Instant::now();
                for dir_iter in 0..p.directory_loops as usize {
                    progress.items_start = 0;
                    ctx.create_remove_items(0, false, false, &root(3, dir_iter), 0, &mut progress)?;
                }
                let tbb = t0.elapsed().as_secs_f64();
                ctx.phase_end();
                set_stat(&mut res, Op::FileRemove, p.items, t0.elapsed().as_secs_f64(), tbb);
            }
        }
        barrier.wait();
        if p.remove_only {
            let t0 = Instant::now();
            for dir_iter in 0..p.directory_loops as usize {
                let testdir = p.testdir(iteration, dir_iter);
                if p.unique_dir {
                    let base = testdir.join(format!("mdtest_tree.{rank}"));
                    create_remove_tree(p, false, &base, 0, 0)?;
                    let _ = fs::remove_dir(&base);
                } else if rank == 0 {
                    create_remove_tree(p, false, &testdir, 0, 0)?;
                }
                if rank == 0 {
                    let _ = fs::remove_dir(&testdir);
                }
            }
            barrier.wait();
            let elapsed = t0.elapsed().as_secs_f64();
            if p.unique_dir || rank == 0 {
                let i = op_index(Op::TreeRemove);
                res[i].time = elapsed;
                res[i].time_before_barrier = elapsed;
                res[i].rate = p.num_dirs_in_tree as f64 / elapsed;
                res[i].items = p.num_dirs_in_tree;
            }
        }
        stats.lock().unwrap()[iteration][rank as usize] = res;
    }
    errors.lock().unwrap()[rank as usize] = verification_errors;
    Ok(())
}

fn set_stat(res: &mut [OpStat; 11], op: Op, items: u64, t: f64, tbb: f64) {
    let i = op_index(op);
    res[i].time = t;
    res[i].time_before_barrier = tbb;
    res[i].items = items;
    res[i].rate = if t > 0.0 { items as f64 / t } else { 0.0 };
}
