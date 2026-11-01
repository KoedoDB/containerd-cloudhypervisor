# Third-party software in a release

The code of this repository is MIT ([LICENSE](LICENSE)). A release also carries software of others. This file lists what, under which
license, and where the source is. The versions are the pins in [hacks/versions.env](hacks/versions.env); the SHA-256 values are the
ones in [hacks/checksums.txt](hacks/checksums.txt) (see [docs/supply-chain.md](docs/supply-chain.md) for where each comes from).

Each release attaches:

| File | What it is |
|---|---|
| `THIRD-PARTY-LICENSES.txt` | the license texts of the **Rust crates** compiled into the shim, the daemon and the agent (made by `cargo about` from `Cargo.lock`; the build fails on a crate whose license is not in [about.toml](about.toml)) |
| `sbom-containerd-shim-cloudhv-v1.cdx.json`, `sbom-cloudhv-sandbox-daemon.cdx.json`, `sbom-cloudhv-agent.cdx.json` | CycloneDX SBOMs of the three binaries' crates (`cargo cyclonedx`, from `Cargo.lock`) |
| `sbom-bundled-components.cdx.json` | CycloneDX SBOM of what is bundled and is not a crate: Cloud Hypervisor, the CNI plugins, crun, the Linux kernel (with the SHA-256 of what the build downloads) |

## What is bundled, and under which license

| Component | Where it ends up | License | Source |
|---|---|---|---|
| Cloud Hypervisor v54.0 | the system extension (`containerd-cloudhypervisor-*.raw`) and the installer image | Apache-2.0 (the main program), BSD-3-Clause for some files; see `LICENSES/` in its repository | https://github.com/cloud-hypervisor/cloud-hypervisor/tree/v54.0 |
| CNI plugins v1.9.1 | the system extension | Apache-2.0 | https://github.com/containernetworking/plugins/tree/v1.9.1 |
| **crun 1.30.1** | the guest root file system (`rootfs-*.erofs`), as a static binary | **GPL-2.0-or-later** (`COPYING`); libcrun under LGPL-2.1 (`COPYING.libcrun`) | see below |
| **Linux 6.18.60** | the guest kernel (`vmlinux-*`) | **GPL-2.0-only** with the Linux-syscall-note | see below |
| erofs-utils (`mkfs.erofs`) | used to build the root file system; not shipped | GPL-2.0+ / Apache-2.0 | https://github.com/erofs/erofs-utils |
| Rust crates | the shim, the daemon, the agent | MIT, Apache-2.0, Zlib, Unicode-3.0 (the list is in `THIRD-PARTY-LICENSES.txt`) | crates.io, as in `Cargo.lock` (checksummed) |
| `containerd-shimkit` 0.1.2-koedodb | compiled into the shim | Apache-2.0 | https://github.com/KoedoDB/containerd-shimkit (the commit is in `Cargo.lock` and in `--version`) |

## Source for the GPL components (written offer)

The two GPL components are distributed as binaries inside the release files. Their corresponding source:

- **crun 1.30.1** (the binary is the upstream release's static `disable-systemd` build, checked against its signature and
  `hacks/checksums.txt`; we do not modify it):
  - source tarball: https://github.com/containers/crun/releases/download/1.30.1/crun-1.30.1.tar.gz
    (SHA-256 `99d036d95113552b6df66c7c879432835504bf0e525fc0372745071df129527b`, computed on 2026-10-17 from one download; **not** checked
    against a publisher's value),
  - git: https://github.com/containers/crun, tag `1.30.1` (the tag object is `0f574733bdd1566a91f092867feb6b6267099094`, the commit it
    points to is `079ff6a7a16d029460af882f9ea44674e6a510b6`).
- **Linux 6.18.60** (built by `guest/kernel/build-kernel.sh` from the upstream tarball with our configuration, no patches):
  - source tarball: https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.18.60.tar.xz
    (SHA-256 `5bd9acdc82ec5efc3790ae840e5aaa40dfde7efff261cf6dde902dc570acf98c`, equal to kernel.org's signed `sha256sums.asc`),
  - our configuration: [guest/kernel/configs/](guest/kernel/configs/) in this repository at the release's tag; each release also attaches the
    resulting `vmlinux-*.kconfig`,
  - the build scripts: [guest/kernel/build-kernel.sh](guest/kernel/build-kernel.sh) at the release's tag.

If those links ever stop working, write to **security@koedodb.com** (subject "GPL source") and we will send the source for the
versions in a release for at least three years after the release.
