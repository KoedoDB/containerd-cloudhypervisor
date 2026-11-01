//! A delegated cgroup subtree for one container (Pod annotation `cloudhv.koedodb.com/cgroup-delegate`, daemon key
//! `allow_cgroup_delegate`).
//!
//! A container marked by name gets, inside the VM, a cgroup it may subdivide: it can make child cgroups and set `memory.max`, `cpu.max`
//! and `pids.max` on them for the processes it starts. Nothing else changes, and the container gets no capability from this (it loses
//! some, see [`STRIPPED_CAPS`]). Two cgroups are made, following the kernel's rules for delegation
//! (Documentation/admin-guide/cgroup-v2.rst, "Delegation"):
//!
//! ```text
//! /sys/fs/cgroup/<id>      the PARENT: the platform's limits (memory.max, cpu.max, pids.max, cpu.weight); every file root's; the
//!                          container cannot see it, name it or write it
//! /sys/fs/cgroup/<id>/d    the delegated CHILD: no limits of its own; the container's cgroup namespace root, where its process starts
//! ```
//!
//! 1. The limits of the Pod are written by the agent to the parent, and taken out of the spec crun gets, so that crun puts the container
//!    in `<id>/d` (`cgroupsPath`) and leaves it without limits. The kernel enforces the limit of every ancestor on all its descendants:
//!    whatever the container writes into `d` or below it can lower what the container may use, never raise it above the parent's.
//!    (A limit placed on `d` itself would not do: a root container with `CAP_SETUID` can become the owner of a file that is not its own.)
//! 2. `d`'s directory and `cgroup.procs`, `cgroup.threads`, `cgroup.subtree_control` belong to the container's user; its other files
//!    belong to `nobody`. The controllers `cpu`, `memory` and `pids` are enabled in the parent for `d`, and at the cgroup root at boot.
//! 3. The container is in a cgroup namespace whose root is `d`, and sees a cgroup2 file system that shows only `d`: no parent, no
//!    sibling. The guest mounts cgroup2 with `nsdelegate`, so the kernel also treats the namespace root as a delegation boundary (a
//!    process cannot be moved across it, from either side). The device rules (eBPF) are attached by crun to `d`, which only grows down.
//! 4. `CAP_DAC_OVERRIDE` and `CAP_FOWNER` are removed from a delegated container, so that root in it cannot override the
//!    permissions of files that are not its own. `CAP_SETUID`/`CAP_SETGID` may stay (point 1 does not depend on them).
//!
//! The container's first process starts in `d` itself. The kernel's "no internal processes" rule means it must move into a leaf child
//! (`mkdir leaf; echo $$ > leaf/cgroup.procs`) before it writes `+memory` to `cgroup.subtree_control`.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Where the guest mounts the cgroup2 file system.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// The name of the delegated child of the container's cgroup.
pub const CHILD: &str = "d";
/// The owner of the files the container must not write.
pub const NOBODY: u32 = 65534;
/// The annotation containerd sets on every container (a Pod's own annotations cannot override it) with the name from the Pod's spec.
const NAME_ANNOTATION: &str = "io.kubernetes.cri.container-name";
/// The files the container owns (the ones the kernel itself hands to the user of a delegated cgroup).
const DELEGATED_FILES: &[&str] = &["cgroup.procs", "cgroup.threads", "cgroup.subtree_control"];
/// Capabilities taken from a delegated container: they would let root ignore the permissions of files that are not its own (the
/// files of `d` that belong to `nobody`) or change their permissions. `CAP_CHOWN` stays (v0.6.1): a container that must hand files to
/// other users (the Functions spawner: the run directory and the socket of each customer's process) needs it, and it breaks nothing
/// here: the limits sit on the parent, which the container cannot see, and a root with `CAP_SETUID` can already become any owner.
pub const STRIPPED_CAPS: &[&str] = &["CAP_DAC_OVERRIDE", "CAP_FOWNER"];

/// Whether this container is one the Pod's annotation names. The annotation holds container names, separated by commas. A container
/// without a name (a client that is not Kubernetes) is never delegated to, and neither is one the list does not name.
pub fn requested(spec: &Value) -> bool {
    let annotations = spec.get("annotations");
    let list = annotations
        .and_then(|a| a.get(cloudhv_common::CGROUP_DELEGATE_ANNOTATION))
        .and_then(|v| v.as_str());
    let name = annotations
        .and_then(|a| a.get(NAME_ANNOTATION))
        .and_then(|v| v.as_str());
    match (list, name) {
        (Some(list), Some(name)) if !name.is_empty() => {
            list.split(',').map(str::trim).any(|n| n == name)
        }
        _ => false,
    }
}

/// The user and group of the container's process (what the files of the delegated cgroup are given to), from the cleaned spec.
pub fn identity(clean: &Value) -> (u32, u32) {
    let id = |k: &str| {
        clean
            .pointer(&format!("/process/user/{k}"))
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0)
    };
    (id("uid"), id("gid"))
}

/// What the agent does to the cgroup file system, so that the logic can be tested on a directory that is not one.
pub trait CgroupOps {
    fn chown(&mut self, path: &Path, uid: u32, gid: u32) -> io::Result<()>;
    fn write(&mut self, path: &Path, value: &str) -> io::Result<()>;
}

struct RealOps;

impl CgroupOps for RealOps {
    fn chown(&mut self, path: &Path, uid: u32, gid: u32) -> io::Result<()> {
        std::os::unix::fs::chown(path, Some(uid), Some(gid))
    }
    fn write(&mut self, path: &Path, value: &str) -> io::Result<()> {
        std::fs::write(path, value)
    }
}

/// The platform's limits from the cleaned spec as (file of the parent, value), and taken out of the spec (the device rules stay).
/// OCI `cpu.shares` becomes `cpu.weight` as crun converts it.
pub fn take_limits(clean: &mut Value) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let Some(res) = clean
        .pointer_mut("/linux/resources")
        .and_then(|r| r.as_object_mut())
    else {
        return out;
    };
    if let Some(m) = res
        .remove("memory")
        .and_then(|m| m.get("limit").and_then(|l| l.as_i64()))
    {
        out.push(("memory.max", m.to_string()));
    }
    if let Some(cpu) = res.remove("cpu") {
        if let (Some(q), Some(p)) = (
            cpu.get("quota").and_then(|v| v.as_i64()),
            cpu.get("period").and_then(|v| v.as_u64()),
        ) {
            out.push(("cpu.max", format!("{q} {p}")));
        }
        if let Some(sh) = cpu.get("shares").and_then(|v| v.as_u64()) {
            let weight = 1 + sh.clamp(2, 262144).saturating_sub(2) * 9999 / 262142;
            out.push(("cpu.weight", weight.to_string()));
        }
    }
    if let Some(p) = res
        .remove("pids")
        .and_then(|p| p.get("limit").and_then(|l| l.as_i64()))
    {
        out.push(("pids.max", p.to_string()));
    }
    out
}

/// Make the parent `<id>` with the limits and the delegated child `<id>/d` under `root`, and hand out the child's files as described
/// above. Returns the child. On an error nothing is left behind. The parent's files stay as the kernel made them (root's).
pub fn prepare_dir(
    root: &Path,
    id: &str,
    uid: u32,
    gid: u32,
    limits: &[(&str, String)],
    ops: &mut dyn CgroupOps,
) -> Result<PathBuf> {
    // The id is a plain word already (`valid_container_id`); this is the last line against a path that leaves `root`.
    if id.is_empty() || id.contains('/') || id.starts_with('.') {
        anyhow::bail!("bad container id for a cgroup");
    }
    let parent = root.join(id);
    let child = parent.join(CHILD);
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("make cgroup {}", parent.display()))?;
    let result = (|| -> Result<()> {
        for (file, value) in limits {
            ops.write(&parent.join(file), value)
                .with_context(|| format!("write {file} of the cgroup"))?;
        }
        // The parent holds no process, so it may hand its controllers to `d`.
        ops.write(&parent.join("cgroup.subtree_control"), "+cpu +memory +pids")
            .context("enable controllers for the delegated cgroup")?;
        std::fs::create_dir_all(&child)
            .with_context(|| format!("make cgroup {}", child.display()))?;
        hand_out(&child, uid, gid, ops)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir(&child);
        let _ = std::fs::remove_dir(&parent);
    }
    result.map(|()| child)
}

fn hand_out(dir: &Path, uid: u32, gid: u32, ops: &mut dyn CgroupOps) -> Result<()> {
    // Every file first (to nobody), then the three that are the container's, and the directory last.
    for entry in std::fs::read_dir(dir).context("read the cgroup")? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let (u, g) = if DELEGATED_FILES.iter().any(|d| name == *d) {
            (uid, gid)
        } else {
            (NOBODY, NOBODY)
        };
        ops.chown(&entry.path(), u, g)
            .with_context(|| format!("chown {}", entry.path().display()))?;
    }
    ops.chown(dir, uid, gid)
        .context("chown the cgroup directory")?;
    Ok(())
}

/// The real thing, under [`CGROUP_ROOT`]. Takes the limits out of the spec crun gets and points `cgroupsPath` at the child.
pub fn prepare(id: &str, clean: &mut Value) -> Result<PathBuf> {
    let (uid, gid) = identity(clean);
    let limits = take_limits(clean);
    let dir = prepare_dir(Path::new(CGROUP_ROOT), id, uid, gid, &limits, &mut RealOps)?;
    if let Some(l) = clean.get_mut("linux").and_then(|l| l.as_object_mut()) {
        l.insert("cgroupsPath".into(), json!(format!("/{id}/{CHILD}")));
    }
    Ok(dir)
}

/// After `crun delete` has removed the child: remove the parent too (best effort; the id is a plain word).
pub fn remove_parent(id: &str) {
    if !id.is_empty() && !id.contains('/') && !id.starts_with('.') {
        let _ = std::fs::remove_dir(Path::new(CGROUP_ROOT).join(id));
    }
}

/// Change the cleaned spec of a delegated container: a cgroup namespace (its root is the container's cgroup, which `crun` has already
/// entered), a cgroup2 mount that shows that root, and no `CAP_DAC_OVERRIDE`, `CAP_FOWNER`.
pub fn apply(clean: &mut Value) {
    if let Some(ns) = clean
        .pointer_mut("/linux/namespaces")
        .and_then(|n| n.as_array_mut())
    {
        if !ns.iter().any(|n| n["type"] == "cgroup") {
            ns.push(json!({"type": "cgroup"}));
        }
    }
    if let Some(m) = clean.get_mut("mounts").and_then(|m| m.as_array_mut()) {
        m.push(
            json!({"destination": CGROUP_ROOT, "type": "cgroup2", "source": "cgroup2",
            "options": ["nosuid", "noexec", "nodev"]}),
        );
    }
    if let Some(caps) = clean
        .pointer_mut("/process/capabilities")
        .and_then(|c| c.as_object_mut())
    {
        for set in caps.values_mut() {
            if let Some(list) = set.as_array_mut() {
                list.retain(|c| !STRIPPED_CAPS.iter().any(|s| c == s));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(list: Option<&str>, name: Option<&str>) -> Value {
        let mut a = serde_json::Map::new();
        if let Some(l) = list {
            a.insert(cloudhv_common::CGROUP_DELEGATE_ANNOTATION.into(), json!(l));
        }
        if let Some(n) = name {
            a.insert(NAME_ANNOTATION.into(), json!(n));
        }
        json!({ "annotations": a })
    }

    #[test]
    fn only_the_named_container_is_marked() {
        assert!(requested(&spec(Some("spawner"), Some("spawner"))));
        assert!(requested(&spec(Some("a, spawner ,b"), Some("spawner"))));
        // Another container of the Pod, no name, no annotation, an empty list, a name that only contains the word.
        assert!(!requested(&spec(Some("spawner"), Some("sidecar"))));
        assert!(!requested(&spec(Some("spawner"), None)));
        assert!(!requested(&spec(Some("spawner"), Some(""))));
        assert!(!requested(&spec(None, Some("spawner"))));
        assert!(!requested(&spec(Some(""), Some("spawner"))));
        assert!(!requested(&spec(Some("spawner2"), Some("spawner"))));
        assert!(!requested(&json!({})));
    }

    /// A fake cgroup root: a directory with the files a new cgroup has (the test makes them where the "kernel" would).
    fn fake_root(parent_files: &[&str], child_files: &[&str]) -> tempdir::Dir {
        let root = tempdir::Dir::new();
        let parent = root.path().join("c1");
        let child = parent.join(CHILD);
        std::fs::create_dir_all(&child).unwrap();
        for f in parent_files {
            std::fs::write(parent.join(f), "").unwrap();
        }
        for f in child_files {
            std::fs::write(child.join(f), "").unwrap();
        }
        std::fs::create_dir(child.join("sub")).unwrap();
        root
    }

    const FILES: &[&str] = &[
        "cgroup.procs",
        "cgroup.threads",
        "cgroup.subtree_control",
        "cgroup.type",
        "cgroup.kill",
        "cgroup.freeze",
        "cgroup.max.depth",
        "memory.max",
        "memory.high",
        "memory.swap.max",
        "memory.min",
        "memory.low",
        "cpu.max",
        "cpu.weight",
        "pids.max",
    ];

    #[derive(Default)]
    struct Recorder {
        chowns: Vec<(PathBuf, u32, u32)>,
        writes: Vec<(PathBuf, String)>,
        fail_chown: bool,
    }

    impl CgroupOps for Recorder {
        fn chown(&mut self, p: &Path, u: u32, g: u32) -> io::Result<()> {
            if self.fail_chown {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            self.chowns.push((p.to_path_buf(), u, g));
            Ok(())
        }
        fn write(&mut self, p: &Path, v: &str) -> io::Result<()> {
            self.writes.push((p.to_path_buf(), v.to_string()));
            Ok(())
        }
    }

    fn limits() -> Vec<(&'static str, String)> {
        vec![
            ("memory.max", "268435456".into()),
            ("cpu.max", "100000 100000".into()),
            ("pids.max", "512".into()),
        ]
    }

    #[test]
    fn the_parent_keeps_the_limits_and_is_never_handed_out() {
        let root = fake_root(FILES, FILES);
        let mut rec = Recorder::default();
        let child = prepare_dir(root.path(), "c1", 1000, 1001, &limits(), &mut rec).unwrap();
        let parent = root.path().join("c1");
        assert_eq!(child, parent.join("d"));
        // The limits are written to the parent, and nowhere else; so is the delegation of the controllers.
        for (f, v) in limits() {
            assert!(rec.writes.contains(&(parent.join(f), v)), "{f}");
        }
        assert!(rec.writes.contains(&(
            parent.join("cgroup.subtree_control"),
            "+cpu +memory +pids".into()
        )));
        assert!(rec.writes.iter().all(|(p, _)| p.parent() == Some(&parent)));
        // Neither the parent nor any of its files is chowned: they stay root's, and the container never gets a path to them.
        assert!(rec.chowns.iter().all(|(p, _, _)| p.starts_with(&child)));
        assert!(!rec.chowns.iter().any(|(p, _, _)| *p == parent));
    }

    #[test]
    fn the_child_has_three_files_and_the_directory_for_the_container_and_nothing_else() {
        let root = fake_root(FILES, FILES);
        let mut rec = Recorder::default();
        let child = prepare_dir(root.path(), "c1", 1000, 1001, &limits(), &mut rec).unwrap();
        let owner = |name: &str| {
            let hit: Vec<_> = rec
                .chowns
                .iter()
                .filter(|c| c.0 == child.join(name))
                .collect();
            assert_eq!(hit.len(), 1, "{name} is changed exactly once");
            (hit[0].1, hit[0].2)
        };
        for f in DELEGATED_FILES {
            assert_eq!(owner(f), (1000, 1001), "{f}");
        }
        for f in FILES.iter().filter(|f| !DELEGATED_FILES.contains(f)) {
            assert_eq!(owner(f), (NOBODY, NOBODY), "{f}");
        }
        // The sub directory is left alone, and the child directory is the last thing changed.
        assert!(!rec.chowns.iter().any(|c| c.0 == child.join("sub")));
        let last = rec.chowns.last().unwrap();
        assert_eq!((&last.0, last.1, last.2), (&child, 1000, 1001));
    }

    #[test]
    fn a_root_container_gets_nobody_for_the_child_limit_files_too() {
        let root = fake_root(FILES, FILES);
        let mut rec = Recorder::default();
        prepare_dir(root.path(), "c1", 0, 0, &limits(), &mut rec).unwrap();
        for f in ["memory.max", "cpu.max", "pids.max", "memory.high"] {
            let p = root.path().join("c1/d").join(f);
            assert!(rec.chowns.contains(&(p, NOBODY, NOBODY)), "{f}");
        }
    }

    #[test]
    fn a_failure_leaves_no_cgroup_behind() {
        let root = tempdir::Dir::new();
        let mut rec = Recorder {
            fail_chown: true,
            ..Default::default()
        };
        assert!(prepare_dir(root.path(), "c2", 0, 0, &[], &mut rec).is_err());
        assert!(!root.path().join("c2").exists());
    }

    #[test]
    fn an_id_cannot_leave_the_cgroup_root() {
        let root = tempdir::Dir::new();
        for bad in ["", "../x", "a/b", ".hidden"] {
            assert!(
                prepare_dir(root.path(), bad, 0, 0, &[], &mut Recorder::default()).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_limits_leave_the_spec_for_the_parent_and_the_devices_stay() {
        let mut s = json!({"linux": {"cgroupsPath": "/c1", "resources": {
            "memory": {"limit": 268435456}, "cpu": {"shares": 1024, "quota": 50000, "period": 100000},
            "pids": {"limit": 64}, "devices": [{"allow": false, "access": "rwm"}]}}});
        let got = take_limits(&mut s);
        assert!(got.contains(&("memory.max", "268435456".to_string())));
        assert!(got.contains(&("cpu.max", "50000 100000".to_string())));
        assert!(got.contains(&("pids.max", "64".to_string())));
        assert!(got.contains(&("cpu.weight", "39".to_string())), "{got:?}"); // 1024 shares, as crun converts it
        let res = &s["linux"]["resources"];
        assert_eq!(res.as_object().unwrap().len(), 1, "only devices is left");
        assert!(res["devices"].is_array());
        // No limits asked: nothing to write, and nothing breaks.
        let mut none = json!({"linux": {"resources": {"devices": []}}});
        assert!(take_limits(&mut none).is_empty());
        assert!(take_limits(&mut json!({})).is_empty());
    }

    fn base_clean() -> Value {
        json!({
            "process": {"user": {"uid": 0, "gid": 0}, "capabilities": {
                "bounding": ["CAP_SETUID", "CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER", "CAP_KILL"],
                "effective": ["CAP_SETUID", "CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER"],
                "permitted": ["CAP_SETUID", "CAP_FOWNER"],
                "inheritable": [], "ambient": []}},
            "linux": {"namespaces": [{"type": "mount"}, {"type": "pid"}]},
            "mounts": [{"destination": "/sys", "type": "sysfs"}],
        })
    }

    #[test]
    fn the_spec_gets_a_cgroup_namespace_a_cgroup_mount_and_fewer_capabilities() {
        let mut s = base_clean();
        apply(&mut s);
        apply(&mut s); // twice changes nothing more than once
        let ns: Vec<_> = s["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ns.iter().filter(|n| *n == "cgroup").count(), 1);
        let mount = s["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["destination"] == CGROUP_ROOT)
            .unwrap();
        assert_eq!(mount["type"], "cgroup2");
        let opts = mount["options"].to_string();
        assert!(!opts.contains("\"ro\"") && opts.contains("nosuid") && opts.contains("noexec"));
        // Nothing is added, the stripped ones are gone from every set, and CAP_CHOWN stays.
        let caps = s["process"]["capabilities"].to_string();
        for c in STRIPPED_CAPS {
            assert!(!caps.contains(c), "{c}");
        }
        assert!(
            caps.contains("CAP_SETUID") && caps.contains("CAP_KILL") && caps.contains("CAP_CHOWN")
        );
        assert_eq!(identity(&s), (0, 0));
    }

    /// Minimal temporary directory (no new dependency).
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let p = std::env::temp_dir().join(format!(
                    "cgdelegate-test-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
