#!/bin/sh
# prepare-ltp.sh — download, build and install the Linux Test Project (LTP)
# so that `fstest ltp --ltp-dir <PREFIX> <MOUNT>` has test binaries to drive.
#
# Why this is needed: LTP publishes no prebuilt test binaries — the release
# tarball is source only (ltp-full-*.tar.xz, ~3 MB) and most distributions
# (Ubuntu included) ship no `ltp` package. The suite must be compiled once
# before fstest can run it. This script does that in one shot.
#
# Progress goes to stderr; the only thing on stdout is the final prefix, so it
# is safe to capture. `fstest ltp --prepare-script` prints this same script.
#
# Usage:
#   fstest ltp --prepare-script | sh                     # install under ~/.cache/fstest/ltp/<ver>
#   fstest ltp --prepare-script | LTP_PREFIX=/opt/ltp sh # custom prefix
#   fstest ltp --prepare-script | sh -s -- --check       # only report missing deps
#
# Environment:
#   LTP_VERSION       release to build (default 20260529)
#   LTP_PREFIX        install prefix (default $LTP_CACHE/$LTP_VERSION)
#   LTP_CACHE         cache for tarball + source tree (default ~/.cache/fstest/ltp)
#   LTP_URL           tarball URL override (default: GitHub release for LTP_VERSION)
#   LTP_SHA256        expected sha256 of the tarball (default: no verification)
#   LTP_JOBS          parallel build jobs (default: nproc)
#   LTP_INSTALL_DEPS  set to 1 to apt-get the build deps (needs root, Debian/Ubuntu)
set -eu

say() { printf 'prepare-ltp: %s\n' "$*" >&2; }
die() { printf 'prepare-ltp: error: %s\n' "$*" >&2; exit 1; }

CHECK_ONLY=0
for arg in "$@"; do
    case "$arg" in
        --check|--check-only) CHECK_ONLY=1 ;;
        -h|--help)
            say "usage: prepare-ltp.sh [--check]"
            say "  fetch + build + install LTP; prints the --ltp-dir prefix on stdout"
            exit 0
            ;;
        *) die "unknown argument: $arg" ;;
    esac
done

LTP_VERSION="${LTP_VERSION:-20260529}"
LTP_CACHE="${LTP_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/fstest/ltp}"
LTP_PREFIX="${LTP_PREFIX:-$LTP_CACHE/$LTP_VERSION}"
LTP_URL="${LTP_URL:-https://github.com/linux-test-project/ltp/releases/download/$LTP_VERSION/ltp-full-$LTP_VERSION.tar.xz}"
SRC="$LTP_CACHE/ltp-full-$LTP_VERSION"
TARBALL="$LTP_CACHE/ltp-full-$LTP_VERSION.tar.xz"

jobs="${LTP_JOBS:-}"
if [ -z "$jobs" ]; then
    if command -v nproc >/dev/null 2>&1; then
        jobs=$(nproc)
    elif command -v getconf >/dev/null 2>&1; then
        jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)
    else
        jobs=1
    fi
fi

# --- already installed? -----------------------------------------------------
# Checked before the dependency probe: an existing install needs no toolchain.
if [ "$CHECK_ONLY" = "0" ] && [ -d "$LTP_PREFIX/runtest" ] && [ -d "$LTP_PREFIX/testcases/bin" ]; then
    say "LTP already installed at $LTP_PREFIX"
    printf '%s\n' "$LTP_PREFIX"
    exit 0
fi

# --- build dependencies -----------------------------------------------------
missing=""
for tool in cc make bison flex m4; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if ! command -v pkg-config >/dev/null 2>&1 && ! command -v pkgconf >/dev/null 2>&1; then
    missing="$missing pkg-config"
fi

if [ -n "$missing" ]; then
    say "missing build tools:$missing"
    if [ "${LTP_INSTALL_DEPS:-0}" = "1" ] && [ "$(id -u)" = "0" ] && command -v apt-get >/dev/null 2>&1; then
        say "installing build dependencies via apt-get"
        DEBIAN_FRONTEND=noninteractive apt-get update -qq
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
            gcc make pkgconf bison flex m4 libc6-dev libaio-dev libcap-dev \
            libnuma-dev uuid-dev libssl-dev
    else
        die "install them first, e.g.
  apt-get install -y gcc make pkgconf bison flex m4 libc6-dev libaio-dev libcap-dev libnuma-dev uuid-dev libssl-dev
  (or re-run with LTP_INSTALL_DEPS=1 as root)"
    fi
fi

if [ "$CHECK_ONLY" = "1" ]; then
    say "build dependencies present; --check done"
    exit 0
fi

mkdir -p "$LTP_CACHE"

# --- download ---------------------------------------------------------------
if [ ! -f "$TARBALL" ]; then
    say "downloading $LTP_URL"
    if command -v curl >/dev/null 2>&1; then
        curl -fL --retry 3 -o "$TARBALL.part" "$LTP_URL" || die "download failed"
    elif command -v wget >/dev/null 2>&1; then
        wget -O "$TARBALL.part" "$LTP_URL" || die "download failed"
    else
        die "need curl or wget to download the tarball"
    fi
    mv "$TARBALL.part" "$TARBALL"
fi

if [ -n "${LTP_SHA256:-}" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
        printf '%s  %s\n' "$LTP_SHA256" "$TARBALL" | sha256sum -c - >&2 || die "sha256 mismatch"
    elif command -v shasum >/dev/null 2>&1; then
        printf '%s  %s\n' "$LTP_SHA256" "$TARBALL" | shasum -a 256 -c - >&2 || die "sha256 mismatch"
    else
        say "no sha256sum/shasum; skipping LTP_SHA256 verification"
    fi
fi

# --- extract ----------------------------------------------------------------
if [ ! -d "$SRC" ]; then
    say "extracting $TARBALL"
    if ! tar -xJf "$TARBALL" -C "$LTP_CACHE" 2>/dev/null; then
        command -v xz >/dev/null 2>&1 || die "need tar with xz support, or the xz tool"
        xz -dc "$TARBALL" | tar -xf - -C "$LTP_CACHE" || die "extract failed"
    fi
fi
[ -x "$SRC/configure" ] || die "unexpected layout: $SRC/configure not found"

# --- build + install --------------------------------------------------------
say "configuring (prefix=$LTP_PREFIX)"
( cd "$SRC" && ./configure --prefix="$LTP_PREFIX" ) >&2 || die "configure failed"

say "building with -j$jobs (the slow part; a few optional tests may fail to build)"
if ! ( cd "$SRC" && make -j"$jobs" ) >&2; then
    say "make reported errors (usually optional/unsupported tests); continuing"
fi

say "installing"
if ! ( cd "$SRC" && make -k install ) >&2; then
    say "make install reported errors; verifying what landed"
fi

if [ ! -d "$LTP_PREFIX/runtest" ] || [ ! -d "$LTP_PREFIX/testcases/bin" ]; then
    die "install incomplete under $LTP_PREFIX (missing runtest/ or testcases/bin/)"
fi

n=$(ls "$LTP_PREFIX/testcases/bin" | wc -l | tr -d ' ')
say "installed $n files under $LTP_PREFIX/testcases/bin"
say "ready: fstest ltp --ltp-dir $LTP_PREFIX <mount>"
printf '%s\n' "$LTP_PREFIX"
