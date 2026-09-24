# fstest

Filesystem functional & benchmark test suites.

One Rust binary that ports and wraps the community's filesystem test tools as uniform subcommands: `fstest <suite> [OPTIONS] <MOUNTPOINT>` (options via `fstest <suite> --help`). Logs go to stderr (level via `RUST_LOG`), the final result is emitted as JSON on stdout — `--json <file>` writes a copy — and the exit code is 0 only when every phase reports ok.

| | suites |
|---|---|
| benchmark | [smallfile](#smallfile) · [fsmark](#fsmark) · [fio](#fio) · [mdtest](#mdtest) · [mdworkbench](#mdworkbench) |
| functional | [pjdfstest](#pjdfstest) · [fsx](#fsx) · [fsstress](#fsstress) · [ltp](#ltp) · [stdfs](#stdfs) |

Defaults policy: ported suites keep the upstream tools' defaults so numbers stay directly comparable; wrapped suites pick mount-point best-practice defaults instead of engine defaults that assume local block devices (see [Platform notes](#platform-notes)).

## smallfile

Port of [distributed-system-analysis/smallfile](https://github.com/distributed-system-analysis/smallfile). The default run executes, with each phase timed and reported independently:

```
cleanup → create → read(verified) → append → rename → delete-renamed → cleanup
```

This mirrors running smallfile operation by operation; stonewall is disabled automatically for the multi-phase run (a stonewalled slow thread would starve later phases of files — smallfile's own regtest.sh disables it the same way). Single-operation mode (`--operation`) keeps stonewall on by default, matching upstream:

```
fstest smallfile --operation create --threads 4 --files 10000 /mnt/workspace
```

| Option | Default | Description |
|---|---|---|
| `--operation <op>` | default phase sequence | create/read/append/overwrite/truncate-overwrite/rename/delete/delete-renamed/cleanup/stat/chmod/mkdir/rmdir/symlink/readdir/ls-l/setxattr/getxattr/swift-put/swift-get/await-create |
| `--files <N>` | 200 | files per thread |
| `--threads <N>` | 2 | threads |
| `--file-size <KB>` | 64 | mean file size |
| `--file-size-distribution <fixed\|exponential>` | fixed | the per-thread random seed is derived deterministically from (top, host, tid), so separate invocations replay identical size sequences |
| `--record-size <KB>` | 0 (= file size) | I/O record size, capped at 1 MiB |
| `--files-per-dir <N>` | 100 | files per directory (radix-named d_000…) |
| `--dirs-per-dir <N>` | 10 | subdirectories per directory |
| `--stonewall <Y/N>` | Y | stop measuring once the first thread finishes |
| `--finish <Y/N>` | N | finish remaining files after stonewall |
| `--verify-read <Y/N>` | Y | read back and verify byte by byte |
| `--fsync <Y/N>` | N | fsync after each file is written |
| `--same-dir <Y/N>` | N | all threads share one directory tree (filenames carry tid/host) |
| `--hash-into-dirs <Y/N>` | N | hash file numbers into directory names (h_000…); readdir/ls-l unsupported in this mode |
| `--pause <USEC>` | 0 | pause between files |
| `--auto-pause <Y/N>` | N | auto-tune the pause from response times (Little's law throttle) |
| `--response-times <Y/N>` | N | record per-op response times to `<top>/network_shared/rsptimes_*.csv` |
| `--xattr-size <B>` / `--xattr-count <N>` | 0 | xattr value size and count for xattr/swift operations |
| `--record-ctime-size <Y/N>` | N | record create time + size as an xattr for await-create replication-latency measurement |
| `--prefix` / `--suffix` | empty | filename prefix/suffix |

Directory/file layout matches upstream: `<top>/file_srcdir/<host>/thrd_XX/d_000/_<host>_XX_N_`, with rename targets under `file_dstdir`. File contents are a deterministic pattern (k%128 with a per-file offset), so fstest interoperates with the python original both ways (python `--verify-read Y` validates files written by fstest and vice versa). Result aggregation follows smallfile's `output_results.py`: rates are summed per thread, elapsed is the max, status is the first non-ok thread. In single-operation mode the report has a top-level `results` instead of `phases`.

## fsmark

Port of [fs_mark 3.3](https://github.com/josefbacik/fs_mark): a synchronous/async file-creation benchmark with per-system-call microsecond statistics. For each file it times creat, write, optional fsync and close separately (min/avg/max per iteration), then unlinks unless files are kept; the iteration rate and the application overhead (loop time minus measured syscall time) are reported, and the run ends with Average plus p50/p90/p99 files/sec.

The positional mount point is the first test directory; pass `-d/--dir` repeatedly for more (thread k uses directory k%N, so the thread count must be an even multiple of the directory count, or directories are raised to the thread count). `-S` selects the sync method 0..6 (no sync, in-band fsync, sync()+single fsync, post-reverse fsync, sync+post-reverse, post-in-order, sync+post-in-order). `-D N` spreads files across N `%02x` subdirectories with a time-based hash policy; adding `-N files` switches to round-robin with a fixed file count per subdirectory. `-L N` runs N iterations (implies keeping files), `-k` keeps files, `-F` runs until the filesystem is full, and `-l <file>` appends the fs_mark-format text report to a log file. Unlike fs_mark, which always writes `fs_log.txt` into the current directory, fstest only writes the log when `-l` is given; the JSON report on stdout (and `--json`) is the primary record.

```
fstest fsmark -n 2000 -s 4096 -S 1 -D 8 -L 5 /mnt/workspace
```

The text report (stderr log lines and the `-l` log file) matches fs_mark's column layout, including the `--verbose-stats` variant that adds per-syscall min/avg/max columns. The JSON report adds per-iteration aggregates, per-thread rows and the summary percentiles. fs_mark's per-iteration rates are truncated to integers and sorted descending for the percentiles, exactly like the C code.

## fio

Thin wrapper around the system [fio](https://github.com/axboe/fio) binary (assumed to be in `PATH`, override with `--fio`): it maps the common best-practice knobs onto one fio invocation, always runs with JSON output (written to a temp file via `--output`, since fio's stdout can interleave non-JSON noise), and repackages the result into the standard fstest JSON envelope. Failures still produce a valid JSON report (`status: failed`, fio exit code, stderr tail).

```
fstest fio --rw randrw --bs 4k --size 1g --numjobs 4 --iodepth 32 --runtime 30 /mnt/workspace
```

| Option | Default | Description |
|---|---|---|
| `--rw <pattern>` | randrw | read/write/randread/randwrite/rw/randrw |
| `--bs <size>` | 4k | block size, fio syntax (4k, 1m, 4k,1m …) |
| `--size <size>` | 256m | per-job file size |
| `--numjobs <N>` | 4 | parallel jobs |
| `--iodepth <N>` | 32 | queue depth per job |
| `--ioengine <name>` | libaio (Linux), posixaio (macOS) | "auto" lets fio choose |
| `--direct <Y/N>` | Y | O_DIRECT, bypass the page cache |
| `--rwmixread <PCT>` | 75 | read share in rw/randrw |
| `--runtime <SEC>` | 0 (size-based) | time-based run for N seconds (per fio job) |
| `--group-reporting <Y/N>` | Y | aggregate per-job stats |
| `--verify <METHOD>` | unset (off) | data-integrity check; the value is passed verbatim to fio `--verify` (crc32, md5, sha256, meta, pattern …; requires a write side in the workload) |
| `--keep-files` | off | keep fio test files (unlinked by default) |
| `--job <FILE>` | — | expert mode: pass a fio job file; fstest adds only `--directory` and output plumbing |
| `--fio-arg <ARG>` | — | extra argument passed verbatim to fio (repeatable) |
| `--fio <PATH>` | fio | binary to invoke |

The report carries a normalized `summary` (with group reporting: the aggregated group entry) and per-`jobs` rows — read/write iops, bandwidth bytes, io bytes and completion-latency p50/p95/p99 in nanoseconds (both fio's `clat_ns` and the older `clat` layout are understood) — plus the full unmodified fio JSON under `raw`, so nothing upstream reports is lost. The fio version is detected and recorded for reproducibility.

## mdtest

Behavior-level port of [mdtest](https://github.com/hpc/ior) (LLNL), single-node: MPI ranks become threads, `MPI_Barrier` becomes `std::sync::Barrier`. The directory tree layout (`test-dir.<iter>-<loop>/mdtest_tree.<n>`, item numbers encoding their directory), the rotating directory rename chain, per-rank rates with min/max/mean/sum aggregation across ranks, and the nstride phase-shift trick (each phase accesses items a different rank created, to defeat client caches) all follow the C implementation.

```
fstest mdtest -n 10000 -z 2 -b 4 --threads 4 /mnt/workspace
fstest mdtest -C -n 5000 -w 4096 /mnt/workspace   # create files with data
fstest mdtest -E -n 5000 -e 4096 -X /mnt/workspace # read back and verify (separate invocation)
fstest mdtest -r -n 5000 /mnt/workspace            # remove-only cleanup
```

| Option | Default | Description |
|---|---|---|
| `--threads <N>` | 1 | number of ranks |
| `-n/--items <N>` | 0 | items per rank (spread across the tree) |
| `-I/--items-per-dir <N>` | 0 | items per directory instead |
| `-z/--depth`, `-b/--branch-factor` | 0, 1 | tree shape |
| `-i/--iterations <N>` | 1 | iterations |
| `-u/--unique-dir` | off | one tree per rank instead of a shared tree |
| `-F/--files-only`, `-D/--dirs-only` | both on | restrict to files or directories |
| `-C/-T/-E/-r` | all on | only create/stat/read/remove phase |
| `-U/--rename-dirs` | off | directory rename phase |
| `-L/--leaf-only` | off | items only at the leaf level |
| `-w/--write-bytes`, `-e/--read-bytes` | 0 | data bytes per item |
| `-y/--sync-file`, `-Y/--sync-after-phase` | off | fsync per file / sync() per phase |
| `-N/--nstride <N>` | 0 | rank shift between phases (1 avoids client caches) |
| `-R/--random`, `--random-seed` | off | random stat order |
| `-W/--stonewall <SEC>` | 0 | stop file creation after N seconds (branch factor <= 1) |
| `-B/--no-barriers`, `-p/--pre-delay` | off, 0 | phase sync control |
| `-X` (`--verify-read`) | off | verify data on read (requires `-E` and `-e`, like upstream) |

The report carries per-iteration per-rank times/rates plus a summary with `totalRate` (sum across ranks), `minRate`/`maxRate`/`meanRatePerRank` — mdtest's classic cross-rank statistics. Not ported: shared-file mode (`-S`), collective creates (`-c`), mknod, GPU options and the CSV exports (the JSON covers them).

## mdworkbench

Behavior-level port of [md-workbench](https://github.com/hpc/ior) (Kunkl): a steady-state metadata benchmark over a fixed working set. `precreate` populates N objects per data set, the `benchmark` phase repeatedly stats/reads/deletes the oldest object and recreates a new one (FIFO; readers and writers are rank-shifted by `--offset`, so with 2+ threads each rank works on its neighbor's objects), and `cleanup` removes the remaining tail. Data sets live under `<mountpoint>/<out-dir>/<rank>_<d>/file-<i>`.

```
fstest mdworkbench -P 3000 -I 1000 -D 10 -S 3901 -R 3 --threads 2 /mnt/workspace
```

| Option | Default | Description |
|---|---|---|
| `--threads <N>` | 1 | number of ranks |
| `-I/--obj-per-proc <N>` | 1000 | benchmark ops per data set (<= precreate keeps the FIFO exact) |
| `-P/--precreate-per-set <N>` | 3000 | objects precreated per data set |
| `-D/--data-sets <N>` | 10 | data sets per rank |
| `-S/--object-size <B>` | 3901 | object size |
| `-R/--iterations <N>` | 3 | benchmark phase repetitions |
| `-O/--offset <N>` | 1 | rank shift between readers and writers |
| `-o/--out-dir <NAME>` | out | working directory under the mount point |
| `-t/--waiting-time <F>` | 0 | sleep F×op-runtime after each op (throttle) |
| `-w/--stonewall-timer <S>`, `-W` | 0, off | stop benchmark after S seconds; `-W` makes all ranks do the same count |
| `-1/-2/-3` | all on | run only precreate/benchmark/cleanup |
| `--read-only` | off | benchmark without deletes/writes |
| `-X` (`--verify-read`), `-G` (seed), `--start-item` | off/0 | verification and resume controls |

Per-phase output includes per-op latency percentiles (create/read/stat/delete p50/p90/p99), the max op time, cross-rank time balance and the object rates — the steady-state/QoS view mdtest does not provide. Byte-level interop with the C tool's data patterns is not provided (its `dataPacketType` layouts are not replicated); `-X` verification is self-consistent across invocations of this tool.

## pjdfstest

POSIX conformance functional suite from [pjd/pjdfstest](https://github.com/pjd/pjdfstest) — the tool JuiceFS cites for its "8,789 tests" compatibility claim. The upstream shell tests (238 `.t` files, `misc.sh`, `conf`) are vendored under `vendor/pjdfstest/tests/` and **embedded into the binary at compile time**; at runtime each test is piped to `sh -c` with an inline prelude, so a single fstest binary runs the whole suite and nothing but the tests' own scratch files ever touches the filesystem under test (scratch dirs live under `<mount>/.fstest-pjdfstest/` and are removed afterwards). The upstream setuid `pjdfstest` C helper is reimplemented as the hidden `fstest __pjdfstest` mode and invoked through the current executable.

```
sudo fstest pjdfstest /mnt/jfs                      # full suite (root needed for chown/mknod cases)
fstest pjdfstest --filter chmod --filter rename /t  # subset
fstest pjdfstest --fs FUSE.JUICEFS /mnt/jfs         # force the fs name used by `supported()` gating
```

| Option | Default | Description |
|---|---|---|
| `--filter <SUBSTR>` | all | run only files whose path contains the substring (repeatable) |
| `--fs <NAME>` | auto-detect | override the filesystem type for feature gating (e.g. `FUSE.JUICEFS`) |
| `--os <NAME>` | auto-detect | override the OS name (`Darwin`, `Linux`, `FreeBSD`) |
| `--timeout <SEC>` | 120 | per-file timeout |
| `--keep` | off | keep scratch directories |

Results are per-file TAP counts folded into the fstest JSON envelope (per-file `tests/passed/failed`, failing assertion lines, summary totals). Running as root is required for the chown/mknod/permission cases — without root those cases fail identically to upstream. Verification: the runner reproduces the native `pjdfstest` + `sh` + `prove` counts **exactly, per file**, on the same machine and configuration (238/238 files matched). With `os=Linux`, `fs=FUSE.JUICEFS` and root, the totals are directly comparable with JuiceFS's published `Files=237, Tests=8789` numbers. Runtime needs a POSIX `sh` plus basic utilities (`stat`, `df`, `dd`, `openssl` for name generation).

## fsx

Behavior-level port of [fsx](https://github.com/kdave/xfstests/blob/master/ltp/fsx.c) from xfstests (originally NeXT, 1991): a single-file data-integrity fuzzer. A shadow buffer tracks the expected file image; every operation (read/write/mapread/mapwrite/truncate/fallocate/punch hole/zero range/collapse/insert range/clone/copy range) is applied to both the shadow and the real file, and after every operation the size is verified and read-back data compared byte by byte. On mismatch fsx dumps the full operation log, saves the expected image (`<file>.fsxgood`), the operation record (`<file>.fsxops`, replayable via `--replay-ops`) and fails. The RNG is glibc's `random()` (TYPE_3), so the same `--seed` reproduces identical operation sequences to C fsx on Linux.

```
fstest fsx -N 100000 -l 262144 /mnt/workspace
```

Key options mirror upstream: `-N` ops, `-l` max file size, `-o` max op size, `-S` seed, `-r/-w/-t` alignment boundaries, `-L` lite (no size changes), `-F/-H/-z/-C/-I/-J/-E` to disable fallocate/punch/zero/collapse/insert/clone/copy, `-R/-W` to disable mapped reads/writes, `-X` full-file compare every op, `-y` fsync per write, `-e` post-EOF pollution, `-c P` close+open probability, `--duration`. Not ported: AIO/io_uring backends, O_DIRECT, atomic/dontcache IO variants, dedupe/exchange-range ioctls and the dmlogwrites integrity mode (the JSON report and replay file cover the record/replay need). Verification includes injected-failure detection tests and clean runs on APFS (where punch hole is block-aligned and clipped to EOF per platform requirement).

## fsstress

Behavior-level port of [fsstress](https://github.com/kdave/xfstests/blob/master/ltp/fsstress.c) from xfstests: randomized directory-tree stress mixing metadata and data operations. Like upstream (which forks children), each thread keeps its own file-entry lists and name sequence starting at 0 in the shared base directory, so cross-thread name collisions are tolerated by design; syscall errors are counted, not fatal. Upstream default frequencies are kept for the ported ops; Linux-ioctl-only and XFS/btrfs-specific ops are disabled.

```
fstest fsstress -n 10000 -p 4 /mnt/workspace          # 4 threads x 10000 ops
fstest fsstress -n 10000 -p 4 -R /mnt/workspace       # read-only workload
fstest fsstress -n 5000 -f creat=10 -f write=20 /mnt/workspace
fstest fsstress -n 10000 -p 4 -c /mnt/workspace       # clean up created files afterwards
```

| Option | Default | Description |
|---|---|---|
| `-n/--nops <N>` | 1000 | operations per thread |
| `-p/--nproc <N>` | 1 | threads |
| `-l/--loops <N>` | 1 | whole-run loops (0 = until `--duration`) |
| `-s/--seed <N>` | random | RNG seed (per-thread offset added) |
| `-f op=N` | upstream freqs | override an op frequency (repeatable); ops: creat write writev read readv mread mwrite mkdir mknod symlink rename truncate setfattr link unlink rmdir readlink stat getdents getfattr removefattr getattr fsync fdatasync sync fallocate punch zero collapse insert clonerange copyrange |
| `-R` / `-w` / `-z`-like | off | zero write-op freqs (read-only) / zero non-write freqs |
| `-c/--cleanup` | off | remove every file the run created |
| `--duration <SEC>` | 0 | time-based run |

The report carries per-thread op histograms and error counts. Concurrent truncate-vs-mmap races can SIGBUS a thread (upstream recovers per child via sigsetjmp, impossible across Rust threads); fstest installs a SIGBUS handler that emits a failed JSON report (ops/errors completed so far) and exits 1 instead of crashing silently. Because one such race would abort the whole run, the mmap ops `mread`/`mwrite` are disabled by default (upstream frequencies kept for every other op) and are opt-in via `-f mread=N` / `-f mwrite=N`.

## ltp

Adapter for the [Linux Test Project](https://github.com/linux-test-project/ltp) — the suite behind JuiceFS's official compatibility numbers. Suite selection maps to LTP's runtest files. The default suites are a narrowed subset of JuiceFS's published selection — `syscalls` (with JuiceFS's syscall removal list applied), `fs_perms_simple`, and `fcntl-locktests` — dropping `fs_bind` (95 bind-mount/mount-namespace shell tests that exercise kernel mount propagation, not the filesystem under test) and `smoketest` (mixes in process- and network-only tests):

```
sudo fstest ltp --ltp-dir /opt/ltp /mnt/jfs
sudo fstest ltp --suite syscalls,fs_bind,fs_perms_simple,smoketest,fcntl-locktests /mnt/jfs  # JuiceFS's full published set
```

LTP ships no prebuilt binaries — the release tarball is source-only (a few MB) and most distributions, Ubuntu included, have no `ltp` package — so the test binaries must be compiled once and pointed at with `--ltp-dir` (a tree containing `runtest/` and `testcases/bin/`, i.e. an LTP `make install` prefix). fstest embeds a prepare script that does the whole job on a fresh machine, so you can bootstrap from the fstest binary alone:

```
fstest ltp --prepare-script | sh                     # fetch + build + install LTP
LTP_PREFIX=/opt/ltp fstest ltp --prepare-script | sh # custom prefix
```

The script needs a C toolchain plus `bison flex m4` and `pkgconf` (header lists the apt/zypper/yum packages); it installs into `~/.cache/fstest/ltp/<version>` by default, prints the resulting prefix as the only thing on stdout (so it composes with `--ltp-dir "$(...)"`), and honours `LTP_VERSION`, `LTP_PREFIX`, `LTP_CACHE`, `LTP_URL`, `LTP_SHA256`, `LTP_JOBS` and `LTP_INSTALL_DEPS=1`. The same file lives at [scripts/prepare-ltp.sh](scripts/prepare-ltp.sh). Then point fstest at the prefix:

```
sudo fstest ltp --ltp-dir ~/.cache/fstest/ltp/20260529 /mnt/jfs
```

By default JuiceFS's published syscall removal list (vendored from their repo, 240+ tests their environment excludes) is applied, so `pass/fail` totals are directly comparable with their `1479 run / 1454 passed` numbers. LTP exit codes map: 0 pass, 1 fail, 2 broken, 32 skip, other warn; a per-test timeout (`--timeout`, default 120s) is reported as `timedout`. Tests run with `TMPDIR`/`LTP_TMPDIR` pointed inside the mount so file-based tests exercise it, and each test gets `LTPROOT` plus `testcases/bin` on `PATH` (as LTP's own runltp does) so shell and child-spawning tests resolve their helper libraries and binaries. The suite is executed by fstest itself — no external runner is involved. Root is required by most FS tests, exactly as upstream.

## stdfs

Runs the filesystem-relevant portions of the Go, Python, Node.js and Rust standard-library test suites against the mount. Where pjdfstest/LTP ask "does this filesystem conform to POSIX", stdfs asks "do everyday language runtimes work on it" — the suites exercise `openat`/`dir_fd` families, `copy_file_range`/`sendfile`, `O_TMPFILE`, `readdir` `d_type` reporting, recursive remove/copy and the exact syscall patterns real applications hit through their runtimes. Like ltp/fio, nothing is embedded or redistributed: each suite comes from the user's own toolchain and runs in place, with every language's scratch directories (`TMPDIR`, regrtest `TESTFN`, `NODE_TEST_DIR`) pointed inside the mount.

```
fstest stdfs /mnt/jfs
fstest stdfs --lang go,rust /mnt/jfs
fstest stdfs --python-dir ~/cpython-3.14/Lib --node-dir ~/node /mnt/jfs
```

| Language | Suite source (default) | Requirement |
|---|---|---|
| go | `go env GOROOT` of the `go` on PATH; packages `os`, `path/filepath`, `io/fs` | Go toolchain |
| python | the interpreter's bundled `test` package | a python3 that ships it (several distributions split it out); otherwise `--python-dir` |
| node | none — binary distributions never ship tests | `--node-dir` at a nodejs/node checkout (from-source trees are found automatically) |
| rust | `std/src/fs/tests.rs` from the rustc sysroot's rust-src component | `rustup component add rust-src`, or `--rust-src-dir` |

Languages whose toolchain or sources cannot be resolved are reported as `unavailable` and do not fail the run; real test failures do.

- go: each package is compiled with `go test -c` and the test binary runs with cwd and `TMPDIR` on the mount. Tests that read GOROOT-relative `testdata/` or glob the package's own source files (`TestReadFile`, `TestGlob`, the `DirFS` family) assume cwd equals the GOROOT package dir and are skipped by default; pass `--go-skip ""` to run them anyway. Tests run sequentially within a package (`-test.parallel=1`) because several of them enumerate the shared temp directory.
- python: `<python> -m test -v <modules>` with the FS-touching module set (os, posix, shutil, stat, fileio, scandir, glob, tempfile, mmap, fcntl). `--python-dir` plus `--python-exe` pin an arbitrary CPython version's `Lib/` tree against a matching interpreter. `test.test_os.ForkTests.test_fork` is ignored by default because its child python re-execs with `-I` and cannot see a PYTHONPATH-injected `test` package; add patterns via `--python-ignore`.
- node: `node --test --test-reporter=tap` over `test/parallel/test-fs-*.js` with the watch/inotify files excluded (event-notification semantics, not filesystem ones). Requires node >= 18.
- rust: fstest reads `library/std/src/fs/tests.rs`, rewrites `crate::` paths onto `std`, links the suite against a small shim replacing std's internal test helpers, and compiles a standalone runner with the same rustc on every run. Tests using std-internal or still-unstable APIs (fs::Dir/dirfd, read_buf/BorrowedBuf, free-function `fs::set_times`), windows-only cases and `#[ignore]` tests are excluded or skipped — the run is version-locked to the toolchain that provides the sources.

| Option | Default | Meaning |
|---|---|---|
| `--lang <LIST>` | go,python,node,rust | languages to run |
| `--go-dir <GOROOT>` | `go` on PATH | another Go installation's stdlib tests |
| `--go-packages <LIST>` | os,path/filepath,io/fs | packages to test |
| `--go-skip <REGEX>` | built-in list | `-test.skip` filter; empty string disables it |
| `--python-dir <DIR>` | interpreter stdlib | CPython `Lib/` directory whose `test/` package runs |
| `--python-exe <PATH>` | python3 on PATH, or one next to `--python-dir` | interpreter |
| `--python-modules <LIST>` | os,posix,shutil,... | test modules |
| `--python-ignore <LIST>` | test.test_os.ForkTests.test_fork | extra regrtest ignore patterns |
| `--node-dir <DIR>` | auto (from-source trees) | nodejs/node checkout root |
| `--node-filter <SUBSTR>` | none | only run matching test-fs files |
| `--rust-src-dir <DIR>` | sysroot rust-src | rust `library/` source directory |
| `--timeout <SEC>` | 600 | per-item timeout |
| `--jobs <N>` | 4 | concurrent items within a language |

## Platform notes

- fsync means plain `fsync(2)` everywhere. Rust's `File::sync_all()` maps to the much heavier `fcntl(F_FULLFSYNC)` on Apple targets, so fstest calls `fsync` directly; benchmark numbers stay comparable with the C/python originals.
- swift-put preallocation uses `posix_fallocate` on Linux and `fcntl(F_PREALLOCATE)` on macOS; the post-write `posix_fadvise(DONTNEED)` cache drop is Linux-only (macOS has no equivalent, it is skipped).
- `--pause` fixes an apparent upstream typo (smallfile gates the sleep on the constant `iterations % 5` instead of `filenum % 5`); fstest implements the per-file average pause.
- The exponential file-size distribution derives its random seed deterministically instead of smallfile's seed sidecar file, so separate `create`/`read` invocations replay the same sizes naturally.

## Licensing

fstest does not include or link upstream code; all ports are behavior-level reimplementations. Third-party notices and upstream license texts live in [licenses/](licenses/). The license of fstest itself is not chosen yet.
