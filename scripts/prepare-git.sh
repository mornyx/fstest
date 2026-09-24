#!/bin/sh
# prepare-git.sh — clone and build a pinned git release so that
# `fstest git --git-dir <TREE> <MOUNT>` has a same-source git binary and t/
# suite to run against a filesystem.
#
# Why a build tree and not vendored tests: git's t/ suite is version-locked to
# the binary it ships with (the scripts exercise their own version's options
# and usage text — running them against any other git build produces
# version-drift failures that have nothing to do with the filesystem under
# test). One checkout provides both, and fstest runs the scripts straight out
# of the tree, so nothing is installed and nothing is vendored into fstest.
#
# The build is deliberately minimal — NO_CURL NO_EXPAT NO_GETTEXT NO_TCLTK —
# so the only hard dependencies are a C toolchain, make, git and zlib.
# Network transports and i18n are out of scope for a filesystem suite; the
# test suite's own prereq gating (driven by GIT-BUILD-OPTIONS) turns those
# tests into skips rather than failures.
#
# Progress goes to stderr; the only thing on stdout is the build-tree path, so
# it composes with --git-dir:
#   fstest git --git-dir "$(fstest git --prepare-script | sh)" <mount>
#
# Usage:
#   fstest git --prepare-script | sh                       # build under ~/.cache/fstest/git/<tag>
#   GIT_VERSION=v2.52.0 fstest git --prepare-script | sh   # another tag
#   fstest git --prepare-script | sh -s -- --check         # only report missing deps
#
# Environment:
#   GIT_VERSION   tag to build (default v2.55.0)
#   GIT_CACHE     cache for the source + build tree (default ~/.cache/fstest/git)
#   GIT_REPO      repository to clone (default https://github.com/git/git)
#   GIT_JOBS      parallel build jobs (default: nproc)
#   GIT_MAKE_OPTS extra make variables/flags appended to every make call
set -eu

say() { printf 'prepare-git: %s\n' "$*" >&2; }
die() { printf 'prepare-git: error: %s\n' "$*" >&2; exit 1; }

CHECK_ONLY=0
for arg in "$@"; do
    case "$arg" in
        --check|--check-only) CHECK_ONLY=1 ;;
        -h|--help)
            say "usage: prepare-git.sh [--check]"
            say "  clone + build a pinned git release; prints the --git-dir tree on stdout"
            exit 0
            ;;
        *) die "unknown argument: $arg" ;;
    esac
done

GIT_VERSION="${GIT_VERSION:-v2.55.0}"
GIT_CACHE="${GIT_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/fstest/git}"
GIT_REPO="${GIT_REPO:-https://github.com/git/git}"
SRC="$GIT_CACHE/$GIT_VERSION"

jobs="${GIT_JOBS:-}"
if [ -z "$jobs" ]; then
    if command -v nproc >/dev/null 2>&1; then
        jobs=$(nproc)
    elif command -v getconf >/dev/null 2>&1; then
        jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)
    else
        jobs=1
    fi
fi

# --- already built? ----------------------------------------------------------
# Checked before the dependency probe: an existing build needs no toolchain.
if [ "$CHECK_ONLY" = "0" ] && [ -x "$SRC/git" ] && [ -f "$SRC/t/test-lib.sh" ] && [ -x "$SRC/t/helper/test-tool" ]; then
    say "git $GIT_VERSION already built at $SRC"
    printf '%s\n' "$SRC"
    exit 0
fi

# --- build dependencies ------------------------------------------------------
missing=""
for tool in cc make git; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if ! printf '#include <zlib.h>\nint main(void){return 0;}\n' | ${CC:-cc} -x c - -o /dev/null >/dev/null 2>&1; then
    missing="$missing zlib-headers"
fi

if [ -n "$missing" ]; then
    say "missing build tools:$missing"
    die "install them first, e.g.
  apt-get install -y build-essential git zlib1g-dev   # Debian/Ubuntu
  zypper install -y gcc make git zlib-devel           # openSUSE
  yum install -y gcc make git zlib-devel              # RHEL/Fedora
  xcode-select --install                              # macOS (zlib ships with the system)"
fi

if [ "$CHECK_ONLY" = "1" ]; then
    say "build dependencies present; --check done"
    exit 0
fi

mkdir -p "$GIT_CACHE"

# --- clone -------------------------------------------------------------------
# Shallow at the tag. Cloned to a scratch name and renamed into place so a
# failed clone never leaves a half tree that the idempotency check would
# accept on the next run.
if [ ! -x "$SRC/git" ]; then
    rm -rf "$SRC.tmp"
    say "cloning $GIT_REPO at $GIT_VERSION (shallow)"
    git clone --depth 1 --branch "$GIT_VERSION" "$GIT_REPO" "$SRC.tmp" >&2 \
        || die "clone failed (is GIT_VERSION=$GIT_VERSION a valid tag in $GIT_REPO?)"
    mv "$SRC.tmp" "$SRC"
fi
[ -f "$SRC/t/test-lib.sh" ] || die "unexpected layout: $SRC/t/test-lib.sh not found"

# --- build -------------------------------------------------------------------
# perl builds a few scripted helpers some tests use; without it the build
# needs NO_PERL and those tests skip through prereq gating.
PERL_FLAG=""
command -v perl >/dev/null 2>&1 || PERL_FLAG="NO_PERL=YesPlease"

say "building with -j$jobs (NO_CURL NO_EXPAT NO_GETTEXT NO_TCLTK${PERL_FLAG:+ $PERL_FLAG})"
( cd "$SRC" && make -j"$jobs" NO_CURL=YesPlease NO_EXPAT=YesPlease NO_GETTEXT=YesPlease \
    NO_TCLTK=YesPlease $PERL_FLAG ${GIT_MAKE_OPTS:-} all ) >&2 || die "build failed"

# `make all` builds t/helper/test-tool on current versions; verify rather than
# assume, and fall back to the helpers' own Makefile if it did not land.
if [ ! -x "$SRC/t/helper/test-tool" ]; then
    say "building the test helpers (t/helper)"
    ( cd "$SRC/t/helper" && make -j"$jobs" ${GIT_MAKE_OPTS:-} ) >&2 || die "test helper build failed"
fi

if [ ! -x "$SRC/git" ] || [ ! -x "$SRC/t/helper/test-tool" ]; then
    die "build incomplete under $SRC (missing git or t/helper/test-tool)"
fi

say "ready: fstest git --git-dir $SRC <mount>"
printf '%s\n' "$SRC"
