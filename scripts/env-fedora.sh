# SPDX-License-Identifier: GPL-3.0-only
#
# Source this before building fono on Fedora / RHEL:
#
#   . scripts/env-fedora.sh
#   cargo build -p fono
#
# CI builds on Ubuntu runners, so two Fedora-specific things are not
# covered by `.github/workflows/ci.yml`:
#
#  1. Extra packages. Beyond the Debian list, Fedora needs:
#       sudo dnf install clang clang-devel libxkbcommon-devel \
#                        wayland-devel alsa-lib-devel libxdo-devel \
#                        libstdc++-static
#     `libstdc++-static` has no Debian counterpart (`build-essential`
#     covers it there) but is required: `llama-cpp-2/static-stdcxx` and
#     the static ONNX Runtime both link `libstdc++` statically.
#
#  2. A non-PIC `libgomp.a`. See scripts/cc-nopie.sh for the full
#     explanation — in short, `llama-cpp-2/static-openmp` bundles the
#     system libgomp objects into the llama-cpp-sys-2 rlib, Fedora builds
#     those without -fPIC, and rustc links executables as PIE, so every
#     fono binary and test binary fails to link. The wrapper below adds
#     `-no-pie` to executable links only.

_fono_root=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)

# Link the pinned minimal static ONNX Runtime instead of letting ort-sys
# download the ~19 MiB CDN build (see ADR 0032, docs/binary-size.md).
ORT_LIB_LOCATION=$(sh "$_fono_root/scripts/fetch-onnxruntime.sh") || return 1
export ORT_LIB_LOCATION

# Local dev only: the resulting binaries are non-PIE (no ASLR). Release
# artefacts are built on Ubuntu, whose libgomp.a is PIC, and stay PIE.
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$_fono_root/scripts/cc-nopie.sh"

unset _fono_root
