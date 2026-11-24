// `fsrand` suite: deterministic model-checked filesystem verification workloads
// (`tree` directory shadow-oracle, `rw` single-file pwrite+fsync shadow-buffer,
// `concurrent` multi-thread atomic-rename + reader-consistency stress, or `all`).

use crate::common::{CaseEntry, SplitMix64, fsync_raw, host_name, iso8601, remove_dir_all_bounded};
use log::{info, warn};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

#[derive(clap::Args)]
pub struct Args {
    /// workload mode: `tree` | `rw` | `concurrent` | `all`
    #[arg(long, default_value = "tree")]
    pub mode: String,

    /// number of random operations (`tree` / `rw` modes)
    #[arg(short = 'n', long, default_value_t = 1000)]
    pub ops: u64,

    /// deterministic RNG seed
    #[arg(short = 's', long, default_value_t = 9)]
    pub seed: u64,

    /// `tree` mode: compare mount tree against local oracle every N ops
    #[arg(long, default_value_t = 25)]
    pub check_every: u64,

    /// `rw` mode: file size in bytes (default 4 MiB)
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    pub file_size: usize,

    /// `rw` mode: target file name inside working directory
    #[arg(long, default_value = "rw.bin")]
    pub file: String,

    /// `concurrent` mode: worker thread count
    #[arg(long, default_value_t = 4)]
    pub workers: usize,

    /// `concurrent` mode: files per worker
    #[arg(long, default_value_t = 64)]
    pub files: usize,

    /// working subdirectory under mountpoint (`.` runs directly in mountpoint root)
    #[arg(long, default_value = ".")]
    pub work_dir: String,

    /// clean up created files when finished (defaults to `false` when `work_dir` is `.`, `true` otherwise)
    #[arg(long)]
    pub cleanup: Option<bool>,

    /// verbose logging
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// write the enriched JSON result to this file
    #[arg(long)]
    pub json: Option<PathBuf>,
}

struct Rng(SplitMix64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(SplitMix64::new(seed.wrapping_add(0x9E37_79B9_7F4A_7C15) | 1))
    }
    fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }
    fn range(&mut self, upper: usize) -> usize {
        if upper == 0 {
            0
        } else {
            (self.next_u64() as usize) % upper
        }
    }
    fn fill_bytes(&mut self, buf: &mut [u8]) {
        self.0.fill_bytes(buf);
    }
}

fn sha256_file(path: &Path) -> Result<(u64, String), String> {
    let mut f = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 65536];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        h.update(&buf[..n]);
    }
    Ok((total, hex::encode(h.finalize())))
}

#[derive(Debug, PartialEq, Eq)]
enum SnapEntry {
    Dir {
        mode: u32,
    },
    File {
        size: u64,
        mode: u32,
        nlink: u64,
        sha256: String,
    },
    Symlink {
        target: String,
    },
}

struct TreeSnap {
    entries: BTreeMap<String, SnapEntry>,
    link_groups: Vec<Vec<String>>,
}

fn walk_snapshot(root: &Path) -> Result<TreeSnap, String> {
    let mut entries = BTreeMap::new();
    let mut ino_map: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    walk_rec(root, root, &mut entries, &mut ino_map)?;
    let mut link_groups: Vec<Vec<String>> = ino_map
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|mut g| {
            g.sort();
            g
        })
        .collect();
    link_groups.sort();
    Ok(TreeSnap { entries, link_groups })
}

fn walk_rec(
    root: &Path,
    cur: &Path,
    out: &mut BTreeMap<String, SnapEntry>,
    ino_map: &mut BTreeMap<u64, Vec<String>>,
) -> Result<(), String> {
    let mut dir_entries: Vec<_> = fs::read_dir(cur)
        .map_err(|e| format!("readdir {}: {e}", cur.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("readdir entry {}: {e}", cur.display()))?;
    dir_entries.sort_by_key(|e| e.file_name());
    for ent in dir_entries {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name == "__payload__" || name.starts_with(".fstest-") || name == ".sim" {
            continue;
        }
        let p = ent.path();
        let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
        let meta = fs::symlink_metadata(&p).map_err(|e| format!("lstat {}: {e}", p.display()))?;
        let ft = meta.file_type();
        if ft.is_dir() {
            out.insert(
                format!("{rel}/"),
                SnapEntry::Dir {
                    mode: meta.mode() & 0o777,
                },
            );
            walk_rec(root, &p, out, ino_map)?;
        } else if ft.is_symlink() {
            let t = fs::read_link(&p)
                .map_err(|e| format!("readlink {}: {e}", p.display()))?
                .to_string_lossy()
                .into_owned();
            out.insert(rel, SnapEntry::Symlink { target: t });
        } else if ft.is_file() {
            let (size, sha256) = sha256_file(&p)?;
            if meta.nlink() > 1 {
                ino_map.entry(meta.ino()).or_default().push(rel.clone());
            }
            out.insert(
                rel,
                SnapEntry::File {
                    size,
                    mode: meta.mode() & 0o777,
                    nlink: meta.nlink(),
                    sha256,
                },
            );
        }
    }
    Ok(())
}

fn diff_snapshots(oracle: &TreeSnap, mount: &TreeSnap) -> Option<String> {
    for (k, ov) in &oracle.entries {
        match mount.entries.get(k) {
            None => return Some(format!("missing on mount: {k} (oracle={ov:?})")),
            Some(mv) if mv != ov => {
                return Some(format!("mismatch at {k}: mount={mv:?} vs oracle={ov:?}"));
            }
            Some(_) => {}
        }
    }
    for (k, mv) in &mount.entries {
        if !oracle.entries.contains_key(k) {
            return Some(format!("unexpected extra entry on mount: {k} ({mv:?})"));
        }
    }
    if oracle.link_groups != mount.link_groups {
        return Some(format!(
            "hardlink inode group mismatch: mount={:?} vs oracle={:?}",
            mount.link_groups, oracle.link_groups
        ));
    }
    None
}

fn random_payload(rng: &mut Rng) -> Vec<u8> {
    let tier = rng.range(100);
    let len = if tier < 65 {
        rng.range(512) + 1
    } else if tier < 90 {
        2048 + rng.range(14 * 1024)
    } else {
        16 * 1024 + rng.range(48 * 1024)
    };
    let mut buf = vec![0u8; len];
    rng.fill_bytes(&mut buf);
    buf
}

fn join_rel(dir_rel: &str, leaf: &str) -> String {
    if dir_rel == "." {
        leaf.to_string()
    } else {
        format!("{dir_rel}/{leaf}")
    }
}

fn run_tree_mode(target: &Path, args: &Args) -> Result<(bool, Vec<String>, Value), String> {
    let oracle = std::env::temp_dir().join(format!("fstest-fsrand-oracle-{}-{}", std::process::id(), args.seed));
    let _ = fs::remove_dir_all(&oracle);
    fs::create_dir_all(&oracle).map_err(|e| e.to_string())?;

    let mut rng = Rng::new(args.seed);
    let names: Vec<String> = (0..48).map(|i| format!("f{i:03}")).collect();
    let mut dirs: Vec<String> = vec![".".into()];
    let ops = [
        "mkdir", "write", "pwrite", "append", "rename", "hardlink", "symlink", "chmod", "fsync", "unlink", "rmdir",
        "truncate",
    ];
    let mut failures = Vec::new();
    let mut checks_run = 0u64;

    for step in 0..args.ops {
        let op = ops[rng.range(ops.len())];
        let dir_rel = dirs[rng.range(dirs.len())].clone();
        let leaf = names[rng.range(names.len())].clone();
        let rel = join_rel(&dir_rel, &leaf);
        let o_src = oracle.join(&rel);
        let m_src = target.join(&rel);

        match op {
            "mkdir" => {
                let depth = dir_rel.matches('/').count();
                let parent = if depth < 2 { &dir_rel } else { "." };
                let d_leaf = format!("d{:02}", rng.range(16));
                let d_rel = join_rel(parent, &d_leaf);
                let r1 = fs::create_dir(oracle.join(&d_rel));
                let r2 = fs::create_dir(target.join(&d_rel));
                if r1.is_ok() != r2.is_ok() {
                    failures.push(format!("step {step} mkdir({d_rel}): oracle={r1:?} mount={r2:?}"));
                    break;
                }
                if oracle.join(&d_rel).is_dir() && !dirs.contains(&d_rel) {
                    dirs.push(d_rel);
                }
            }
            "write" => {
                if o_src.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) {
                    continue;
                }
                if o_src
                    .symlink_metadata()
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    let _ = fs::remove_file(&o_src);
                    let _ = fs::remove_file(&m_src);
                }
                let data = random_payload(&mut rng);
                let r1 = fs::write(&o_src, &data);
                let r2 = fs::write(&m_src, &data);
                if r1.is_ok() != r2.is_ok() {
                    failures.push(format!("step {step} write({rel}): oracle={r1:?} mount={r2:?}"));
                    break;
                }
            }
            "pwrite" => {
                if o_src.symlink_metadata().map(|m| m.is_file()).unwrap_or(false) {
                    let off = rng.range(32 * 1024) as u64;
                    let plen = rng.range(8192) + 1;
                    let mut patch = vec![0u8; plen];
                    rng.fill_bytes(&mut patch);
                    let w1 = OpenOptions::new()
                        .write(true)
                        .open(&o_src)
                        .and_then(|f| f.write_all_at(&patch, off));
                    let w2 = OpenOptions::new()
                        .write(true)
                        .open(&m_src)
                        .and_then(|f| f.write_all_at(&patch, off));
                    if w1.is_ok() != w2.is_ok() {
                        failures.push(format!(
                            "step {step} pwrite({rel}@{off}+{plen}): oracle={w1:?} mount={w2:?}"
                        ));
                        break;
                    }
                }
            }
            "append" => {
                if o_src.symlink_metadata().map(|m| m.is_file()).unwrap_or(false) {
                    let alen = rng.range(4096) + 1;
                    let mut data = vec![0u8; alen];
                    rng.fill_bytes(&mut data);
                    let a1 = OpenOptions::new()
                        .append(true)
                        .open(&o_src)
                        .and_then(|mut f| f.write_all(&data));
                    let a2 = OpenOptions::new()
                        .append(true)
                        .open(&m_src)
                        .and_then(|mut f| f.write_all(&data));
                    if a1.is_ok() != a2.is_ok() {
                        failures.push(format!("step {step} append({rel}): oracle={a1:?} mount={a2:?}"));
                        break;
                    }
                }
            }
            "rename" => {
                if o_src.symlink_metadata().map(|m| !m.is_dir()).unwrap_or(false) {
                    let dst_dir = dirs[rng.range(dirs.len())].clone();
                    let dst_leaf = &names[rng.range(names.len())];
                    let dst_rel = join_rel(&dst_dir, dst_leaf);
                    let o_dst = oracle.join(&dst_rel);
                    let m_dst = target.join(&dst_rel);
                    if !o_dst.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) {
                        let r1 = fs::rename(&o_src, &o_dst);
                        let r2 = fs::rename(&m_src, &m_dst);
                        if r1.is_ok() != r2.is_ok() {
                            failures.push(format!(
                                "step {step} rename({rel}->{dst_rel}): oracle={r1:?} mount={r2:?}"
                            ));
                            break;
                        }
                    }
                }
            }
            "hardlink" => {
                if o_src.symlink_metadata().map(|m| m.is_file()).unwrap_or(false) {
                    let dst_dir = dirs[rng.range(dirs.len())].clone();
                    let dst_leaf = &names[rng.range(names.len())];
                    let dst_rel = join_rel(&dst_dir, dst_leaf);
                    let o_dst = oracle.join(&dst_rel);
                    let m_dst = target.join(&dst_rel);
                    if o_dst.symlink_metadata().is_err() {
                        let l1 = fs::hard_link(&o_src, &o_dst);
                        let l2 = fs::hard_link(&m_src, &m_dst);
                        // Tolerate mounts that explicitly disable hardlinks (`EOPNOTSUPP`/`EMLINK`/`EXDEV`),
                        // otherwise require exact parity.
                        let unsupported =
                            l2.as_ref().err().and_then(|e| e.raw_os_error()).is_some_and(|c| {
                                matches!(c, libc::EOPNOTSUPP | libc::ENOSYS | libc::EXDEV | libc::EPERM)
                            });
                        if unsupported {
                            let _ = fs::remove_file(&o_dst);
                        } else if l1.is_ok() != l2.is_ok() {
                            failures.push(format!(
                                "step {step} hardlink({rel}->{dst_rel}): oracle={l1:?} mount={l2:?}"
                            ));
                            break;
                        }
                    }
                }
            }
            "symlink" => {
                if o_src.symlink_metadata().is_err() {
                    let sym_target = format!("target-{step}");
                    let s1 = std::os::unix::fs::symlink(&sym_target, &o_src);
                    let s2 = std::os::unix::fs::symlink(&sym_target, &m_src);
                    if s1.is_ok() != s2.is_ok() {
                        failures.push(format!("step {step} symlink({rel}): oracle={s1:?} mount={s2:?}"));
                        break;
                    }
                }
            }
            "chmod" => {
                if o_src.symlink_metadata().map(|m| m.is_file()).unwrap_or(false) {
                    let mode = [0o644, 0o600, 0o755][rng.range(3)];
                    let c1 = fs::set_permissions(&o_src, fs::Permissions::from_mode(mode));
                    let c2 = fs::set_permissions(&m_src, fs::Permissions::from_mode(mode));
                    if c1.is_ok() != c2.is_ok() {
                        failures.push(format!(
                            "step {step} chmod({rel}, 0{mode:o}): oracle={c1:?} mount={c2:?}"
                        ));
                        break;
                    }
                }
            }
            "fsync" => {
                if m_src.symlink_metadata().map(|m| m.is_file()).unwrap_or(false) {
                    let res = OpenOptions::new().write(true).open(&m_src).and_then(|f| fsync_raw(&f));
                    if let Err(e) = res {
                        failures.push(format!("step {step} fsync({rel}) failed: {e}"));
                        break;
                    }
                }
            }
            "unlink" => {
                if o_src
                    .symlink_metadata()
                    .map(|m| m.is_file() || m.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    let u1 = fs::remove_file(&o_src);
                    let u2 = fs::remove_file(&m_src);
                    if u1.is_ok() != u2.is_ok() {
                        failures.push(format!("step {step} unlink({rel}): oracle={u1:?} mount={u2:?}"));
                        break;
                    }
                }
            }
            "rmdir" => {
                if dirs.len() > 1 {
                    let idx = 1 + rng.range(dirs.len() - 1);
                    let d = dirs[idx].clone();
                    let r1 = fs::remove_dir(oracle.join(&d));
                    let r2 = fs::remove_dir(target.join(&d));
                    if r1.is_ok() != r2.is_ok() {
                        failures.push(format!("step {step} rmdir({d}): oracle={r1:?} mount={r2:?}"));
                        break;
                    }
                    if r1.is_ok() {
                        dirs.remove(idx);
                    }
                }
            }
            "truncate" => {
                if o_src.symlink_metadata().map(|m| m.is_file()).unwrap_or(false) {
                    let sz = if rng.range(4) == 0 {
                        0
                    } else {
                        rng.range(64 * 1024) as u64
                    };
                    let t1 = OpenOptions::new().write(true).open(&o_src).and_then(|f| f.set_len(sz));
                    let t2 = OpenOptions::new().write(true).open(&m_src).and_then(|f| f.set_len(sz));
                    if t1.is_ok() != t2.is_ok() {
                        failures.push(format!("step {step} truncate({rel}, {sz}): oracle={t1:?} mount={t2:?}"));
                        break;
                    }
                }
            }
            _ => {}
        }

        if args.check_every > 0 && (step + 1) % args.check_every == 0 {
            checks_run += 1;
            let o_snap = walk_snapshot(&oracle)?;
            let m_snap = walk_snapshot(target)?;
            if let Some(diff) = diff_snapshots(&o_snap, &m_snap) {
                failures.push(format!("divergence at op {}: {diff}", step + 1));
                break;
            }
        }
    }

    let mut final_entries = 0usize;
    if failures.is_empty() {
        checks_run += 1;
        let o_snap = walk_snapshot(&oracle)?;
        let m_snap = walk_snapshot(target)?;
        final_entries = m_snap.entries.len();
        if let Some(diff) = diff_snapshots(&o_snap, &m_snap) {
            failures.push(format!("final divergence: {diff}"));
        }
    }
    let _ = fs::remove_dir_all(&oracle);

    Ok((
        failures.is_empty(),
        failures,
        json!({
            "mode": "tree",
            "ops": args.ops,
            "checksRun": checks_run,
            "finalEntries": final_entries,
        }),
    ))
}

fn run_rw_mode(target: &Path, args: &Args) -> Result<(bool, Vec<String>, Value), String> {
    let size = args.file_size.max(4096);
    let max_io = (64 * 1024).min(size);
    let path = target.join(&args.file);
    let mut rng = Rng::new(args.seed);
    let mut model = vec![0u8; size];
    let mut failures = Vec::new();

    {
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| format!("create {}: {e}", path.display()))?;
        f.write_all(&model).map_err(|e| format!("initial write: {e}"))?;
        fsync_raw(&f).map_err(|e| format!("initial fsync: {e}"))?;

        let mut chunk = vec![0u8; max_io];
        let mut verify_buf = vec![0u8; max_io];
        for step in 0..args.ops {
            let n = if step % 8 == 0 {
                rng.range(max_io) + 1
            } else {
                rng.range(4096.min(max_io)) + 1
            };
            let off = rng.range(size - n + 1);
            rng.fill_bytes(&mut chunk[..n]);
            f.seek(SeekFrom::Start(off as u64))
                .map_err(|e| format!("seek step {step}: {e}"))?;
            f.write_all(&chunk[..n])
                .map_err(|e| format!("write step {step}: {e}"))?;
            if let Err(e) = fsync_raw(&f) {
                failures.push(format!("fsync step {step} on {}: {e}", path.display()));
                break;
            }
            model[off..off + n].copy_from_slice(&chunk[..n]);

            if (step + 1) % 64 == 0 {
                drop(f);
                f = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(|e| format!("reopen step {step} {}: {e}", path.display()))?;
                let vlen = max_io.min(size);
                let voff = rng.range(size - vlen + 1);
                if let Err(e) = f.read_exact_at(&mut verify_buf[..vlen], voff as u64) {
                    failures.push(format!("pread step {step}@{voff}: {e}"));
                    break;
                }
                if verify_buf[..vlen] != model[voff..voff + vlen] {
                    failures.push(format!("intermediate readback mismatch at step {step} offset {voff}"));
                    break;
                }
            }
        }
    }

    if failures.is_empty() {
        let got = fs::read(&path).map_err(|e| format!("readback {}: {e}", path.display()))?;
        if got.len() != model.len() {
            failures.push(format!(
                "size mismatch on {}: got {} bytes, expected {} bytes",
                path.display(),
                got.len(),
                model.len()
            ));
        } else if let Some(off) = got.iter().zip(model.iter()).position(|(a, b)| a != b) {
            failures.push(format!(
                "byte mismatch on {} at offset {off}: got 0x{:02x}, expected 0x{:02x}",
                path.display(),
                got[off],
                model[off]
            ));
        }
    }

    Ok((
        failures.is_empty(),
        failures,
        json!({
            "mode": "rw",
            "file": args.file,
            "fileSize": size,
            "ops": args.ops,
        }),
    ))
}

fn make_framed_payload(tid: usize, seq: usize, body_repeats: usize) -> Vec<u8> {
    let body = format!("slot-t{tid}-s{seq}:").repeat(body_repeats.max(1));
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("{digest}\n{body}").into_bytes()
}

fn verify_framed_payload(raw: &[u8]) -> Result<(), String> {
    let Some(nl) = raw.iter().position(|&b| b == b'\n') else {
        return Err(format!("torn payload without header newline ({}B)", raw.len()));
    };
    let want_hex = std::str::from_utf8(&raw[..nl]).map_err(|e| format!("corrupt header: {e}"))?;
    let mut hasher = Sha256::new();
    hasher.update(&raw[nl + 1..]);
    let got_hex = hex::encode(hasher.finalize());
    if want_hex != got_hex {
        return Err(format!(
            "torn/corrupt atomic-rename read: sha256 header={want_hex} body={got_hex} ({}B)",
            raw.len()
        ));
    }
    Ok(())
}

fn run_concurrent_mode(target: &Path, args: &Args) -> Result<(bool, Vec<String>, Value), String> {
    let workers = args.workers.max(1);
    let files = args.files.max(1);
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());

    let hot_slots: Vec<PathBuf> = (0..workers.min(4))
        .map(|k| target.join(format!(".fstest-hot-slot-{k}")))
        .collect();
    for slot in &hot_slots {
        let init = make_framed_payload(0, 0, 32);
        fs::write(slot, &init).map_err(|e| format!("seed {}: {e}", slot.display()))?;
    }

    let writers_done = AtomicBool::new(false);

    std::thread::scope(|s| {
        for tid in 0..workers {
            let errors = &errors;
            let hot_slots = &hot_slots;
            let writers_done = &writers_done;
            s.spawn(move || {
                let mut local_errs = Vec::new();
                for n in 0..files {
                    let part = target.join(format!("stress-{tid}-{n}.part"));
                    let final_path = target.join(format!("stress-{tid}-{n}"));
                    let repeats = 64 + ((tid * 17 + n * 31) % 256);
                    let data = format!("w{tid}f{n}-").repeat(repeats);
                    let write_res = File::create(&part).and_then(|mut f| {
                        f.write_all(data.as_bytes())?;
                        f.flush()?;
                        fsync_raw(&f)
                    });
                    if let Err(e) = write_res {
                        local_errs.push(format!("create/fsync {}: {e}", part.display()));
                        break;
                    }
                    match fs::read(&part) {
                        Ok(got) if got == data.as_bytes() => {}
                        Ok(got) => {
                            local_errs.push(format!(
                                "pre-rename readback mismatch {} (got {}B vs {}B)",
                                part.display(),
                                got.len(),
                                data.len()
                            ));
                        }
                        Err(e) => local_errs.push(format!("read {}: {e}", part.display())),
                    }
                    if let Err(e) = fs::rename(&part, &final_path) {
                        local_errs.push(format!("rename {} -> {}: {e}", part.display(), final_path.display()));
                        continue;
                    }
                    match fs::read(&final_path) {
                        Ok(got) if got == data.as_bytes() => {}
                        Ok(got) => {
                            local_errs.push(format!(
                                "post-rename readback mismatch {} (got {}B vs {}B)",
                                final_path.display(),
                                got.len(),
                                data.len()
                            ));
                        }
                        Err(e) => local_errs.push(format!("read {}: {e}", final_path.display())),
                    }

                    // Also exercise atomic rename-over-existing on shared hot slots.
                    let slot_idx = (tid + n) % hot_slots.len();
                    let slot_tmp = target.join(format!(".fstest-hot-tmp-{tid}-{n}"));
                    let framed = make_framed_payload(tid, n + 1, 48 + (n % 128));
                    let hot_res = File::create(&slot_tmp)
                        .and_then(|mut f| {
                            f.write_all(&framed)?;
                            fsync_raw(&f)
                        })
                        .and_then(|()| fs::rename(&slot_tmp, &hot_slots[slot_idx]));
                    if let Err(e) = hot_res {
                        let _ = fs::remove_file(&slot_tmp);
                        local_errs.push(format!("hot atomic overwrite {}: {e}", hot_slots[slot_idx].display()));
                        break;
                    }
                }
                if tid + 1 == workers {
                    writers_done.store(true, Ordering::Release);
                }
                if !local_errs.is_empty() {
                    errors.lock().unwrap().extend(local_errs);
                }
            });
        }

        // Reader threads continuously open+read hot slots to catch non-atomic `ENOENT` windows or torn reads.
        for rtid in 0..workers.min(2) {
            let errors = &errors;
            let hot_slots = &hot_slots;
            let writers_done = &writers_done;
            s.spawn(move || {
                let mut r_errs = Vec::new();
                let mut iter = 0usize;
                while !writers_done.load(Ordering::Acquire) && iter < files * 16 {
                    let slot = &hot_slots[(rtid + iter) % hot_slots.len()];
                    match fs::read(slot) {
                        Ok(bytes) => {
                            if let Err(msg) = verify_framed_payload(&bytes) {
                                r_errs.push(format!("{}: {msg}", slot.display()));
                                break;
                            }
                        }
                        Err(e) => {
                            r_errs.push(format!(
                                "concurrent reader saw error during atomic rename on {}: {e}",
                                slot.display()
                            ));
                            break;
                        }
                    }
                    iter += 1;
                }
                if !r_errs.is_empty() {
                    errors.lock().unwrap().extend(r_errs);
                }
            });
        }
    });

    for slot in &hot_slots {
        let _ = fs::remove_file(slot);
    }

    let failures = errors.into_inner().unwrap();
    Ok((
        failures.is_empty(),
        failures,
        json!({
            "mode": "concurrent",
            "workers": workers,
            "filesPerWorker": files,
            "totalFiles": workers * files,
        }),
    ))
}

pub fn run(mountpoint: &Path, args: &Args) -> Result<Value, String> {
    let meta = fs::symlink_metadata(mountpoint).map_err(|e| format!("{}: {e}", mountpoint.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", mountpoint.display()));
    }
    let started = SystemTime::now();
    let t0 = Instant::now();
    let in_place = args.work_dir == "." || args.work_dir.is_empty();
    let target = if in_place {
        mountpoint.to_path_buf()
    } else {
        let d = mountpoint.join(&args.work_dir);
        fs::create_dir_all(&d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
        d
    };

    info!(
        "fsrand: mode={} ops={} seed={} on {}",
        args.mode,
        args.ops,
        args.seed,
        target.display()
    );

    if args.mode == "all" {
        let work_root = if in_place {
            let d = mountpoint.join(".fstest-fsrand-all");
            fs::create_dir_all(&d).map_err(|e| format!("mkdir {}: {e}", d.display()))?;
            d
        } else {
            target.clone()
        };
        let mut all_ok = true;
        let mut all_failures = Vec::new();
        let mut mode_reports = Vec::new();
        let mut cases = Vec::new();

        for sub in ["tree", "rw", "concurrent"] {
            let sub_dir = work_root.join(sub);
            fs::create_dir_all(&sub_dir).map_err(|e| e.to_string())?;
            let sub_t0 = Instant::now();
            let (ok, fails, summary) = match sub {
                "tree" => run_tree_mode(&sub_dir, args)?,
                "rw" => run_rw_mode(&sub_dir, args)?,
                _ => run_concurrent_mode(&sub_dir, args)?,
            };
            let sub_ms = sub_t0.elapsed().as_millis() as u64;
            if !ok {
                all_ok = false;
                for f in &fails {
                    all_failures.push(format!("{sub}: {f}"));
                }
            }
            cases.push(CaseEntry {
                id: sub.into(),
                outcome: if ok { "pass".into() } else { "fail".into() },
                detail: if ok {
                    summary.to_string()
                } else {
                    fails.first().cloned().unwrap_or_default()
                },
            });
            mode_reports.push(json!({
                "mode": sub,
                "status": if ok { "ok" } else { "failed" },
                "durationMs": sub_ms,
                "summary": summary,
                "failures": fails,
            }));
        }

        if args.cleanup.unwrap_or(true) {
            remove_dir_all_bounded(&work_root, Duration::from_secs(30));
        }
        cases.push(CaseEntry {
            id: "summary".into(),
            outcome: if all_ok { "pass".into() } else { "fail".into() },
            detail: format!("modes=3 ok={all_ok}"),
        });

        return Ok(json!({
            "suite": "fsrand",
            "host": host_name(),
            "date": iso8601(started),
            "top": mountpoint.display().to_string(),
            "params": {
                "mode": "all",
                "ops": args.ops,
                "seed": args.seed,
                "check_every": args.check_every,
                "file_size": args.file_size,
                "workers": args.workers,
                "files": args.files,
                "work_dir": args.work_dir,
            },
            "status": if all_ok { "ok" } else { "failed" },
            "durationMs": t0.elapsed().as_millis() as u64,
            "summary": { "mode": "all", "modesRun": 3 },
            "modes": mode_reports,
            "failures": all_failures,
            "cases": cases,
        }));
    }

    let (ok, failures, summary) = match args.mode.as_str() {
        "tree" => run_tree_mode(&target, args)?,
        "rw" => run_rw_mode(&target, args)?,
        "concurrent" => run_concurrent_mode(&target, args)?,
        other => {
            return Err(format!(
                "unknown --mode {other:?} (expected tree | rw | concurrent | all)"
            ));
        }
    };

    let do_cleanup = args.cleanup.unwrap_or(!in_place);
    if do_cleanup && !in_place {
        remove_dir_all_bounded(&target, Duration::from_secs(30));
    }

    if ok {
        info!(
            "fsrand: {} completed OK in {:.2}s",
            args.mode,
            t0.elapsed().as_secs_f64()
        );
    } else {
        warn!("fsrand: {} FAILED ({} divergences/errors)", args.mode, failures.len());
    }

    let mut cases: Vec<CaseEntry> = failures
        .iter()
        .take(25)
        .enumerate()
        .map(|(idx, f)| CaseEntry {
            id: format!("{}-{idx}", args.mode),
            outcome: "fail".into(),
            detail: f.clone(),
        })
        .collect();
    cases.push(CaseEntry {
        id: "summary".into(),
        outcome: if ok { "pass".into() } else { "fail".into() },
        detail: summary.to_string(),
    });

    Ok(json!({
        "suite": "fsrand",
        "host": host_name(),
        "date": iso8601(started),
        "top": mountpoint.display().to_string(),
        "params": {
            "mode": args.mode,
            "ops": args.ops,
            "seed": args.seed,
            "check_every": args.check_every,
            "file_size": args.file_size,
            "workers": args.workers,
            "files": args.files,
            "work_dir": args.work_dir,
        },
        "status": if ok { "ok" } else { "failed" },
        "durationMs": t0.elapsed().as_millis() as u64,
        "summary": summary,
        "failures": failures,
        "cases": cases,
    }))
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
    fn fsrand_all_modes_pass_and_preserve_simulator_artifacts() {
        let dir = std::env::temp_dir().join(format!("fstest-fsrand-unit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let all_cli = TestCli::parse_from([
            "fsrand",
            "--mode",
            "all",
            "-n",
            "150",
            "--file-size",
            "65536",
            "--workers",
            "3",
            "--files",
            "12",
        ]);
        let r_all = run(&dir, &all_cli.args).expect("fsrand all");
        assert_eq!(r_all["status"], json!("ok"), "r_all: {r_all}");

        // Verify `concurrent` mode leaves exact `workers * files` `stress-*` files when `work_dir = .`.
        let c_cli = TestCli::parse_from(["fsrand", "--mode", "concurrent", "--workers", "2", "--files", "8"]);
        let rc = run(&dir, &c_cli.args).expect("concurrent");
        assert_eq!(rc["status"], json!("ok"));
        let stress_files = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("stress-"))
            .count();
        assert_eq!(stress_files, 16);

        let _ = fs::remove_dir_all(&dir);
    }
}
