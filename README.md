# fstest

Filesystem functional & benchmark test suites.

One Rust binary that ports and wraps the community's filesystem test tools as uniform subcommands: `fstest <suite> [OPTIONS] <MOUNTPOINT>` (options via `fstest <suite> --help`). Logs go to stderr (level via `RUST_LOG` or `-v`/`--verbose`), the final result is emitted as JSON on stdout — `--json <file>` writes a copy — and the exit code is 0 only when every phase reports ok.

| | suites |
|---|---|
| benchmark | [smallfile](#smallfile) · [fsmark](#fsmark) · [fio](#fio) · [mdtest](#mdtest) · [mdworkbench](#mdworkbench) |
| functional | [pjdfstest](#pjdfstest) · [errno-diff](#errno-diff) · [fsx](#fsx) · [fsstress](#fsstress) · [fsrand](#fsrand) · [ltp](#ltp) · [stdfs](#stdfs) · [git](#git) · [sqlite](#sqlite) |
| packaging | [bundle](#bundle) (compile-time self-contained multi-platform binary) |

### Self-contained single-binary build (`linux-x86_64` · `linux-aarch64` · `macos-aarch64`)

A plain `cargo build --release` automatically builds, caches (`~/.cache/fstest/target-bundles/<os>-<arch>-v5/bundle.tar.gz`), and embeds **every external suite and runtime** directly into the `fstest` binary for the target platform — no post-build packing step or external prepare scripts required:

- **macOS arm64 (`aarch64-apple-darwin`)**: `cargo build --release` (embeds 13/14 suites natively including `fio`, `git`, `stdfs`, `pjdfstest`, and `sqlite`; `ltp` reports `skip` cleanly on non-Linux).
- **Linux x86_64 (`x86_64-unknown-linux-gnu` / `musl`) & arm64 (`aarch64-unknown-linux-gnu` / `musl`)**: `cargo build --release --target <triple>` (embeds all 14/14 suites including compiled `fio`, `ltp`, and `git`; when cross-compiling from macOS, `build.rs` automatically builds Linux C suites inside `debian:bullseye` via Docker for broad glibc ≥ 2.31 compatibility).
- Pass `FSTEST_SKIP_BUNDLE=1` or `--no-default-features` during development when you want a lightweight build without embedding external suite assets.

### Canonical JSON envelope & library embedding

Every suite report is enriched with a uniform envelope (`schemaVersion: 1`):

- `mount`: pre/post mountpoint liveness check (`healthy`, `fstype`, `devBefore`, `devAfter`, `disconnectReason`). If a FUSE daemon crashes mid-run (`ENOTCONN`), unmounts (`st_dev` changes), or wedges (`stat` > 5s), the report status is promoted to `mount_disconnected` instead of misreporting cascading `EIO`/`ENOENT` test failures.
- `cases`: flat array of `{ id, outcome, detail }` (`outcome` ∈ `pass | fail | skip | blocked`) across every functional and benchmark suite.
- `metrics`: flat numeric map (`f64`, with latencies normalized to `_ms` and rates to `_per_sec` / `iops` / `bw_mibps`) for automated budget regression checks.

`fstest` also exposes `fstest::cli_main_from` and `fstest::try_run_helper_mode` as a Rust library crate (`src/lib.rs`) so external test harnesses can link all suites into a single multi-call binary.

## smallfile

Port of [distributed-system-analysis/smallfile](https://github.com/distributed-system-analysis/smallfile). The default run executes, with each phase timed and reported independently:

```
cleanup → create → read(verified) → append → rename → delete-renamed → cleanup
```

Stonewall is disabled automatically for the default multi-phase run (and `finish_all_rq` is enforced on `create`/`mkdir`/`cleanup`/`swift-put` so subsequent phases never starve for files). Single-operation mode (`--operation`) keeps stonewall on by default, matching upstream:

```
fstest smallfile --operation create --threads 4 --files 10000 /mnt/workspace
```

| Option | Default | Description |
|---|---|---|
| `--operation <op>` | default phase sequence | create/read/append/overwrite/truncate-overwrite/rename/delete/delete-renamed/cleanup/stat/chmod/mkdir/rmdir/symlink/readdir/ls-l/setxattr/getxattr/swift-put/swift-get/await-create |
| `--files <N>` | 200 | files per thread |
| `--threads <N>` | 2 | threads |
| `--file-size <KB>` | 64 | mean file size |
| `--file-size-distribution <fixed\|exponential>` | fixed | deterministic per-thread seed derived from `(top, host, tid)` |
| `--record-size <KB>` | 0 (= file size) | I/O record size, capped at 1 MiB |
| `--files-per-dir <N>` | 100 | files per directory (`d_000…`) |
| `--dirs-per-dir <N>` | 10 | subdirectories per directory |
| `--stonewall <Y/N>` | Y | stop measuring once the first thread finishes |
| `--finish <Y/N>` | Y | finish remaining files after stonewall on mutating setup ops |
| `--verify-read <Y/N>` | Y | read back and verify byte by byte |
| `--incompressible <Y/N>` | N | high-entropy SplitMix64 payload instead of 7-bit ASCII pattern |
| `--fsync <Y/N>` | N | `fsync(2)` after each file is written |
| `--same-dir <Y/N>` | N | all threads share one directory tree |
| `--hash-into-dirs <Y/N>` | N | hash file numbers into directory names (`h_000…`); readdir/ls-l unsupported |
| `--pause <USEC>` / `--auto-pause <Y/N>` | 0 / N | fixed or Little's-law adaptive inter-file throttle |
| `--response-times <Y/N>` | N | write per-op response times to `<top>/network_shared/rsptimes_*.csv` |
| `--xattr-size <B>` / `--xattr-count <N>` | 0 | xattr value size and count (`fsetxattr`/`fgetxattr`) |
| `--record-ctime-size <Y/N>` / `--await-timeout <S>` | N / 60 | `await-create` replication-latency xattr & bounded poll timeout |
| `--prefix` / `--suffix` | empty | filename prefix/suffix |

## fsmark

Port of [fs_mark 3.3](https://github.com/josefbacik/fs_mark): a synchronous/async file-creation benchmark with per-system-call microsecond statistics (`creat`, `write`, `fsync`, `close`, `unlink`, and `syncfs`).

The positional mount point is the first test directory; pass `-d/--dir` repeatedly for more. `-S` selects sync method `0..6` (no sync, in-band `fsync`, `syncfs`+single `fsync`, post-reverse `fsync`, `syncfs`+post-reverse, post-in-order, `syncfs`+post-in-order). `-D N` spreads files across `N` `%02x` subdirectories with a time-based hash policy; adding `-N files` switches to round-robin. `-L N` runs `N` iterations (implies keeping files), `-k` keeps files, `-F` runs until `ENOSPC`, `--incompressible` uses pseudo-random bytes instead of zeros, `-v/--verbose-stats` adds per-syscall min/avg/max columns to the text table, and `-l <file>` appends the `fs_mark`-format text report to a file.

```
fstest fsmark -n 2000 -s 4096 -S 1 -D 8 -L 5 /mnt/workspace
```

## fio

Pure pass-through to **compile-time embedded upstream [axboe/fio](https://github.com/axboe/fio)** (`fio-3.38`, zero target-host runtime dependencies; Linux links `libaio` statically alongside `io_uring` and disables `libnuma` so minimal hosts need neither `libaio.so.1` nor `libnuma.so.1`, and macOS uses `posixaio`/`psync`/`mmap`).

- **Zero-option default matrix (`fstest fio <mnt>`)**: runs a **5-phase standard FUSE/cloud-FS benchmark matrix** (`seq-write` `1m`, `seq-read` `1m`, `rand-write` `4k`, `rand-read` `4k`, `rand-rw` `4k` 75/25) inside `<mnt>/.fstest-fio/` (probing `O_DIRECT`/`F_NOCACHE` support automatically) and cleans up afterwards.
- **100% verbatim native `fio` pass-through (`fstest fio [FIO_OPTIONS...] <mnt>`)**: every native `fio` option or `.fio` job file is forwarded untouched to the embedded `fio` binary with `--directory=<mnt>` and `--output-format=json` (`--json <FILE>` is the only `fstest` envelope flag).

```
fstest fio /mnt/jfs                                                          # default 5-phase benchmark matrix
fstest fio --rw=randrw --bs=4k --size=1g --numjobs=4 --iodepth=32 --runtime=30 --time_based --group_reporting=1 /mnt/jfs
fstest fio --rw=randwrite --bs=4k --nrfiles=4 --filesize=16k --fdatasync=1 /mnt/jfs
fstest fio workload.fio /mnt/jfs
fstest fio -h                                                                # upstream fio --help + all 226 job options + engines
fstest fio --cmdhelp=verify                                                  # detailed upstream docs for any option
```

## mdtest

Single-node threaded port of [mdtest](https://github.com/hpc/ior) (LLNL). Preserves upstream directory tree layout (`test-dir.<iter>-<loop>/mdtest_tree[.<rank>].<n>`), rotating directory rename chains, per-rank min/max/mean/sum aggregation, cross-rank stonewall wear-out top-up, and `-N` rank stride phase shifting. Any operational syscall error (`mkdir`/`open`/`write`/`fsync`/`stat`/`read`/`rename`/`unlink`/`rmdir`) increments `ioErrors` and marks the report `"failed"`.

```
fstest mdtest -n 10000 -z 2 -b 4 --threads 4 /mnt/workspace
fstest mdtest --threads 4 -u -z 2 -b 2 -I 50 -N 1 -w 4096 -e 4096 -X /mnt/workspace
fstest mdtest -C -n 5000 -w 4096 /mnt/workspace   # create files with data
fstest mdtest -E -n 5000 -e 4096 -X /mnt/workspace # read back and verify
fstest mdtest -r -n 5000 /mnt/workspace            # remove-only cleanup
```

| Option | Default | Description |
|---|---|---|
| `--threads <N>` | 1 | number of ranks |
| `-n/--items <N>` | 1000 (if `-I` unset) | items per rank (spread across the tree) |
| `-I/--items-per-dir <N>` | 0 | items per directory |
| `-z/--depth`, `-b/--branch-factor` | 0, 1 | tree shape |
| `-i/--iterations <N>` | 1 | iterations |
| `-u/--unique-dir` | off | one `mdtest_tree.<rank>.0` hierarchy per rank |
| `-F/--files-only`, `-D/--dirs-only` | both on | restrict to files or directories |
| `-C/-T/-E/-r` | all on | only create/stat/read/remove phase |
| `-U/--rename-dirs` | off | directory rename phase |
| `-L/--leaf-only` | off | items only at the leaf level |
| `-w/--write-bytes`, `-e/--read-bytes` | 0 | data bytes per item |
| `-y/--sync-file`, `-Y/--sync-after-phase` | off | `fsync` per file / `syncfs` per phase |
| `-N/--nstride <N>` | 0 | rank shift between phases (`1` avoids client caches) |
| `-R/--random`, `--random-seed` | off | random stat order |
| `-W/--stonewall <SEC>` | 0 | stonewall file creation timer with cross-rank wear-out (`-b <= 1`) |
| `-B/--no-barriers`, `-p/--pre-delay` | off, 0 | phase sync control |
| `-X/--verify-read` | off | verify read data back byte-for-byte (requires read phase and `-e > 0`) |

## mdworkbench

Single-node threaded port of [md-workbench](https://github.com/hpc/ior): steady-state metadata & small-object QoS benchmark over a fixed FIFO working set (`precreate` → `benchmark-<i>` → `cleanup`). Persists `<out-dir>/md-workbench.status` (`current_index` + `seed`) when run without `-3` so split `-1` → `-2 -X` → `-3` runs stay aligned automatically.

```
fstest mdworkbench -P 3000 -I 1000 -D 10 -S 3901 -R 3 --threads 2 -X /mnt/workspace
```

| Option | Default | Description |
|---|---|---|
| `--threads <N>` | 1 | number of ranks |
| `-I/--obj-per-proc <N>` | 1000 | benchmark ops per data set (`<= precreate` keeps the FIFO exact) |
| `-P/--precreate-per-set <N>` | 3000 | objects precreated per data set |
| `-D/--data-sets <N>` | 10 | data sets per rank |
| `-S/--object-size <B>` | 3901 | object size |
| `-R/--iterations <N>` | 3 | benchmark phase repetitions |
| `-O/--offset <N>` | 1 | rank shift between readers and writers |
| `-o/--out-dir <NAME>` | out | working directory under the mount point |
| `-t/--waiting-time <F>` | 0 | sleep `F × op-runtime` after each op |
| `-w/--stonewall-timer <S>`, `-W` | 0, off | stop benchmark after `S` seconds; `-W` aligns iteration counts across ranks |
| `-1/-2/-3` | all on | run only precreate/benchmark/cleanup |
| `--read-only` | off | benchmark stats+reads without deletes/writes |
| `-y/--fsync` | off | `fsync` each created object |
| `-X/--verify-read`, `-G` (seed), `--start-item` | off/0 | byte-level verification and resume controls |

Per-phase output reports aggregate and per-rank `p50`/`p90`/`p99`/`max` latencies (`create`, `read`, `stat`, `delete`), phase-specific `iops` (`4 × obj_read` in RW benchmark, `2 × obj_read` in `--read-only`, `dset + obj` in `precreate`/`cleanup`), `rateObjPerSec`, and `throughputMiBps` (`(obj_read + obj_create) × object_size`).

## pjdfstest

POSIX conformance functional suite from [pjd/pjdfstest](https://github.com/pjd/pjdfstest) (238 `.t` files embedded at compile time; privileged syscall helper implemented as `fstest __pjdfstest` inside the same binary). Runs `.t` files concurrently across `-j/--jobs` isolated scratch directories under `<mount>/.fstest-pjdfstest/`.

```
sudo fstest pjdfstest /mnt/jfs                      # full suite in parallel (root needed for chown/mknod cases)
fstest pjdfstest -j 8 --filter chmod --filter rename /t
fstest pjdfstest --fs FUSE.JUICEFS /mnt/jfs
```

| Option | Default | Description |
|---|---|---|
| `--filter <SUBSTR>` | all | run only files whose path contains the substring (repeatable) |
| `-j/--jobs <N>` | `min(CPU, 8)` | parallel `.t` worker count |
| `--fs <NAME>` | auto-detect | override filesystem type for `supported()` gating (e.g. `FUSE.JUICEFS`, `EXT4`, `APFS`) |
| `--os <NAME>` | auto-detect | override OS name (`Darwin`, `Linux`, `FreeBSD`) |
| `--timeout <SEC>` | 120 | per-file process-group timeout |
| `--keep` | off | keep scratch directories |

## errno-diff

Differential syscall & `errno` oracle comparing the target mountpoint against a local POSIX reference directory (`--ref-dir`, avoiding `tmpfs` so xattr/sparse/fallocate probes reflect real disk filesystems). Exercises **84 scenarios across families A–M** (`open` flags, `rename` atomicity/cycles, `unlink`/`rmdir`, `link`/`symlink`/`readlink`, `chmod`/`chown`/`utimensat`, `truncate`/`pread`/`pwrite`, byte-edge filenames, `readdir`, `xattr`, `flock`/`fcntl` locks, `fallocate`/`copy_file_range`/`SEEK_HOLE`/`SEEK_DATA`, and open-unlinked file I/O), plus optional cross-layer (`--alt-root`, family `X`) and git-workspace (`--git-ws-root`, family `W`) checks. Automatically enables `--strict-posix` when the target mountpoint is itself a local kernel filesystem (`ext4`, `xfs`, `btrfs`, `apfs`).

```
fstest errno-diff /mnt/jfs
sudo fstest errno-diff --require-ext4 --table-out errno-matrix.txt /mnt/jfs
fstest errno-diff --strict-posix /mnt/jfs
fstest errno-diff --diverge-as-record --alt-root /mnt/jfs-observer /mnt/jfs
```

## fsx

Port of [fsx](https://github.com/kdave/xfstests/blob/master/ltp/fsx.c) from xfstests: single-file data-integrity fuzzer verified against an in-memory shadow buffer (`good_buf`). Exercises `read`, `write`, `mapread`, `mapwrite`, `truncate`, `fallocate` (with and without `KEEP_SIZE`), `punch_hole`, `zero_range`, `write_zeroes`, `collapse_range`, `insert_range`, `clone_range`, `copy_range`, periodic `fsync`, and `-c` close/reopen cache-flush cycles. Uses glibc's TYPE_3 `random()` so identical seeds replay deterministic sequences.

```
fstest fsx -N 10000 -l 262144 -c 10 /mnt/workspace
fstest fsx -N 5000 -Z --keep-good /mnt/workspace          # direct I/O + save <file>.fsxgood
fstest fsx --verify-good --cleanup /mnt/workspace         # post-remount byte-for-byte durability check
fstest fsx --record-ops /mnt/workspace                    # save <file>.fsxops
fstest fsx --replay-ops /mnt/workspace/fsx.testfile.fsxops /mnt/workspace
```

Key options: `-N` ops, `-l` max file size, `-o` max op size, `-S` seed, `-r/-w/-t` alignment boundaries, `-Z/--direct` (`O_DIRECT` + 4K bounce buffers on Linux, `F_NOCACHE` on macOS), `-b/--begin-op` simulated warm-up, `-c P` close+reopen probability (`1/P`), `-L` lite, `-F/-H/-z/-k/-C/-I/-J/-E/--no-fsync-op` op toggles, `-R/-W` mapped I/O toggles, `-X` full-file check every op, `-y` `fsync` every write, `-e` post-EOF page pollution, `--keep-good`, `--verify-good`, `--record-ops`, `--replay-ops`, `--cleanup`, `--duration`.

## fsstress

Port of [fsstress](https://github.com/kdave/xfstests/blob/master/ltp/fsstress.c) from xfstests: multi-threaded randomized directory-tree stress mixing 37 metadata and data operations (including directory & cross-directory `rename`, `rnoreplace`, `rexchange`, `clonerange`, `copyrange`, `fallocate`/`punch`/`zero`/`collapse`/`insert`, `link`/`symlink`/`mkfifo`/`mknod`, `chmod`/`utimens`, `xattr`, `fsync`/`fdatasync`/`syncfs`). Expected concurrency-race errnos (`ENOENT`, `EEXIST`, `ENOTEMPTY`, `EINVAL`, …) are tolerated, whereas fatal filesystem/transport errors (`EIO`, `ENOTCONN`, `EUCLEAN`, `EBADFD`) fail the run immediately in the report.

```
fstest fsstress -n 10000 -p 4 -c /mnt/workspace
fstest fsstress -n 5000 -z -f creat=10 -f write=20 -f rename=10 -c /mnt/workspace
```

| Option | Default | Description |
|---|---|---|
| `-n/--nops <N>` | 1000 | operations per thread |
| `-p/--nproc <N>` | 1 | worker threads |
| `-l/--loops <N>` | 1 | whole-run loops (`0` = loop until `--duration`) |
| `-s/--seed <N>` | time-based | RNG seed |
| `-z/--zero-freq` | off | zero all default frequencies before applying `-f` |
| `-f op=N` | upstream freqs | override operation frequency (repeatable; `mread`/`mwrite` opt-in) |
| `-R` / `-w` | off | read-only / write-only frequency filter |
| `-c/--cleanup` | off | bounded removal of created subtrees after completion |
| `--duration <SEC>` | 0 | time-based run |

## fsrand

Deterministic model-checked verification suite (`--mode <tree|rw|concurrent|all>`, default `tree`):

- `tree`: maintains an in-memory shadow directory tree (file byte payloads, sparse zero-fill extensions, executable mode bits, hardlink inode-group parity, symlink targets, nested directories) across seeded `create`, `overwrite`, `append`, sparse `pwrite`, `truncate` (shrink & sparse extend), `mkdir`, `rmdir`, `unlink`, cross-dir/overwrite `rename`, `hardlink`, `symlink`, `chmod`, and `fsync` operations (payloads up to 64 KiB crossing 4 KiB page boundaries), walking and diffing the mount every `--check-every` ops.
- `rw`: performs seeded random-offset `pwrite` (up to 64 KiB) + `fsync` + periodic close/reopen `pread` checks against an exact byte-array shadow buffer.
- `concurrent`: spawns `--workers` writer threads plus concurrent hot-slot atomic-rename & reader threads verifying zero torn payloads or transient `ENOENT` windows under contention.
- `all`: executes `tree` → `rw` → `concurrent` sequentially in isolated subdirectories and aggregates all cases/metrics.

```
fstest fsrand --mode all --ops 1000 --seed 42 /mnt/jfs
fstest fsrand --mode tree --ops 2000 --seed 42 /mnt/jfs
fstest fsrand --mode rw --file-size 4194304 --ops 500 /mnt/jfs
fstest fsrand --mode concurrent --workers 8 --files 64 /mnt/jfs
```

## ltp

Adapter for the [Linux Test Project](https://github.com/linux-test-project/ltp) (`20260529`) — aligned with JuiceFS's official POSIX compatibility and CI methodology. Default suites: **`syscalls`**, **`fs`**, **`fs_perms_simple`**, and **`fcntl-locktests`** (with built-in `rm_syscalls` / `rm_fs` filtering, `TMPDIR`/`LTP_TMPDIR` on `<mount>/.fstest-ltp/tmp`, `fcntl-locktests` bound to `$TMPDIR`, and `kirk`-compatible exit-code & sub-assertion accounting).

```
sudo fstest ltp /mnt/jfs
sudo fstest ltp --ltp-dir /opt/ltp /mnt/jfs
sudo fstest ltp --suite syscalls,fs,fs_bind,fs_perms_simple,smoketest,fcntl-locktests /mnt/jfs
```

## stdfs

Runs the filesystem-relevant portions of the **Go**, **Python**, **Node.js**, and **Rust** standard-library test suites against `<mountpoint>/.fstest-stdfs/` using compile-time embedded runtimes and test suites (**zero target-host runtime dependencies**):

- **go**: precompiled `os`, `path/filepath`, `io/fs` test binaries + staged fixtures (`TestReadDir`, `TestDirFS`, `TestCopyFS`, `TestGlob`, `TestRoot*`, etc.).
- **python**: standalone CPython `3.12` + `Lib/test` across 13 FS modules (`os`, `posix`, `posixpath`, `genericpath`, `pathlib`, `shutil`, `tarfile`, `stat`, `fileio`, `glob`, `tempfile`, `mmap`, `fcntl`).
- **node**: Node.js `v22` `node --test` over curated `test/parallel/test-fs-*.js`, including `stat()`-polling `fs.watchFile` on all platforms and event-driven `fs.watch` / `fsPromises.watch` (`inotify` over FUSE) on Linux by default (`--node-watch` / `--no-node-watch`).
- **rust**: build-time embedded `library/std/src/fs/tests.rs` executed in parallel via `fstest __stdfs-rust`.

```
fstest stdfs /mnt/jfs
fstest stdfs --lang go,rust /mnt/jfs
```

## git

Runs **67 curated filesystem, storage-backend (`files` + `reftable`, `pack`/`mmap`/`MIDX`/`commit-graph`/`gc`/`repack`), index/worktree, and porcelain workflow scripts** from Git's `t/` suite against `<mount>/.fstest-git/` with **`GIT_TEST_FSYNC=1` enabled by default**, using the compile-time embedded Git `v2.55.0` build tree (or `--all` for all ~1,046 scripts).

```
fstest git /mnt/jfs
fstest git --no-fsync /mnt/jfs
fstest git --tests t0610,t5319,t7700 /mnt/jfs
```

## sqlite

Comprehensive SQLite filesystem verification suite built on bundled SQLite (`rusqlite` `"bundled"`). Runs **7 probe families (20 items)** with `PRAGMA synchronous=FULL` by default (`mptest` 5-process rollback + WAL crash recovery, `crud` relational/FK/JSONB/external-sort/savepoint/`ATTACH`/incremental-BLOB/`FTS5`/`VACUUM INTO`, `speedtest` 5-mode SHA-256 parity, `kvtest`, `walthread`, `wal-lifecycle`, and cross-process `mmap` + `fcntl` lock probes), routing `temp_store_directory` / `SQLITE_TMPDIR` onto `<mount>/.fstest-sqlite/tmp`.

```
fstest sqlite /mnt/jfs
fstest sqlite --probes crud,wal-lifecycle,mmap /mnt/jfs
fstest sqlite --no-sync /mnt/jfs
```

## bundle

Inspects or pre-extracts the compile-time embedded suite bundle (`fio-3.38`, Go stdlib `.test` binaries, standalone CPython `3.12` + `Lib/test`, Node.js `v22` + `test-fs-*.js`, Git `v2.55.0` + `t/`, and Linux LTP `20260529`). Extracted once by SHA-256 into `~/.cache/fstest/bundles/<hash>/` off the mount under test.

```
fstest bundle info
fstest bundle extract
fstest bundle extract --dir /tmp/suite
```

## Platform notes

- `fsync` calls plain `fsync(2)` everywhere rather than Apple `fcntl(F_FULLFSYNC)`, keeping POSIX durability and benchmark numbers comparable with C/Python/Go tools; filesystem-scoped sync uses `syncfs(2)` on Linux instead of host-wide `sync(2)`.
- Preallocation uses `posix_fallocate(fd, offset, len)` on Linux and `fcntl(F_PREALLOCATE)` + `ftruncate` extension on macOS.
- Third-party notices and upstream license texts for behaviorally ported and bundled suites live in [licenses/](licenses/).
