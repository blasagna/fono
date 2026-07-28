#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-only
#
# Linker wrapper for local builds on distributions that ship a NON-PIC
# `libgomp.a` (Fedora, RHEL; Debian/Ubuntu build theirs with -fPIC, which
# is why CI never hits this).
#
# `llama-cpp-2/static-openmp` (see the root Cargo.toml) makes
# `llama-cpp-sys-2` bundle the system `libgomp.a` objects straight into its
# rlib. On Fedora those objects carry `R_X86_64_32S` relocations against
# `.rodata`, and rustc links Linux executables as PIE, so every `fono`
# binary and test binary dies with:
#
#   ld.bfd: libllama_cpp_sys_2-*.rlib(task.o): relocation R_X86_64_32S
#           against `.rodata' can not be used when making a PIE object
#
# The fix is `-no-pie` on the final link. It cannot be applied through
# RUSTFLAGS, because those also reach proc-macro crates, which link with
# `-shared` — and `gcc -shared -no-pie` drops the shared-object handling
# and tries to link an executable ("undefined reference to `main'").
#
# So we intercept at the linker instead and add `-no-pie` only to links
# that are NOT building a shared object. Usage:
#
#   export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=$PWD/scripts/cc-nopie.sh
#   cargo build -p fono
#
# This is a LOCAL DEV workaround only: it disables ASLR for the resulting
# binary. Release artefacts are built on Ubuntu runners and stay PIE.

for arg in "$@"; do
    if [ "$arg" = "-shared" ]; then
        exec "${FONO_REAL_CC:-cc}" "$@"
    fi
done

exec "${FONO_REAL_CC:-cc}" "$@" -no-pie
