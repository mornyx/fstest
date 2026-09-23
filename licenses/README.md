# Third-party notices

Community test tools that fstest references or plans to reference. fstest does not include or link any source code from these projects: smallfile / fs_mark / mdtest / md-workbench are behavior-level ports (reimplemented from documentation and observed black-box behavior), fio / ior / ltp are invoked as standalone binaries provided by the user's environment, pjdfstest's BSD-2-Clause shell test suite is vendored and embedded verbatim with its license, and the stdfs adapter executes the Go / CPython / Node.js / Rust standard-library test suites shipped with the user's own toolchains in place. Upstream license texts are collected here for attribution.

| Project | Upstream | License | Copyright |
|---|---|---|---|
| smallfile | https://github.com/distributed-system-analysis/smallfile | Apache-2.0 | Ben England |
| fs_mark | https://github.com/josefbacik/fs_mark | GPL-2.0-or-later | EMC Corporation (Ric Wheeler) |
| ior / mdtest / md-workbench | https://github.com/hpc/ior | GPL-2.0-only | Lawrence Livermore National Laboratory |
| fio | https://github.com/axboe/fio | GPL-2.0 | Jens Axboe |
| pjdfstest | https://github.com/pjd/pjdfstest | BSD-2-Clause | Pawel Jakub Dawidek |
| xfstests (fsx, fsstress) | https://github.com/kdave/xfstests | GPL-2.0 | Silicon Graphics, Oracle and others |
| LTP (adapter target) | https://github.com/linux-test-project/ltp | GPL-2.0-or-later | LTP contributors |
| JuiceFS removal list | https://github.com/juicedata/juicefs | Apache-2.0 | JuiceFS Authors |
| Go standard library tests (stdfs target) | https://github.com/golang/go | BSD-3-Clause | The Go Authors |
| CPython test suite (stdfs target) | https://github.com/python/cpython | PSF-2.0 | Python Software Foundation |
| Node.js test suite (stdfs target) | https://github.com/nodejs/node | MIT | Node.js contributors |
| Rust standard library tests (stdfs target) | https://github.com/rust-lang/rust | MIT OR Apache-2.0 | The Rust Project Developers |

License texts:

- [smallfile.LICENSE-Apache-2.0](smallfile.LICENSE-Apache-2.0)
- [fs_mark.COPYING-GPL-2.0](fs_mark.COPYING-GPL-2.0)
- [ior.COPYRIGHT-GPL-2.0](ior.COPYRIGHT-GPL-2.0)
- [fio.COPYING-GPL-2.0](fio.COPYING-GPL-2.0)
- [xfstests.COPYING-GPL-2.0](xfstests.COPYING-GPL-2.0)
- [pjdfstest.COPYING-BSD-2](pjdfstest.COPYING-BSD-2)
