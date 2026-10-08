//! Admission boundaries of the process table (`docs/design/processes.md` s6),
//! each mirroring a `Process.tla` negative control. Black-box acceptance
//! cannot reach these cheaply (64 launches, a held slot across a restart,
//! a forged relay parent).

use super::*;
use crate::{
    AuthenticationScope, CleanupState, Client, ErrorCode, ExitStatus, NewJob, PublicRequest,
    Server, SessionAuthority, TimingReport, authorize_request_session,
};

const PROJECT: &str = "/Users/example/project";

fn registered() -> Vec<String> {
    ["claude", "codex", "fixture", "pi", "shell"]
        .map(str::to_owned)
        .to_vec()
}

fn view() -> Vec<PublicMount> {
    vec![PublicMount {
        target: PROJECT.into(),
        access: "read_write".into(),
    }]
}

fn session(store: &DaemonStore, home: &Path) -> String {
    store.attach_shell(
        42,
        SessionAuthority {
            username: "example".into(),
            uid: 1000,
            gid: 1000,
            launch_directory: PROJECT.into(),
            guest_home: "/Users/example".into(),
            home_backing: home.into(),
            ephemeral_home: false,
        },
    )
}

fn launch(
    store: &DaemonStore,
    session: &str,
    parent: Option<&str>,
    kit: &str,
    mounts: Vec<PublicMount>,
    branch: bool,
) -> Result<String, DaemonError> {
    let link = ProcessLink {
        parent_job: parent.map(str::to_owned),
        spawn: None,
        branch,
        start_env: std::collections::BTreeMap::new(),
    };
    store
        .begin_job_process(
            NewJob {
                session_id: session.into(),
                command: kit.into(),
                kit_ref: kit.into(),
                workload_image: format!("example/{kit}@sha256:{}", "a".repeat(64)),
                mounts,
            },
            &link,
            &registered(),
        )
        .map(|(job, _, _)| job)
}

fn child(
    store: &DaemonStore,
    session: &str,
    parent: &str,
    kit: &str,
) -> Result<String, DaemonError> {
    launch(store, session, Some(parent), kit, view(), false)
}

fn end(store: &DaemonStore, job: &str, cleanup: CleanupState) {
    store
        .finish_job(
            job,
            ExitStatus {
                code: Some(0),
                cause: "exited".into(),
            },
            true,
            cleanup,
            TimingReport::default(),
        )
        .unwrap();
}

fn refusal(result: Result<String, DaemonError>) -> String {
    match result {
        Err(DaemonError::Refused(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn pool_holds_uncertain_slots_across_restart_until_reset() {
    // BudgetBound / freeOnRevoke: an unconfirmed end keeps its slot, also
    // after a daemon restart, until a successful reset of its Kit.
    let home = tempfile::tempdir().unwrap();
    {
        let store = DaemonStore::new(home.path());
        let session = session(&store, home.path());
        let jobs = (0..8)
            .map(|_| launch(&store, &session, None, "fixture", view(), false).unwrap())
            .collect::<Vec<_>>();
        assert!(
            refusal(launch(&store, &session, None, "fixture", view(), false))
                .starts_with("capacity: 8 jobs")
        );
        end(&store, &jobs[0], CleanupState::Uncertain);
        let held = refusal(launch(&store, &session, None, "fixture", view(), false));
        assert!(
            held.contains("1 held by uncertain jobs on Kit fixture"),
            "{held}"
        );
        end(&store, &jobs[1], CleanupState::Verified);
        launch(&store, &session, None, "fixture", view(), false).unwrap();
    }
    // Restart: every unfinished job is uncertain and holds a slot.
    let store = reopen(home.path());
    assert_eq!(store.process_counts().held, 8);
    store.release_process_slots(&["shell".to_owned()]);
    assert_eq!(store.process_counts().held, 8);
    store.release_process_slots(&["fixture".to_owned()]);
    assert_eq!(store.process_counts().held, 0);
    drop(store);
    // The reset is durable.
    assert_eq!(reopen(home.path()).process_counts().held, 0);
}

/// A restarted store. Another test's fork may briefly hold a duplicate of
/// the dropped store's journal lock (until its exec closes it).
fn reopen(home: &Path) -> DaemonStore {
    for _ in 0..100 {
        if let Ok(store) = DaemonStore::open(home) {
            return store;
        }
        thread::sleep(Duration::from_millis(20));
    }
    DaemonStore::new(home)
}

#[test]
fn depth_fan_out_and_parent_liveness() {
    let home = tempfile::tempdir().unwrap();
    let store = DaemonStore::new(home.path());
    let session = session(&store, home.path());
    // noDepth: a root is depth 1; depth 5 is refused.
    let mut chain = vec![launch(&store, &session, None, "claude", view(), false).unwrap()];
    for kit in ["codex", "claude", "codex"] {
        chain.push(child(&store, &session, chain.last().unwrap(), kit).unwrap());
    }
    let deepest = chain.last().unwrap();
    assert_eq!(store.job(deepest).unwrap().lineage.unwrap().depth, 4);
    assert!(refusal(child(&store, &session, deepest, "shell")).starts_with("depth limit 4"));
    for job in chain.iter().rev() {
        end(&store, job, CleanupState::Verified);
    }
    // noFan: four live children, the fifth is refused until one ends.
    let root = launch(&store, &session, None, "claude", view(), false).unwrap();
    let kids = (0..4)
        .map(|_| child(&store, &session, &root, "shell").unwrap())
        .collect::<Vec<_>>();
    assert!(refusal(child(&store, &session, &root, "shell")).starts_with("fan-out limit 4"));
    end(&store, &kids[0], CleanupState::Verified);
    child(&store, &session, &root, "shell").unwrap();
    // Spawn is atomic with the parent's liveness: an ended parent starts nothing.
    assert!(refusal(child(&store, &session, &kids[0], "shell")).contains("has ended"));
    // revokeOnly / orphan: the ended parent's refusal counted.
    assert!(store.process_counts().refused >= 3);
}

#[test]
fn refused_launches_count_toward_the_tree_total() {
    // noTotal: 64 launches per tree, refusals included.
    let home = tempfile::tempdir().unwrap();
    let store = DaemonStore::new(home.path());
    let session = session(&store, home.path());
    let root = launch(&store, &session, None, "claude", view(), false).unwrap();
    for _ in 0..63 {
        let refused = refusal(child(&store, &session, &root, "unregistered"));
        assert!(
            refused.starts_with("spawn refused: unregistered"),
            "{refused}"
        );
    }
    assert!(refusal(child(&store, &session, &root, "shell")).starts_with("total limit 64"));
    // Another tree is unaffected.
    let other = launch(&store, &session, None, "claude", view(), false).unwrap();
    child(&store, &session, &other, "shell").unwrap();
}

#[test]
fn same_kit_chain_counts_consecutive_steps_and_stops_at_a_branch() {
    // noSameKit: claude -> claude is allowed, a third claude is refused.
    let home = tempfile::tempdir().unwrap();
    let store = DaemonStore::new(home.path());
    let session = session(&store, home.path());
    let root = launch(&store, &session, None, "claude", view(), false).unwrap();
    let second = child(&store, &session, &root, "claude").unwrap();
    assert_eq!(
        refusal(child(&store, &session, &second, "claude")),
        "claude → claude → claude refused: same-Kit chain limit 2 (see /run/marsh/context.md)"
    );
    // Another Kit breaks the chain.
    let codex = child(&store, &session, &second, "codex").unwrap();
    child(&store, &session, &codex, "claude").unwrap();
    // A split argv branch is not a same-Kit step, and the walk stops there.
    let fork = vec![PublicMount {
        target: format!("{PROJECT}/.marsh/split/0123456789ab/k").into(),
        access: "read_write".into(),
    }];
    let branch = launch(
        &store,
        &session,
        Some(&second),
        "claude",
        fork.clone(),
        true,
    )
    .unwrap();
    let lineage = store.job(&branch).unwrap().lineage.unwrap();
    assert_eq!(
        (lineage.split.as_deref(), lineage.label.as_deref()),
        (Some("0123456789ab"), Some("k"))
    );
    launch(
        &store,
        &session,
        Some(&branch),
        "claude",
        fork.clone(),
        false,
    )
    .unwrap();
    for job in [codex, second, root] {
        end(&store, &job, CleanupState::Verified);
    }
    // Under a branch, ProcessRun steps count again from the branch job.
    let top = launch(&store, &session, None, "claude", view(), false).unwrap();
    let branch = launch(&store, &session, Some(&top), "claude", fork.clone(), true).unwrap();
    let under = launch(&store, &session, Some(&branch), "claude", fork, false).unwrap();
    // A child copies the fork view but is not itself a branch job.
    assert_eq!(store.job(&under).unwrap().lineage.unwrap().split, None);
    assert!(refusal(child(&store, &session, &under, "claude")).contains("same-Kit chain limit 2"));
}

#[test]
fn a_child_view_is_the_parents_and_never_wider() {
    // widen / Attenuation: the view is copied; a cwd outside it is refused.
    let parent = vec![
        PublicMount {
            target: format!("{PROJECT}/.marsh/split/0123456789ab/k").into(),
            access: "read_write".into(),
        },
        PublicMount {
            target: "/Users/example".into(),
            access: "read_write".into(),
        },
    ];
    let fork = Path::new(PROJECT).join(".marsh/split/0123456789ab/k/src");
    assert_eq!(child_view(&parent, &fork).unwrap(), parent);
    assert!(child_view(&parent, Path::new("/private/elsewhere")).is_err());
}

#[test]
fn a_relay_cannot_name_a_parent_job() {
    // A forged parent would borrow another tree's view and spawn set.
    let home = tempfile::tempdir().unwrap();
    let store = DaemonStore::new(home.path());
    let session = session(&store, home.path());
    let token = store.issue_relay_token(&session).unwrap();
    let request = |parent: &str| {
        PublicRequest::Execute(ExecuteSpec {
            command: "fixture".into(),
            arguments: Vec::new(),
            placement: crate::Placement::Local,
            environment: BTreeMap::new(),
            working_directory: None,
            session: SessionSpec {
                session_id: session.clone(),
                username: "example".into(),
                uid: 1000,
                gid: 1000,
                launch_directory: PROJECT.into(),
                guest_home: "/Users/example".into(),
                home_backing: home.path().into(),
                ephemeral_home: false,
                terminal: false,
                terminal_size: None,
            },
            process: Some(ProcessLink {
                parent_job: Some(parent.into()),
                spawn: Some(vec!["fixture".into()]),
                branch: false,
                start_env: std::collections::BTreeMap::new(),
            }),
        })
    };
    let mut relayed = request("someone-else");
    authorize_request_session(
        &store,
        &AuthenticationScope::Relay(session.clone()),
        &token,
        &mut relayed,
    )
    .unwrap();
    let PublicRequest::Execute(spec) = relayed else {
        unreachable!()
    };
    let link = spec.process.unwrap();
    assert_eq!(link.parent_job, None);
    assert_eq!(link.spawn, Some(vec!["fixture".to_owned()]));
}

#[test]
fn a_job_capability_sees_only_its_own_subtree() {
    let root = tempfile::tempdir().unwrap();
    let server = Arc::new(Server::bind(root.path()).unwrap());
    let store = &server.store;
    let session = session(store, root.path());
    let mine = launch(store, &session, None, "claude", view(), false).unwrap();
    let kid = child(store, &session, &mine, "codex").unwrap();
    let other = launch(store, &session, None, "claude", view(), false).unwrap();
    let capability = Client {
        paths: server.lifecycle.paths.clone(),
        token: store.issue_capability_token(&mine).unwrap(),
    };
    let ask = |request: PublicRequest| {
        let task_server = Arc::clone(&server);
        let task = thread::spawn(move || task_server.serve_one().unwrap());
        let reply = capability.request(request).unwrap();
        task.join().unwrap();
        reply
    };
    let PublicReply::Jobs(jobs) = ask(PublicRequest::Jobs) else {
        panic!("jobs refused")
    };
    let mut ids = jobs
        .jobs
        .into_iter()
        .map(|job| job.job_id)
        .collect::<Vec<_>>();
    ids.sort();
    let mut want = vec![mine.clone(), kid.clone()];
    want.sort();
    assert_eq!(ids, want);
    let PublicReply::Job(shown) = ask(PublicRequest::ShowJob {
        job_id: mine.clone(),
    }) else {
        panic!("own job hidden")
    };
    assert_eq!(shown.children, vec![kid]);
    assert!(matches!(
        ask(PublicRequest::ShowJob { job_id: other }),
        PublicReply::Error {
            code: ErrorCode::NotFound,
            ..
        }
    ));
    let PublicReply::ProcessTree { document } = ask(PublicRequest::ProcessShow) else {
        panic!("tree refused")
    };
    assert_eq!(document["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(document["jobs"][0]["job_id"], mine.as_str());
}

#[test]
fn a_tree_cancel_signals_every_descendant_at_once_and_ends_them_cancelled() {
    // CancelPropagates (s8): one cancel reaches every level now, not one
    // level per grace; nothing new starts under it; each ends `cancelled`.
    let home = tempfile::tempdir().unwrap();
    let store = DaemonStore::new(home.path());
    let session = session(&store, home.path());
    let root = launch(&store, &session, None, "claude", view(), false).unwrap();
    let middle = child(&store, &session, &root, "shell").unwrap();
    let leaf = child(&store, &session, &middle, "codex").unwrap();
    let other = launch(&store, &session, None, "fixture", view(), false).unwrap();
    let signals = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
    for job in [&root, &middle, &leaf, &other] {
        let signals = Arc::clone(&signals);
        let job = job.clone();
        store.register_job_control(
            &job.clone(),
            JobControl(Arc::new(move |signal: &str| {
                signals
                    .lock()
                    .unwrap()
                    .push((job.clone(), signal.to_owned()));
            })),
        );
    }
    store.cancel_tree(&root, false);
    let mut seen = signals.lock().unwrap().clone();
    seen.sort();
    let mut want = vec![
        (middle.clone(), "INT".to_owned()),
        (leaf.clone(), "INT".to_owned()),
    ];
    want.sort();
    assert_eq!(seen, want, "the caller already signalled the root itself");
    assert!(refusal(child(&store, &session, &leaf, "fixture")).contains("being cancelled"));
    for job in [&leaf, &middle, &root] {
        assert!(store.cancel_requested(job));
        end(&store, job, CleanupState::Verified);
        assert_eq!(store.job(job).unwrap().state, JobState::Cancelled);
    }
    end(&store, &other, CleanupState::Verified);
    assert_eq!(store.job(&other).unwrap().state, JobState::Finished);
}

#[test]
fn tree_kit_vm_cap_counts_distinct_live_kits() {
    // docs/design/processes.md s6: at most 4 distinct Kit VMs live in one tree; a Kit
    // already live in the tree (an alias, a second call) costs nothing.
    let home = tempfile::tempdir().unwrap();
    let store = DaemonStore::new(home.path());
    let session = session(&store, home.path());
    let root = launch(&store, &session, None, "fixture", view(), false).unwrap();
    let middle = child(&store, &session, &root, "claude").unwrap();
    let codex = child(&store, &session, &root, "codex").unwrap();
    child(&store, &session, &root, "shell").unwrap();
    child(&store, &session, &middle, "fixture").unwrap();
    let link = ProcessLink {
        parent_job: Some(middle.clone()),
        ..ProcessLink::default()
    };
    let registered = registered();
    let refused = store
        .begin_job_process(
            NewJob {
                session_id: session.clone(),
                command: "pi".into(),
                kit_ref: "pi".into(),
                workload_image: format!("example/pi@sha256:{}", "a".repeat(64)),
                mounts: view(),
            },
            &link,
            &registered,
        )
        .unwrap_err();
    assert!(
        matches!(&refused, DaemonError::Refused(message) if message
            == "pi refused: tree already uses 4 Kit VMs (claude, codex, fixture, shell) (MARSH_TREE_KIT_VMS)"),
        "{refused:?}"
    );
    // The pre-check (Kit identity known before VM work) refuses the same.
    assert!(
        store
            .admit_process(&session, "pi", Some("pi"), &link, &registered)
            .is_err()
    );
    end(&store, &codex, CleanupState::Verified);
    store
        .admit_process(&session, "pi", Some("pi"), &link, &registered)
        .unwrap();
}

#[test]
fn tree_kit_vm_cap_follows_the_daemon_setting() {
    let home = tempfile::tempdir().unwrap();
    let store = DaemonStore::new(home.path());
    store.lock().job_defaults = Some(
        crate::JobDefaults::from_environment(|name| {
            (name == "MARSH_TREE_KIT_VMS").then(|| "1".into())
        })
        .unwrap(),
    );
    let session = session(&store, home.path());
    let root = launch(&store, &session, None, "fixture", view(), false).unwrap();
    child(&store, &session, &root, "fixture").unwrap();
    assert_eq!(
        refusal(child(&store, &session, &root, "shell")),
        "shell refused: tree already uses 1 Kit VM (fixture) (MARSH_TREE_KIT_VMS)"
    );
    // Another tree has its own budget.
    launch(&store, &session, None, "shell", view(), false).unwrap();
    assert!(
        crate::JobDefaults::from_environment(|name| {
            (name == "MARSH_TREE_KIT_VMS").then(|| "0".into())
        })
        .is_err()
    );
}

#[test]
fn child_wall_time_ends_by_its_parents_deadline() {
    // docs/design/processes.md s6: wall = min(the Kit's own limit, the parent's
    // remaining time); a parent with too little left starts nothing.
    let home = tempfile::tempdir().unwrap();
    let store = DaemonStore::new(home.path());
    let session = session(&store, home.path());
    let root = launch(&store, &session, None, "fixture", view(), false).unwrap();
    assert_eq!(store.set_job_deadline(&root, 30), (30, false));
    let long = child(&store, &session, &root, "shell").unwrap();
    let (left, from_parent) = store.set_job_deadline(&long, 3600);
    assert!(from_parent && (29..=30).contains(&left), "{left}");
    let short = child(&store, &session, &root, "fixture").unwrap();
    assert_eq!(store.set_job_deadline(&short, 10), (10, false));
    // The deadline is journaled with the receipt's lineage.
    let lineage = store.job(&long).unwrap().lineage.unwrap();
    assert!(lineage.parent_deadline && lineage.deadline_unix_ms.is_some());
    assert!(!store.job_deadline_passed(&long, 0));
    // The grandchild of `long` inherits the root's deadline through it.
    let grandchild = child(&store, &session, &long, "fixture").unwrap();
    assert!(store.set_job_deadline(&grandchild, 3600).1);
    // With under a second left, admission refuses before any record.
    let soon = crate::unix_time_ms() + 500;
    for job in [&root, &long] {
        store
            .lock()
            .jobs
            .get_mut(job.as_str())
            .unwrap()
            .lineage
            .as_mut()
            .unwrap()
            .deadline_unix_ms = Some(soon);
    }
    let refused = refusal(child(&store, &session, &long, "fixture"));
    assert!(
        refused.starts_with("fixture refused: deadline: parent job ")
            && refused.ends_with("ms of wall time left (MARSH_JOB_WALL_SECONDS)"),
        "{refused}"
    );
    assert!(store.job_deadline_passed(&long, 1_000));
}

#[test]
fn a_shell_branch_kit_job_and_its_children_hang_under_the_split() {
    // A Kit command run by a shell branch's session (cwd outside its fork,
    // so no fork mount names it) is the branch's child; its own children
    // follow; the forest draws the split in the creating session.
    let home = tempfile::tempdir().unwrap();
    let store = DaemonStore::new(home.path());
    let creator = session(&store, home.path());
    let branch = session(&store, home.path());
    store.mark_branch_session(&branch, "1a2b3c4d5e6f", "fix");
    let top = launch(&store, &branch, None, "claude", view(), false).unwrap();
    let kid = child(&store, &branch, &top, "codex").unwrap();
    let consumer = launch(&store, &creator, None, "claude", view(), false).unwrap();
    store.set_job_consumes(&consumer, "1a2b3c4d5e6f");
    store.set_job_consumes(&kid, "1a2b3c4d5e6f"); // a child never consumes
    let lineage = |job: &str| store.job(job).unwrap().lineage.unwrap();
    assert_eq!(lineage(&top).parent, "split:1a2b3c4d5e6f/fix");
    assert_eq!(lineage(&top).label.as_deref(), Some("fix"));
    assert_eq!(lineage(&kid).parent, format!("job:{top}"));
    assert_eq!(lineage(&consumer).consumes.as_deref(), Some("1a2b3c4d5e6f"));
    assert_eq!(lineage(&kid).consumes, None);
    store.detach_shell(&branch).unwrap();
    let split = crate::split::SplitLineage {
        id: "1a2b3c4d5e6f".into(),
        session: creator.clone(),
        creator_job: None,
        parent: None,
        state: "joined".into(),
        status: Some(0),
        created_unix_ms: 1,
        finished_unix_ms: Some(2),
        timing_ms: BTreeMap::new(),
        branches: ["fix", "review"]
            .map(|label| crate::split::BranchLineage {
                label: label.into(),
                kind: "shell".into(),
                command: format!("{label} it"),
                state: "captured".into(),
                status: Some("exited 0".into()),
                code: Some(0),
                job_id: None,
                files: 0,
                started_unix_ms: Some(1),
                finished_unix_ms: Some(2),
            })
            .to_vec(),
    };
    let forest = store.process_forest(&[split]);
    let roots = forest["jobs"].as_array().unwrap();
    assert_eq!(roots.len(), 2, "{forest}");
    let node = roots.iter().find(|root| root["node"] == "split").unwrap();
    assert_eq!(node["session_id"], creator.as_str());
    let fix = &node["children"][0];
    assert_eq!(
        (fix["label"].as_str(), fix["command"].as_str()),
        (Some("fix"), Some("fix it"))
    );
    assert_eq!(fix["children"][0]["job_id"], top.as_str());
    assert_eq!(fix["children"][0]["children"][0]["job_id"], kid.as_str());
    assert_eq!(
        node["children"][1]["children"].as_array().map(Vec::len),
        Some(0)
    );
    // A forgotten split is still drawn from its jobs' lineage.
    let orphan = store.process_forest(&[]);
    let node = orphan["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|root| root["node"] == "split")
        .unwrap();
    assert_eq!(node["children"][0]["children"][0]["job_id"], top.as_str());
}

#[test]
fn a_fanout_in_a_shell_branch_is_drawn_as_a_fanout_node_in_that_branch() {
    // `split { review: fanout { races: X, leaks: Y } | collect }`: the
    // fanout's jobs run in the branch's session; the tree draws them under
    // one fanout node inside the branch, after the branch's other jobs.
    let home = tempfile::tempdir().unwrap();
    let store = DaemonStore::new(home.path());
    let branch = session(&store, home.path());
    store.mark_branch_session(&branch, "1a2b3c4d5e6f", "review");
    let before = launch(&store, &branch, None, "fixture", view(), false).unwrap();
    let races = launch(&store, &branch, None, "codex", view(), false).unwrap();
    let leaks = launch(&store, &branch, None, "claude", view(), false).unwrap();
    store.set_job_fanout(&races, "00112233aabbccdd/races");
    store.set_job_fanout(&leaks, "00112233aabbccdd/leaks");
    let kid = child(&store, &branch, &races, "fixture").unwrap();
    store.set_job_fanout(&kid, "00112233aabbccdd/races"); // a child is never grouped
    assert_eq!(store.job(&kid).unwrap().lineage.unwrap().fanout, None);
    let forest = store.process_forest(&[]);
    let split = forest["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|root| root["node"] == "split")
        .unwrap_or_else(|| panic!("{forest}"));
    let review = &split["children"][0];
    assert_eq!(review["label"], "review");
    let children = review["children"].as_array().unwrap();
    assert_eq!(children.len(), 2, "{review}");
    assert_eq!(children[0]["job_id"], before.as_str());
    let fanout = &children[1];
    assert_eq!(fanout["node"], "fanout");
    let labels = fanout["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|branch| branch["label"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(labels, ["races", "leaks"]);
    assert_eq!(
        fanout["children"][0]["children"][0]["job_id"],
        races.as_str()
    );
    assert_eq!(
        fanout["children"][0]["children"][0]["children"][0]["job_id"],
        kid.as_str()
    );
    // No fanout id reaches a split branch from an enclosing fanout.
    assert!(!crate::split::forwardable("FANOUT_BRANCH"));
}
