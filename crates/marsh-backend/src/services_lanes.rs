//! App data in lanes (C3): the backend side of `DaemonBackend::
//! {services_restore_prepare, services_restore_up, services_drop_workspace,
//! services_enabled, test_hold}`.
//!
//! The backend only carries calls to the project's services agent through E1's
//! relay and hands the **raw** answer back. Validating it, writing the lane
//! files and building the restore line are the daemon's, on the Mac
//! (`marsh_daemon::lane_services`; SECURITY-DESIGN A8, A9). A restore starts
//! the project's services VM if it is stopped (a checkpoint taken while it was
//! idle-stopped restores from the snapshot it references); a drop never does:
//! a stopped VM runs nothing, and the daemon retries the drop when it is back.

use super::StockDaemonBackend;
use super::endpoint::EndpointScope;
use marsh_daemon::lane_services::breaks as lane_breaks;
use marsh_daemon::lane_services::{CallFailure, DropOutcome, Problem};
use marsh_daemon::{LaneCall, PublicMount};
use marsh_sbx::services_relay::RelayError;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::PoisonError,
    time::{Duration, Instant},
};

/// An open lane: its project and what its restore remapped.
#[derive(Clone, Debug, Default)]
pub(crate) struct LaneEntry {
    root: PathBuf,
    /// Original port number to the fork's published number.
    ports: BTreeMap<u16, u16>,
    /// Bumped on every update, so a port watcher notices a new mapping.
    generation: u64,
}

/// What a job's recorded mounts say about where it runs.
#[derive(Debug, Eq, PartialEq)]
enum Placement {
    /// The project itself.
    Project,
    /// A fork of a split: its id, label and folder.
    Fork(String, String, PathBuf),
    /// Something else (no endpoint).
    Unknown,
}

/// Where a job runs, from the mounts the daemon recorded for it (a child's are
/// its parent's), never from its working directory: a child of a lane job that
/// `cd ~` still has the lane's mounts, and so the lane's scope. Read-write
/// mounts only: the project itself, or exactly one fork
/// `<root>/.marsh/split/<id>/<label>`.
fn placement(root: &Path, mounts: &[PublicMount]) -> Placement {
    let split_area = root.join(".marsh/split");
    let mut found = Placement::Unknown;
    for mount in mounts.iter().filter(|mount| mount.access == "read_write") {
        let candidate = if mount.target == root {
            Placement::Project
        } else if let Ok(relative) = mount.target.strip_prefix(&split_area) {
            let parts: Vec<_> = relative
                .components()
                .map(|component| component.as_os_str().to_str())
                .collect();
            match parts.as_slice() {
                [Some(id), Some(label)]
                    if marsh_daemon::split_confinement::split_id(id)
                        && marsh_daemon::split_confinement::split_label(label) =>
                {
                    Placement::Fork((*id).to_owned(), (*label).to_owned(), mount.target.clone())
                }
                // `.admin/<label>` and the like are Git metadata of a fork.
                _ => continue,
            }
        } else {
            continue;
        };
        if found != Placement::Unknown {
            // The project and a fork, or two forks: not one scope.
            return Placement::Unknown;
        }
        found = candidate;
    }
    found
}

fn failure(error: &RelayError) -> CallFailure {
    let problem = match error {
        RelayError::NotAuthorized(_) => Problem::Off,
        RelayError::Unavailable(_) | RelayError::Hostile(_) | RelayError::Timeout(_) => {
            Problem::Unavailable
        }
        RelayError::Invalid(_)
        | RelayError::Limit(_)
        | RelayError::Refused(_)
        | RelayError::Agent(_) => Problem::Failed,
    };
    CallFailure::new(problem, error.to_string())
}

fn text<'a>(path: &'a Path, what: &str) -> Result<&'a str, CallFailure> {
    path.to_str()
        .ok_or_else(|| CallFailure::new(Problem::Failed, format!("{what} is not valid text")))
}

impl StockDaemonBackend {
    /// `DaemonBackend::services_lane`: a lane exists (with the ports its
    /// restore remapped) or is gone.
    pub(super) fn lane_register(
        &self,
        root: &Path,
        workspace: &str,
        ports: Option<&BTreeMap<u16, u16>>,
    ) {
        let mut lanes = self
            .services
            .lanes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match ports {
            Some(ports) => {
                let generation = lanes.get(workspace).map_or(1, |lane| lane.generation + 1);
                lanes.insert(
                    workspace.to_owned(),
                    LaneEntry {
                        root: root.to_owned(),
                        ports: ports.clone(),
                        generation,
                    },
                );
            }
            None => {
                lanes.remove(workspace);
            }
        }
    }

    /// The number a job in `workspace` listening on `port` reaches: the fork's
    /// published port in a lane, `port` itself everywhere else (A6).
    pub(crate) fn lane_port(&self, workspace: &str, port: u16) -> u16 {
        self.services
            .lanes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(workspace)
            .and_then(|lane| lane.ports.get(&port).copied())
            .unwrap_or(port)
    }

    /// Changes whenever `workspace`'s mapping does; `0` for the base.
    pub(crate) fn lane_generation(&self, workspace: &str) -> u64 {
        self.services
            .lanes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(workspace)
            .map_or(0, |lane| lane.generation)
    }

    /// The ports a job in `workspace` listens on, given the fork's published
    /// ones: each remapped port as its **original** number (A6: the project's
    /// own numbers work in a lane) and also as the number the fork published
    /// (the lane's rewritten `.env` names those), the rest as they are.
    pub(crate) fn lane_listen_ports(&self, workspace: &str, published: &[u16]) -> Vec<u16> {
        let lanes = self
            .services
            .lanes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(lane) = lanes.get(workspace) else {
            return published.to_vec();
        };
        let originals: BTreeMap<u16, u16> = lane
            .ports
            .iter()
            .map(|(original, actual)| (*actual, *original))
            .collect();
        let mut listen: Vec<u16> = published
            .iter()
            .flat_map(|port| [originals.get(port).copied().unwrap_or(*port), *port])
            .collect();
        listen.sort_unstable();
        listen.dedup();
        listen
    }

    /// The scope of a job's endpoint (E2, C3): from the daemon's own state
    /// only. The project's own jobs are the base workspace; a job whose
    /// recorded mounts name an **open lane's** fork is that lane's workspace
    /// with the fork as the root its bind sources resolve beneath. Anything
    /// else (a fork that is not an open lane, mounts that name two places) gets
    /// no endpoint at all. `lane-scope-root` is the defect that gives a lane the
    /// project's root scope.
    pub(super) fn services_job_scope(
        &self,
        launch_directory: &Path,
        mounts: &[PublicMount],
    ) -> Option<EndpointScope> {
        let root = self.services_job_root(launch_directory)?;
        // The mounts name paths as the session does; the scope uses the
        // project's canonical root.
        match placement(launch_directory, mounts) {
            Placement::Project => Some(EndpointScope::base(root)),
            Placement::Fork(split, label, fork) => {
                let fork = root.join(fork.strip_prefix(launch_directory).ok()?);
                let workspace =
                    marsh_daemon::checkpoint::lane_workspace(&format!("{split}/{label}"))?;
                let registered = self
                    .services
                    .lanes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&workspace)
                    .is_some_and(|lane| lane.root == root);
                if !registered {
                    return None;
                }
                if marsh_contracts::break_mode(lane_breaks::LANE_SCOPE_ROOT) {
                    return Some(EndpointScope::base(root));
                }
                Some(EndpointScope {
                    root,
                    authority_root: fork,
                    workspace,
                })
            }
            Placement::Unknown => None,
        }
    }

    /// `DaemonBackend::services_restore_prepare`.
    pub(super) fn lane_restore_prepare(
        &self,
        call: &LaneCall<'_>,
        snapshot: &str,
    ) -> Result<serde_json::Value, CallFailure> {
        let relay = self.services_relay(call.root).map_err(|e| failure(&e))?;
        let root = text(call.root, "the project folder")?;
        let lane = text(call.lane_dir, "the lane folder")?;
        let source = call
            .source
            .map(|source| text(source, "the source folder"))
            .transpose()?;
        relay
            .restore_prepare(snapshot, call.workspace, root, source, lane, call.fork)
            .map(|reply| reply.result)
            .map_err(|e| failure(&e))
    }

    /// `DaemonBackend::services_restore_up`.
    pub(super) fn lane_restore_up(
        &self,
        call: &LaneCall<'_>,
    ) -> Result<serde_json::Value, CallFailure> {
        let relay = self.services_relay(call.root).map_err(|e| failure(&e))?;
        let root = text(call.root, "the project folder")?;
        let lane = text(call.lane_dir, "the lane folder")?;
        let source = call
            .source
            .map(|source| text(source, "the source folder"))
            .transpose()?;
        relay
            .restore_up(call.workspace, root, source, lane, call.fork)
            .map(|reply| reply.result)
            .map_err(|e| failure(&e))
    }

    /// `DaemonBackend::services_drop_workspace`: by workspace, through the
    /// agent. A project with no services VM has nothing to drop; a VM that is
    /// not ready (stopped, starting, relay closed) is retried later.
    pub(super) fn lane_drop_workspace(&self, root: &Path, workspace: &str) -> DropOutcome {
        if self.services.relays.is_none() {
            return DropOutcome::Nothing;
        }
        let relay = match self.ready_relay(root) {
            Ok(relay) => relay,
            Err(why) => {
                let vm_exists = self
                    .sbx
                    .services_vm_view(root, false)
                    .ok()
                    .flatten()
                    .is_some();
                return if vm_exists {
                    DropOutcome::Later(why)
                } else {
                    DropOutcome::Nothing
                };
            }
        };
        match relay.drop_workspace(workspace) {
            Ok(report) => DropOutcome::Dropped {
                containers: report.containers,
                volumes: report.volumes,
                networks: report.networks,
                left: report.left,
            },
            Err(error) => DropOutcome::Later(error.to_string()),
        }
    }

    /// `DaemonBackend::services_enabled`.
    pub(super) fn lane_services_enabled(&self, root: &Path) -> bool {
        self.services_settings()
            .is_ok_and(|settings| settings.enabled_for(root))
    }

    /// `DaemonBackend::test_hold`: `<point>.reached` appears under
    /// `test-holds` while the mode `<point>` is on, until `<point>.release`
    /// (at most two minutes).
    pub(super) fn lane_hold(&self, point: &str) {
        if !marsh_contracts::break_mode(point) {
            return;
        }
        let dir = self.config.daemon_home.join("test-holds");
        let _ = fs::create_dir_all(&dir);
        let release = dir.join(format!("{point}.release"));
        let _ = fs::write(dir.join(format!("{point}.reached")), b"");
        let deadline = Instant::now() + Duration::from_mins(2);
        while !release.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = fs::remove_file(release);
        let _ = fs::remove_file(dir.join(format!("{point}.reached")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(target: &str, access: &str) -> PublicMount {
        PublicMount {
            target: PathBuf::from(target),
            access: access.into(),
        }
    }

    /// The scope a job gets comes from its recorded mounts (table).
    #[test]
    fn a_jobs_placement_comes_from_its_mounts() {
        let root = Path::new("/Users/u/proj");
        let fork = "/Users/u/proj/.marsh/split/abc123def456/a";
        for (what, mounts, expected) in [
            (
                "project job",
                vec![
                    mount("/Users/u/proj", "read_write"),
                    mount("/Users/u/proj/.marsh", "read_only"),
                    mount("/home/u", "read_write"),
                ],
                Placement::Project,
            ),
            (
                "lane job",
                vec![
                    mount(fork, "read_write"),
                    mount(
                        "/Users/u/proj/.marsh/split/abc123def456/.admin/a",
                        "read_write",
                    ),
                    mount(
                        "/Users/u/proj/.marsh/split/abc123def456/store.git",
                        "read_only",
                    ),
                    mount("/home/u", "read_write"),
                ],
                Placement::Fork("abc123def456".into(), "a".into(), PathBuf::from(fork)),
            ),
            (
                "the project and a fork",
                vec![
                    mount("/Users/u/proj", "read_write"),
                    mount(fork, "read_write"),
                ],
                Placement::Unknown,
            ),
            (
                "two forks",
                vec![
                    mount(fork, "read_write"),
                    mount("/Users/u/proj/.marsh/split/abc123def456/b", "read_write"),
                ],
                Placement::Unknown,
            ),
            (
                "a read-only fork",
                vec![mount(fork, "read_only"), mount("/home/u", "read_write")],
                Placement::Unknown,
            ),
            (
                "a bad label",
                vec![mount(
                    "/Users/u/proj/.marsh/split/abc123def456/.hidden",
                    "read_write",
                )],
                Placement::Unknown,
            ),
            (
                "something deeper in a fork",
                vec![mount(
                    "/Users/u/proj/.marsh/split/abc123def456/a/src",
                    "read_write",
                )],
                Placement::Unknown,
            ),
            (
                "nothing",
                vec![mount("/home/u", "read_write")],
                Placement::Unknown,
            ),
        ] {
            assert_eq!(placement(root, &mounts), expected, "{what}");
        }
    }
}
