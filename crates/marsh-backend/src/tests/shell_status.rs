//! Actual native exit, actual output failure and authenticated receipt query.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

fn execute_seven() -> marsh_runtime::CommandOutput {
    use marsh_runtime::{CommandRunner, Invocation, SystemCommandRunner};
    SystemCommandRunner::new("/tmp")
        .run(&Invocation {
            program: "/usr/bin/python3".into(),
            arguments: vec![
                "-c".into(),
                "import os; os.write(1,b'actual-seven'); os._exit(7)".into(),
            ],
            working_directory: None,
        })
        .unwrap()
}

#[test]
fn observed_nonzero_exit_is_separate_from_public_delivery_and_cleanup_failure() {
    use marsh_daemon::{Client, Server};
    let root = tempfile::tempdir().unwrap();
    let server = Server::bind(root.path()).unwrap();
    let store = server.store();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let task = thread::spawn(move || {
        server
            .serve_until(|| stopping.load(Ordering::Acquire))
            .unwrap();
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let client = Client::connect(root.path()).unwrap();
        let session = store.attach_shell(
            std::process::id(),
            marsh_daemon::SessionAuthority {
                username: "fixture".into(),
                uid: rustix::process::getuid().as_raw(),
                gid: rustix::process::getgid().as_raw(),
                launch_directory: root.path().into(),
                guest_home: root.path().into(),
                home_backing: root.path().into(),
                ephemeral_home: false,
            },
        );
        for (uncertain, lose_output, expected) in
            [(false, false, 7), (true, false, 125), (false, true, 125)]
        {
            let observed = execute_seven();
            assert_eq!(observed.exit_code, Some(7));
            let execution = ExecutionOutcome::Exited {
                code: observed.exit_code.unwrap(),
            };
            let (output, sink) = UnixStream::pair().unwrap();
            // Lost output is a closed reader (macOS accepts writes after SHUT_RD).
            let mut sink = (!lose_output).then_some(sink);
            // macOS may already refuse attachment setup on a closed peer.
            let delivered = ServerAttachment::new(output)
                .and_then(|attachment| {
                    attachment.send(&AttachmentFrame::Stdout {
                        bytes: observed.stdout,
                    })
                })
                .is_ok();
            assert_eq!(delivered, !lose_output);
            if let Some(sink) = sink.as_mut() {
                assert!(
                    matches!(marsh_daemon::read_frame::<AttachmentFrame>(sink).unwrap(), AttachmentFrame::Stdout { bytes } if bytes == b"actual-seven")
                );
            }
            let (code, cause) = public_exit(&execution, uncertain, delivered);
            let (public, mut public_reader) = UnixStream::pair().unwrap();
            ServerAttachment::new(public)
                .unwrap()
                .send(&AttachmentFrame::Exited {
                    code: code.unwrap_or(125),
                })
                .unwrap();
            let (job, _) = store
                .begin_job(marsh_daemon::NewJob {
                    session_id: session.clone(),
                    command: "actual-seven".into(),
                    kit_ref: "fixture".into(),
                    workload_image: "fixture".into(),
                    mounts: vec![],
                })
                .unwrap();
            store
                .finish_job_with_execution(
                    &job,
                    execution.clone(),
                    ExitStatus { code, cause },
                    delivered,
                    if uncertain {
                        CleanupState::Uncertain
                    } else {
                        CleanupState::Verified
                    },
                    TimingReport::default(),
                )
                .unwrap();
            let receipt = client.job(job).unwrap();
            assert_eq!(receipt.execution, execution);
            assert_eq!(receipt.exit.unwrap().code, Some(expected));
            assert_eq!(
                marsh_daemon::read_frame::<AttachmentFrame>(&mut public_reader).unwrap(),
                AttachmentFrame::Exited { code: expected }
            );
        }
    }));
    stop.store(true, Ordering::Release);
    task.join().unwrap();
    result.unwrap();
}
