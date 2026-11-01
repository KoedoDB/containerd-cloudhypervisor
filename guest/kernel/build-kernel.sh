#!/usr/bin/env sh
#
# Build a minimal Linux kernel for Cloud Hypervisor microVMs.
#
# Based on Cloud Hypervisor's ch_defconfig with additions for:
# - virtio-fs (FUSE + virtiofs)
# - vsock (VIRTIO_VSOCK)
# - overlayfs (container layers)
# - cgroups v2 (resource limits)
# - namespaces (PID, mount, network, user)
# - ext4 filesystem
#
# Usage: ./build-kernel.sh [kernel_version]
#
set -euo pipefail

KERNEL_VERSION="${1:-6.18.60}"

KERNEL_MAJOR="${KERNEL_VERSION%%.*}"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/linux-${KERNEL_VERSION}.tar.xz"
KERNEL_DIR="linux-${KERNEL_VERSION}"
# Compilation is storage bound, not CPU bound - make sure we saturate all CPUs
NPROC="$(($(nproc)*2 + 1))"

# Select configs
HOST_ARCH="$(uname -m)"
CONFIG_FILE="configs/microvm.config"
CONFIG_ARCH="configs/microvm-${HOST_ARCH}.config"
if [ ! -f "${CONFIG_ARCH}" ] ; then
    echo "ERROR: unsupported architecture: ${HOST_ARCH}"
    exit 1
fi

echo -e "=== Building minimal kernel ${KERNEL_VERSION} (${HOST_ARCH}) ===\n"

# Download kernel source if not present
if [ ! -d "${KERNEL_DIR}" ]; then
    echo "Downloading kernel ${KERNEL_VERSION}..."
    # The tarball is checked against the pinned SHA-256 in hacks/checksums.txt (see docs/supply-chain.md).
    HACKS_DIR="$(cd "$(dirname "$0")/../../hacks" && pwd)"
    . "$HACKS_DIR/verify-download.sh"
    download_verified "${KERNEL_URL}" "linux-${KERNEL_VERSION}.tar.xz" "linux-${KERNEL_VERSION}.tar.xz"

    echo "Decompressing..."
    tar xf "linux-${KERNEL_VERSION}.tar.xz"
    rm -f "linux-${KERNEL_VERSION}.tar.xz"
fi

cd "${KERNEL_DIR}"

# Apply our config
cat "../${CONFIG_ARCH}" "../${CONFIG_FILE}" > .config
make olddefconfig

missing="$( \
  gawk -F = '/=[y]$/ {print "^" $1 "="}' ".config" | \
  grep -vf - ".config.old" | \
  gawk -F = '/=[y]$/ {print "    " $1}' | \
  sort)"
if [ -n "${missing}" ]; then
  echo -e "ERROR: The following options from '${CONFIG_FILE}' are not enabled in the final kernel configuration:\n${missing}"
  exit 1
fi
disabled="$( \
  gawk -F = '/=[n]$/ {print "^" $1 "="}' ".config.old" | \
  grep -f - ".config" | \
  gawk -F = '/=[y]$/ {print "    " $1}' | \
  sort || true)"
if [ -n "${disabled}" ]; then
  echo -e "ERROR: The following options are disabled in '${CONFIG_FILE}' but enabled in the final kernel configuration:\n${disabled}"
  exit 1
fi

# Hardening that must be in the result. The checks above only see the options this config asks for; an option that a dependency
# switches off (or that olddefconfig does not know in this kernel version) would be lost silently, so the ones that matter are
# listed here once more and the build stops if one is missing.
echo "Checking hardening options..."
hardening_missing=""
for opt in SECURITY SECURITY_YAMA SECURITY_LANDLOCK STACKPROTECTOR_STRONG HARDENED_USERCOPY SLAB_FREELIST_RANDOM \
           SLAB_FREELIST_HARDENED INIT_ON_ALLOC_DEFAULT_ON FORTIFY_SOURCE BPF_UNPRIV_DEFAULT_OFF SECCOMP_FILTER \
           CGROUP_BPF CGROUP_DEVICE BPF_SYSCALL; do
    grep -q "^CONFIG_${opt}=y" .config || hardening_missing="${hardening_missing} ${opt}=y"
done
for opt in DEVMEM DEVPORT PROC_KCORE IO_URING USERFAULTFD POSIX_MQUEUE MODULES KALLSYMS; do
    if grep -qE "^CONFIG_${opt}=(y|m)" .config; then
        hardening_missing="${hardening_missing} ${opt}=n"
    fi
done
if [ "${HOST_ARCH}" = "x86_64" ]; then
    grep -q "^CONFIG_LEGACY_VSYSCALL_NONE=y" .config || hardening_missing="${hardening_missing} LEGACY_VSYSCALL_NONE=y"
fi
if [ -n "${hardening_missing}" ]; then
    echo "ERROR: hardening options missing from the final kernel configuration:${hardening_missing}"
    exit 1
fi

echo "Building kernel with ${NPROC} jobs..."
make -j "${NPROC}" vmlinux

echo "Stripping kernel debug info..."
strip --strip-debug vmlinux 2>/dev/null || true

# Copy the kernel binary to a predictable location
cp vmlinux .config ../
KERNEL_SIZE=$(stat -c%s ../vmlinux 2>/dev/null || stat -f%z ../vmlinux)
echo "=== Kernel built: vmlinux ($(( KERNEL_SIZE / 1024 / 1024 )) MB) ==="
