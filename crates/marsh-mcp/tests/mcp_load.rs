//! Real publication CLI + MCP JSON-RPC over a Unix socket. Only stock SBX and
//! pipeline placement are controlled processes; this is not stock qualification.
use marsh_mcp::{ExportConfig, ExportMcp, HostConfig, ToolDeclaration};
use rmcp::{
    ClientHandler, ServiceExt,
    model::{CallToolRequestParams, ClientConfig},
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::UnixStream,
    },
    path::Path,
    process::{Command, Stdio},
};

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[derive(Clone)]
struct Caller;
impl ClientHandler for Caller {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keep one live MCP caller across the CLI load/revoke transaction.
#[ignore = "requires MARSH_LOAD_TEST_BIN pointing at the prebuilt marsh with sibling host binaries"]
async fn cli_load_keeps_original_mcp_caller_valid_until_unpublish() {
    let cli = std::env::var_os("MARSH_LOAD_TEST_BIN").expect("MARSH_LOAD_TEST_BIN required");
    for name in ["marsh", "marsh-mcp", "marshd"] {
        let path = Path::new(&cli).with_file_name(name);
        let digest = format!(
            "{:x}",
            Sha256::digest(fs::read(&path).expect("all three host binaries required"))
        );
        eprintln!("MCP caller binary {} sha256={digest}", path.display());
        if let Some(evidence) = std::env::var_os("MARSH_MCP_TEST_EVIDENCE") {
            fs::create_dir_all(&evidence).unwrap();
            fs::write(
                Path::new(&evidence).join(format!("caller-{name}.sha256")),
                format!("{digest}  {}\n", path.display()),
            )
            .unwrap();
        }
    }
    let root = tempfile::tempdir().unwrap();
    // marsh admits only symlink-free paths; macOS temp dirs live under /var -> /private/var.
    let base = root.path().canonicalize().unwrap();
    let project = base.join("project");
    let home = base.join("home");
    let host = base.join("host");
    for path in [&project, &home, &host] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let sbx = root.path().join("sbx");
    executable(
        &sbx,
        r#"#!/usr/bin/env python3
import json,pathlib,sys
root=pathlib.Path(__file__).parent
p=root/'registration.json'
a=sys.argv[1:]
with (root/'effects').open('a') as f: f.write(a[1]+'\n')
if a[:2]==['inspect','--json']:
 print(json.dumps({'name':a[2],'state':'running'}))
elif a[1]=='inspect':
 if p.exists() and json.loads(p.read_text())['name']==a[2]: print(p.read_text())
 else:
  sys.stderr.write(f'error: mcp server "{a[2]}" not found: mcp server not found\n  try: sbx mcp ls\n'); sys.exit(1)
elif a[1]=='add':
 cmd=a[a.index('--command')+1]
 args=a[a.index('--args')+1].split(',')
 p.write_text(json.dumps({'name':a[2],'type':'local','resolved_command':cmd,'command':[cmd,*args]}))
elif a[1]=='rm': p.unlink()
elif a[1]!='load': sys.exit(64)
"#,
    );
    fs::create_dir(home.join("home")).unwrap();
    let run = |args: &[&str]| {
        let mut command = Command::new(&cli);
        command
            .arg("mcp")
            .args(args)
            .current_dir(&project)
            .env_remove("MARSH_DAEMON_SOCKET")
            .env_remove("MARSH_DAEMON_TOKEN")
            .env("HOME", &host)
            .env("MARSH_HOME", &home)
            .env("MARSH_SBX", &sbx)
            .env("USER", "fixture")
            .env("LOGNAME", "fixture");
        if args.first() == Some(&"publish") {
            // Publication is attached-only. This controlled daemon-side peer
            // supplies the actual host CLI's typed context; it does not mirror
            // validation or registration. The separate daemon relay journey
            // proves real authenticated session admission and lineage.
            let (mut channel, inherited) = UnixStream::pair().unwrap();
            channel
                .set_read_timeout(Some(std::time::Duration::from_secs(20)))
                .unwrap();
            let metadata = fs::metadata(&project).unwrap();
            let context = marsh_daemon::HostPublicationContext {
                session: marsh_daemon::SessionSpec {
                    session_id: "controlled-publisher".into(),
                    username: "fixture".into(),
                    uid: nix::unistd::Uid::effective().as_raw(),
                    gid: nix::unistd::Gid::effective().as_raw(),
                    launch_directory: project.clone(),
                    guest_home: host.clone(),
                    home_backing: home.join("home"),
                    ephemeral_home: false,
                    terminal: false,
                    terminal_size: None,
                },
                project_identity: (metadata.dev(), metadata.ino()),
                kind: marsh_daemon::PublicationKind::Mcp,
                operation: marsh_daemon::PublicationOperation::Publish,
                name: marsh_daemon::PublishedName::parse(args[1]).unwrap(),
                kit: None,
                scope_admitted: false,
                admission_lock: None,
                sandbox: None,
                agent_session_id: None,
                generation: None,
            };
            let child = command
                .env("MARSH_PUBLICATION_CHANNEL", "1")
                .stdin(Stdio::from(std::os::fd::OwnedFd::from(inherited)))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            drop(command);
            marsh_daemon::write_frame(&mut channel, &context).unwrap();
            loop {
                match marsh_daemon::read_frame(&mut channel).unwrap() {
                    marsh_daemon::PublicationHostEvent::RunStock { arguments } => {
                        let output = Command::new(&sbx)
                            .args(arguments)
                            .current_dir(&project)
                            .output()
                            .unwrap();
                        marsh_daemon::write_frame(
                            &mut channel,
                            &Ok::<_, String>(marsh_daemon::PublicationStockOutput {
                                exit_code: output.status.code(),
                                stdout: output.stdout,
                                stderr: output.stderr,
                            }),
                        )
                        .unwrap();
                    }
                    marsh_daemon::PublicationHostEvent::BeginCommit
                    | marsh_daemon::PublicationHostEvent::BeginRollback => {
                        marsh_daemon::write_frame(
                            &mut channel,
                            &Ok::<Option<String>, String>(None),
                        )
                        .unwrap();
                    }
                    marsh_daemon::PublicationHostEvent::Complete(outcome) => {
                        assert!(
                            matches!(outcome, marsh_daemon::PublicationOutcome::Committed(_)),
                            "{outcome:?}"
                        );
                        break;
                    }
                    marsh_daemon::PublicationHostEvent::PrepareKit => {
                        panic!("unexpected Kit preparation for an untargeted publication")
                    }
                }
            }
            child.wait_with_output().unwrap()
        } else {
            command.output().unwrap()
        }
    };
    let result = run(&["publish", "echo", "--", "cat"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let registration = fs::read(root.path().join("registration.json")).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&registration).unwrap();
    let args = value["command"].as_array().unwrap();
    let declaration_path = Path::new(
        args[args.iter().position(|v| v == "--declaration").unwrap() + 1]
            .as_str()
            .unwrap(),
    );
    let bytes = fs::read(declaration_path).unwrap();
    let declaration = ToolDeclaration::load_from_path(declaration_path, Some("echo")).unwrap();
    // Placement fixture only: exporter still parses/binds/executes, and validates
    // the same publication bytes before every call. No Linux host bypass in CLI.
    let launcher = root.path().join("marsh");
    executable(&launcher, "#!/bin/sh\ncat\n");
    executable(&launcher.with_file_name("marshd"), "#!/bin/sh\nexit 0\n");
    let host_config = HostConfig::new(&project, &home, &launcher, &sbx, false).unwrap();
    let config = ExportConfig::new(host_config, declaration)
        .unwrap()
        .with_declaration_path(declaration_path.to_path_buf());
    let (server_socket, client_socket) = tokio::net::UnixStream::pair().unwrap();
    let server = tokio::spawn(async move {
        ExportMcp::new(config)
            .serve(server_socket)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let client = Caller.serve(client_socket).await.unwrap();
    let call = || {
        CallToolRequestParams::new("echo").with_arguments(
            serde_json::json!({"input":"still valid"})
                .as_object()
                .unwrap()
                .clone(),
        )
    };
    assert_eq!(client.list_all_tools().await.unwrap().len(), 1);
    let before = client.call_tool(call()).await.unwrap();
    assert_ne!(before.is_error, Some(true), "{before:?}");
    assert!(
        before.structured_content.as_ref().unwrap()["data"]
            .get("execution")
            .is_none(),
        "pipeline must not fabricate one worker outcome"
    );
    assert_eq!(
        before.structured_content.as_ref().unwrap()["data"]["stdout_base64"],
        "c3RpbGwgdmFsaWQ="
    );
    let loaded = run(&["load", "echo", "--sandbox", "another-target"]);
    assert!(
        loaded.status.success(),
        "{}",
        String::from_utf8_lossy(&loaded.stderr)
    );
    assert_eq!(fs::read(declaration_path).unwrap(), bytes);
    assert_eq!(
        fs::read(root.path().join("registration.json")).unwrap(),
        registration
    );
    let after = client.call_tool(call()).await.unwrap();
    assert_ne!(after.is_error, Some(true), "{after:?}");
    assert_eq!(
        after.structured_content.as_ref().unwrap()["data"]["stdout_base64"],
        "c3RpbGwgdmFsaWQ="
    );
    assert!(run(&["unpublish", "echo"]).status.success());
    let effects = fs::read(root.path().join("effects")).unwrap();
    assert!(
        !run(&["load", "echo", "--sandbox", "another-target"])
            .status
            .success()
    );
    assert_eq!(fs::read(root.path().join("effects")).unwrap(), effects);
    assert!(client.list_all_tools().await.unwrap().is_empty());
    assert_eq!(client.call_tool(call()).await.unwrap().is_error, Some(true));
    client.cancel().await.unwrap();
    server.await.unwrap();
}
