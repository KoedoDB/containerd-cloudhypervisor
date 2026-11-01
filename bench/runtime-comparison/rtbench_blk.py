#!/usr/bin/env python3
"""The Kata variant with a raw block device: PostgreSQL's data on a logical volume that is given to the Kata container with `--device`, so that Kata hot-plugs it into the VM as a
virtio-blk disk (and not through virtio-fs, which is Kata's default for a directory). The entrypoint makes an ext4 on it (only if it has no file system) and mounts it inside the container; the
container gets CAP_SYS_ADMIN and no seccomp filter for that (the VM is the boundary).

  rtbench_blk.py probe            can a Kata container see the device (lsblk, /sys/block), which driver and cache mode does the VM use
  rtbench_blk.py pg [--direct]    the PostgreSQL run of rtbench.py (same settings) on that volume; --direct points Kata at a configuration with block_device_cache_direct = true (O_DIRECT on the disk)
  rtbench_blk.py nginx [--direct] the nginx run of rtbench.py (same settings) with the site's files on a block device: an ext4 made inside a Kata container, the files copied onto it
                                  from the same bytes that are baked into the image the other runtimes serve, mounted read-only by the entrypoint of the nginx container

Run after rtbench.py's own runs, with the benchmark's Docker started (20-start.sh) and the volume group (30-loopvg.sh). Results: pg-kata-clh-blk.json (+ -direct) in $RT_RESULTS.
"""
import json, os, sys, time
import rtbench as r

IMAGE = "rtbench-pg-e2fs"   # the PostgreSQL image plus e2fsprogs (the stock image has no mkfs.ext4): the only difference to the image of the other runs


KDIR = "/opt/kata/share/defaults/kata-containers/runtime-rs"


def set_kata_config(direct):
    """The shim takes the default configuration (`runtime-rs/configuration.toml`, a symlink) when it is not given another: this points it at the Cloud Hypervisor configuration as shipped,
    or at a copy of it with block_device_cache_direct = true (Kata's runtime-rs ignores the annotation io.katacontainers.config.hypervisor.block_device_cache_direct)."""
    src = f"{KDIR}/configuration-clh-runtime-rs.toml"
    dst = f"{KDIR}/configuration-clh-direct-runtime-rs.toml"
    if direct:
        r.sh(f"sudo sed 's/^block_device_cache_direct = false/block_device_cache_direct = true/' {src} | sudo tee {dst} >/dev/null")
    r.sh(f"sudo ln -sf {os.path.basename(dst if direct else src)} {KDIR}/configuration.toml")


def ensure_image():
    if r.dk("image", "inspect", IMAGE, check=False).returncode == 0:
        return
    ctx = f"{r.DATA}/build-pge2"
    r.sh(f"sudo mkdir -p {ctx} && sudo chown $(id -u) {ctx}")
    open(f"{ctx}/Dockerfile", "w").write(f"FROM {r.POSTGRES}\nRUN apt-get update -qq && apt-get install -y -qq --no-install-recommends e2fsprogs && rm -rf /var/lib/apt/lists/*\n")
    r.dk("build", "-q", "--network", "host", "-t", IMAGE, ctx, timeout=900)


LV = os.environ.get("RT_LVP", "") + ("kblk" if r.VG == "rtvg" else "pgdata")   # in a real group Kata and cchv share the one 8 GiB volume (one run at a time, zeroed before each)
DEV = f"/dev/{r.VG}/{LV}"
ENTRY = ('blkid -o value -s TYPE /dev/kblk >/dev/null 2>&1 || mkfs.ext4 -q -F /dev/kblk; mkdir -p /var/lib/postgresql; '
         'mount -o noatime /dev/kblk /var/lib/postgresql && exec docker-entrypoint.sh "$@"')
DIRECT = False


def ensure_lv():
    r.sh(f"sudo lvs {r.VG}/{LV} >/dev/null 2>&1 || sudo lvcreate -y -L 8G -n {LV} {r.VG} >/dev/null")


def wipe():
    r.sh(f"sudo blkdiscard -f --zeroout {DEV}")


def extra_args():
    a = ["--device", f"{DEV}:/dev/kblk", "--cap-add", "SYS_ADMIN", "--security-opt", "seccomp=unconfined", "--security-opt", "apparmor=unconfined"]
    return a


def start_pg(name, rtname, tag, limits=None):
    limits = limits or r.vm_limits(rtname, "pg")   # the same limits (and so the same VM size, checked by r.check_vm) as the virtio-fs run of this Kata variant
    ensure_image(); ensure_lv(); wipe()
    args = ["run", "-d", "--name", name, "--runtime", "kata-clh", "--cpus", limits[0], "-m", limits[1], "--memory-swap", limits[1], "--pids-limit", "2048",
            "--shm-size", "256m", "-e", "POSTGRES_HOST_AUTH_METHOD=trust", *extra_args(), "--entrypoint", "/bin/sh", IMAGE, "-c", ENTRY, "sh", "postgres", *r.PG_ARGS]
    t0 = time.time()
    r.dk(*args, timeout=300)
    return t0


def probe():
    set_kata_config(False)
    ensure_image(); ensure_lv(); wipe()
    r.services("kata-clh")
    out = {}
    for label, direct in (("default", False), ("direct", True)):
        global DIRECT
        DIRECT = direct
        res = r.dk("run", "--rm", "--runtime", "kata-clh", *extra_args(), "--entrypoint", "/bin/sh", IMAGE, "-c",
                   "ls -l /dev/kblk; cat /sys/block/sda/device/vendor /sys/block/sda/device/model 2>&1 | head -2; ls /sys/bus/virtio/drivers 2>&1 | tr '\\n' ' '; echo;  lsblk -o NAME,SIZE,TYPE,MOUNTPOINT 2>&1 | head; ls /sys/block; cat /sys/block/vd*/queue/rotational 2>/dev/null | head -2; "
                   "which mkfs.ext4 blkid; blkid -o value -s TYPE /dev/kblk; (mkfs.ext4 -q -F /dev/kblk && mount /dev/kblk /mnt && df -h /mnt | tail -1 && grep kblk /proc/mounts) 2>&1; "
                   "grep -E 'virtio|vd' /proc/partitions | head -4; cat /proc/mounts | grep -c virtiofs", check=False, timeout=300)
        out[label] = (res.stdout + res.stderr)[-1800:]
        print("=====", label, "\n", out[label])
    r.save("probe-kata-clh-blk", out)


WWW_LV = os.environ.get("RT_LVP", "") + "kblkwww"
WWW_DEV = f"/dev/{r.VG}/{WWW_LV}"


def prep_www():
    ensure_image()
    """An ext4 on a block device, made inside a Kata container (the device hot-plugged as virtio-blk) and filled with the three files of the nginx benchmark: the bytes are those of the
    image `rtbench-nginx` (copied out of it with docker cp), so every runtime serves the same files. The files are read back by a second container and compared."""
    r.sh(f"sudo lvs {r.VG}/{WWW_LV} >/dev/null 2>&1 || sudo lvcreate -y -L 1G -n {WWW_LV} {r.VG} >/dev/null")
    r.sh(f"sudo blkdiscard -f --zeroout {WWW_DEV}")
    src = f"{r.DATA}/www-src"
    r.sh(f"sudo rm -rf {src}; sudo mkdir -p {src}")
    r.dk("rm", "-f", "wwwsrc", check=False)
    r.dk("create", "--name", "wwwsrc", "rtbench-nginx")
    r.dk("cp", "wwwsrc:/usr/share/nginx/html/.", src)
    r.dk("rm", "-f", "wwwsrc")
    common = ["--runtime", "kata-clh", "--cpus", "1", "-m", "288m", "--memory-swap", "288m", "--device", f"{WWW_DEV}:/dev/kblk", "--cap-add", "SYS_ADMIN", "--security-opt", "seccomp=unconfined", "--security-opt", "apparmor=unconfined"]
    out = r.dk("run", "--rm", *common, "-v", f"{src}:/src:ro", "--entrypoint", "/bin/sh", IMAGE, "-c",
               "mkfs.ext4 -q -F /dev/kblk && mount /dev/kblk /mnt && cp -a /src/. /mnt/ && sync && umount /mnt && echo prepared").stdout.strip()
    want = r.sh(f"cd {src} && sudo sha256sum 1k.html 100k.bin 10m.bin").stdout
    got = r.dk("run", "--rm", *common, "--entrypoint", "/bin/sh", IMAGE, "-c", "mount -o ro /dev/kblk /mnt && cd /mnt && sha256sum 1k.html 100k.bin 10m.bin").stdout
    assert want == got, f"the files on the device differ from the image's:\n{want}\n{got}"
    return out


def start_nginx_blk(name, rtname):
    common = ["--device", f"{WWW_DEV}:/dev/kblk", "--cap-add", "SYS_ADMIN", "--security-opt", "seccomp=unconfined", "--security-opt", "apparmor=unconfined"]
    t0 = time.time()
    cpus, mem = r.vm_limits(rtname, "nginx")   # the same VM as cchv's and the virtio-fs run's: 2 vCPUs, 320 MiB (checked by r.check_vm)
    r.dk("run", "-d", "--name", name, "--runtime", "kata-clh", "--cpus", cpus, "-m", mem, "--memory-swap", mem, "--pids-limit", "512", *common,
         "--entrypoint", "/bin/sh", "rtbench-nginx", "-c", 'mount -o ro /dev/kblk /usr/share/nginx/html && exec nginx -g "daemon off;"', timeout=300)
    ip = r.container_ip(name)
    deadline = time.time() + 120
    while time.time() < deadline:
        if r.dk("exec", "wrkc", "wget", "-q", "-O", "/dev/null", f"http://{ip}/1k.html", check=False, timeout=30).returncode == 0:
            return time.time() - t0, ip
        time.sleep(0.02)
    raise RuntimeError("nginx on the block device did not answer")


def nginx_blk(direct):
    global DIRECT
    DIRECT = direct
    set_kata_config(direct)
    name = "kata-clh-blk" + ("-direct" if direct else "")
    r.RUNTIMES[name] = {"runtime": "kata-clh", "vol": "kblk"}
    r.services("kata-clh")
    print("prepared:", prep_www())
    r.nginx.__globals__["start_nginx"] = start_nginx_blk
    return r.nginx(name)


def ndensity_blk(direct, n):
    """N idle nginx containers on the block device, then under load (rtbench.nginx_density): the memory one instance costs, without virtio-fs."""
    global DIRECT
    DIRECT = direct
    set_kata_config(direct)
    name = "kata-clh-blk" + ("-direct" if direct else "")
    r.RUNTIMES[name] = {"runtime": "kata-clh", "vol": "kblk"}
    r.services("kata-clh")
    print("prepared:", prep_www())
    r.nginx_density.__globals__["start_nginx"] = start_nginx_blk
    return r.nginx_density(name, n)


def pg(direct):
    global DIRECT
    DIRECT = direct
    set_kata_config(direct)
    name = "kata-clh-blk" + ("-direct" if direct else "")
    r.RUNTIMES[name] = {"runtime": "kata-clh", "vol": "kblk"}
    r.start_pg = start_pg
    r.pg.__globals__["start_pg"] = start_pg
    res = r.pg(name)
    # r.pg saves as pg-<name>.json
    return res


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "probe":
        probe()
    elif len(sys.argv) > 1 and sys.argv[1] == "nginx":
        try:
            nginx_blk("--direct" in sys.argv)
        finally:
            r.dk("rm", "-f", "nginxsrv", check=False, timeout=300)
    elif len(sys.argv) > 2 and sys.argv[1] == "ndensity":
        try:
            ndensity_blk("--direct" in sys.argv, int(sys.argv[2]))
        finally:
            r.dk("rm", "-f", *[f"ndens{i}" for i in range(int(sys.argv[2]))], check=False, timeout=300)
    elif len(sys.argv) > 1 and sys.argv[1] == "pg":
        try:
            pg("--direct" in sys.argv)
        finally:
            r.dk("rm", "-f", "pgsrv", check=False, timeout=300)
    else:
        print(__doc__)
