use futures::{StreamExt, future};
use marsh_mcp::{
    AcpDeclaration, AcpExportMcp, HostConfig, HostMcp,
    broker::{self, BrokerOptions, MAX_MCP_FRAME_BYTES},
    default_paths, generated_scope_root_for_workspace,
};
use rmcp::{
    RoleServer, ServiceExt,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::async_rw::JsonRpcMessageCodec,
};
use std::{
    env,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
};
use tokio_util::codec::{FramedRead, FramedWrite};

const HELP: &str = "Usage: marsh-mcp MODE --sbx ABS [--workspace ABS] [--scope-root ABS | --home ABS] [--marsh ABS] [--allow-full-sbx-control] [--declaration PATH] [--expected-generation UUID]\n\nModes:\n  serve             run a direct bounded stdio MCP server (Codex/Claude)\n  export-serve      run an export-only stdio MCP server for one declared Kit command\n  acp-export-serve  run an export-only stdio MCP server for one published ACP session\n  --expected-generation pins export modes to one publication generation\n  broker-start      idempotently ensure the resident host broker is ready\n  broker-stop       stop an idle resident host broker\n  connect           proxy one bounded stdio MCP session to that broker (stock SBX)\n\nserve without --home/--scope-root drives the marsh CLI's selected home (MARSH_HOME or ~/.marsh); scope_start is then disabled.\nBroker modes default to the generated scope root ~/Library/Application Support/marsh-mcp-scopes/<workspace-hash>\n(<MARSH_CONTROL_HOME>-mcp-scopes/<workspace-hash> when MARSH_CONTROL_HOME is set). Export modes require an explicit command home.
Broker modes require --scope-root semantics.\nThe pinned SBX executable is required. Full SBX control is disabled unless explicitly enabled at startup.\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Serve,
    ExportServe,
    AcpExportServe,
    BrokerStart,
    BrokerStop,
    BrokerRun,
    Connect,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("marsh-mcp: {error}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)]
async fn run() -> Result<(), String> {
    let (mut workspace, mut home, mut marsh) = default_paths()?;
    let mut sbx = None;
    let mut scope_root = None;
    let mut home_explicit = false;
    let mut allow_full_sbx_control = false;
    let mut declaration_path = None;
    let mut selected_tool = None;
    let mut expected_generation = None;
    let mut arguments = env::args_os().skip(1);
    let mut mode = match arguments.next() {
        Some(argument) if argument == "serve" => Mode::Serve,
        Some(argument)
            if argument == "export-serve" || argument == "export" || argument == "serve-export" =>
        {
            Mode::ExportServe
        }
        Some(argument) if argument == "acp-export-serve" => Mode::AcpExportServe,
        Some(argument) if argument == "broker-start" => Mode::BrokerStart,
        Some(argument) if argument == "broker-stop" => Mode::BrokerStop,
        Some(argument) if argument == "connect" => Mode::Connect,
        Some(argument) if argument == "_broker-run" => Mode::BrokerRun,
        Some(argument) if argument == "-h" || argument == "--help" => {
            print!("{HELP}");
            return Ok(());
        }
        Some(argument) => {
            return Err(format!(
                "expected `serve` or `export-serve`, got {}\n{HELP}",
                argument.to_string_lossy()
            ));
        }
        None => return Err(HELP.trim_end().to_string()),
    };
    while let Some(argument) = arguments.next() {
        let argument = argument
            .into_string()
            .map_err(|_| "arguments must be valid UTF-8".to_string())?;
        match argument.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            "--workspace" => workspace = next_path(&mut arguments, "--workspace")?,
            "--home" => {
                home = next_path(&mut arguments, "--home")?;
                home_explicit = true;
            }
            "--scope-root" => scope_root = Some(next_path(&mut arguments, "--scope-root")?),
            "--marsh" => marsh = next_path(&mut arguments, "--marsh")?,
            "--sbx" => sbx = Some(next_path(&mut arguments, "--sbx")?),
            "--declaration" | "--export" => {
                declaration_path = Some(next_path(&mut arguments, "--declaration")?);
            }
            "--tool" => {
                selected_tool = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--tool requires a name".to_string())?
                        .into_string()
                        .map_err(|_| "--tool must be valid UTF-8".to_string())?,
                );
            }
            "--expected-generation" => {
                expected_generation = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--expected-generation requires a UUID".to_string())?
                        .into_string()
                        .map_err(|_| "--expected-generation must be UTF-8".to_string())?,
                );
            }
            "--allow-full-sbx-control" => allow_full_sbx_control = true,
            _ => return Err(format!("unknown option: {argument}\n{HELP}")),
        }
    }
    if declaration_path.is_some() && mode == Mode::Serve {
        mode = Mode::ExportServe;
    }
    if expected_generation.is_some() && !matches!(mode, Mode::ExportServe | Mode::AcpExportServe) {
        return Err("--expected-generation requires an export mode".into());
    }
    if let Some(generation) = &expected_generation
        && !uuid::Uuid::parse_str(generation).is_ok_and(|parsed| parsed.to_string() == *generation)
    {
        return Err("--expected-generation must be a canonical UUID".into());
    }
    if matches!(mode, Mode::ExportServe | Mode::AcpExportServe) {
        if !home_explicit || scope_root.is_some() {
            return Err("export-serve requires an explicit --home naming the intended marsh command home; --scope-root is for development control only".into());
        }
        if allow_full_sbx_control {
            return Err("export-serve cannot enable full SBX control".into());
        }
        if home.starts_with(generated_scope_root_for_workspace(&workspace)?) {
            return Err(
                "export-serve cannot use the development-control MCP scope root as --home".into(),
            );
        }
    }
    if home_explicit && scope_root.is_some() {
        return Err("--home and --scope-root are mutually exclusive".into());
    }
    // `serve` with neither --home nor --scope-root drives the same selected
    // home the `marsh` CLI uses (MARSH_HOME or ~/.marsh). Broker modes always
    // need a generated scope root beside the protected host state root.
    if !home_explicit && (mode != Mode::Serve || scope_root.is_some()) {
        let root = scope_root.get_or_insert(generated_scope_root_for_workspace(&workspace)?);
        home = root.join("default");
    }
    let sbx = sbx.ok_or_else(|| "--sbx requires an absolute executable path".to_string())?;
    validate_absolute_paths(&workspace, &home, &marsh, &sbx)?;
    if matches!(mode, Mode::ExportServe | Mode::AcpExportServe) {
        if !cfg!(target_os = "macos") {
            return Err("export-serve requires the macOS host; it cannot run in a guest".into());
        }
        let decl_path = declaration_path.ok_or("export-serve requires --declaration PATH")?;
        let canonical_decl = decl_path
            .canonicalize()
            .map_err(|e| format!("cannot resolve declaration path: {e}"))?;
        let canonical_workspace = workspace
            .canonicalize()
            .map_err(|e| format!("cannot resolve workspace: {e}"))?;
        let canonical_home = home
            .canonicalize()
            .map_err(|e| format!("cannot resolve selected home: {e}"))?;
        if canonical_decl.starts_with(&canonical_workspace)
            || canonical_decl.starts_with(&canonical_home)
        {
            return Err(
                "export declaration must be outside the guest-mounted workspace and selected home"
                    .into(),
            );
        }
        require_private_declaration(&canonical_decl)?;
        if mode == Mode::AcpExportServe {
            let declaration = AcpDeclaration::load_private(&canonical_decl)?;
            if expected_generation.as_deref() != Some(declaration.generation.as_str()) {
                return Err("ACP publication generation changed; reload this tool".into());
            }
            if declaration.canonical_workspace != canonical_workspace {
                return Err("ACP publication belongs to another workspace".into());
            }
            return serve_acp_stdio(AcpExportMcp::new(
                canonical_home,
                canonical_decl,
                declaration,
            )?)
            .await;
        }
        let declaration =
            marsh_mcp::ToolDeclaration::load_from_path(&canonical_decl, selected_tool.as_deref())?;
        if declaration.publication_generation != expected_generation {
            return Err("MCP publication generation changed; reload this tool".into());
        }
        let host_config = HostConfig::new(&workspace, &home, &marsh, &sbx, false)?;
        let export_config = marsh_mcp::ExportConfig::new(host_config, declaration)?
            .with_declaration_path(canonical_decl);
        return serve_export_stdio(marsh_mcp::ExportMcp::new(export_config)).await;
    }
    if mode != Mode::Serve && home_explicit {
        return Err("broker modes do not support legacy --home mode; use --scope-root".into());
    }
    if mode != Mode::Serve {
        let scope_root = scope_root.ok_or_else(|| {
            "broker mode requires a generated or explicit --scope-root".to_owned()
        })?;
        let options = BrokerOptions {
            workspace,
            home,
            scope_root,
            marsh,
            sbx,
            allow_full_sbx_control,
        };
        return match mode {
            Mode::BrokerStart => broker::broker_start(options).await,
            Mode::BrokerStop => broker::broker_stop(options).await,
            Mode::BrokerRun => broker::broker_run(options).await,
            Mode::Connect => broker::connect(options).await,
            Mode::Serve | Mode::ExportServe | Mode::AcpExportServe => unreachable!(),
        };
    }
    let config = if let Some(scope_root) = scope_root {
        if !scope_root.is_absolute() {
            return Err("--scope-root must be an absolute path".into());
        }
        HostConfig::new_with_scope_root(
            &workspace,
            &home,
            &scope_root,
            &marsh,
            &sbx,
            allow_full_sbx_control,
        )?
    } else {
        HostConfig::new(&workspace, &home, &marsh, &sbx, allow_full_sbx_control)?
    };
    serve_stdio(HostMcp::new(config)).await
}

fn require_private_declaration(path: &Path) -> Result<(), String> {
    let owner = rustix::process::geteuid().as_raw();
    let file = std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect export declaration: {error}"))?;
    let parent = path.parent().ok_or("export declaration has no parent")?;
    let directory = std::fs::symlink_metadata(parent)
        .map_err(|error| format!("cannot inspect export declaration directory: {error}"))?;
    if !file.file_type().is_file()
        || file.uid() != owner
        || file.mode() & 0o077 != 0
        || !directory.file_type().is_dir()
        || directory.uid() != owner
        || directory.mode() & 0o077 != 0
    {
        return Err("export declaration and its parent must be owner-only real files".into());
    }
    Ok(())
}

async fn serve_stdio(server: HostMcp) -> Result<(), String> {
    let reader = FramedRead::new(
        tokio::io::stdin(),
        JsonRpcMessageCodec::<RxJsonRpcMessage<RoleServer>>::new_with_max_length(
            MAX_MCP_FRAME_BYTES,
        ),
    )
    .take_while(|result| future::ready(result.is_ok()))
    .filter_map(|result| future::ready(result.ok()));
    let writer = FramedWrite::new(
        tokio::io::stdout(),
        JsonRpcMessageCodec::<TxJsonRpcMessage<RoleServer>>::new_with_max_length(
            MAX_MCP_FRAME_BYTES,
        ),
    );
    let running = server
        .clone()
        .serve((writer, reader))
        .await
        .map_err(|error| format!("cannot start MCP transport: {error}"))?;
    let wait = running
        .waiting()
        .await
        .map_err(|error| format!("MCP transport failed: {error}"));
    let shutdown = server.shutdown_session().await;
    wait?;
    shutdown
}

async fn serve_export_stdio(server: marsh_mcp::ExportMcp) -> Result<(), String> {
    let reader = FramedRead::new(
        tokio::io::stdin(),
        JsonRpcMessageCodec::<RxJsonRpcMessage<RoleServer>>::new_with_max_length(
            MAX_MCP_FRAME_BYTES,
        ),
    )
    .take_while(|result| future::ready(result.is_ok()))
    .filter_map(|result| future::ready(result.ok()));
    let writer = FramedWrite::new(
        tokio::io::stdout(),
        JsonRpcMessageCodec::<TxJsonRpcMessage<RoleServer>>::new_with_max_length(
            MAX_MCP_FRAME_BYTES,
        ),
    );
    let running = server
        .clone()
        .serve((writer, reader))
        .await
        .map_err(|error| format!("cannot start MCP export transport: {error}"))?;
    let wait = running
        .waiting()
        .await
        .map_err(|error| format!("MCP export transport failed: {error}"));
    let shutdown = server.shutdown_server().await;
    wait?;
    shutdown
}

async fn serve_acp_stdio(server: AcpExportMcp) -> Result<(), String> {
    let reader = FramedRead::new(
        tokio::io::stdin(),
        JsonRpcMessageCodec::<RxJsonRpcMessage<RoleServer>>::new_with_max_length(
            MAX_MCP_FRAME_BYTES,
        ),
    )
    .take_while(|result| future::ready(result.is_ok()))
    .filter_map(|result| future::ready(result.ok()));
    let writer = FramedWrite::new(
        tokio::io::stdout(),
        JsonRpcMessageCodec::<TxJsonRpcMessage<RoleServer>>::new_with_max_length(
            MAX_MCP_FRAME_BYTES,
        ),
    );
    let running = server
        .serve((writer, reader))
        .await
        .map_err(|error| format!("cannot start ACP MCP transport: {error}"))?;
    running
        .waiting()
        .await
        .map(|_| ())
        .map_err(|error| format!("ACP MCP transport failed: {error}"))
}

fn validate_absolute_paths(
    workspace: &std::path::Path,
    home: &std::path::Path,
    marsh: &std::path::Path,
    sbx: &std::path::Path,
) -> Result<(), String> {
    for (label, path) in [
        ("workspace", workspace),
        ("home", home),
        ("marsh", marsh),
        ("sbx", sbx),
    ] {
        if !path.is_absolute() {
            return Err(format!("--{label} must be an absolute path"));
        }
    }
    Ok(())
}

fn next_path(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
    option: &str,
) -> Result<PathBuf, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{option} requires a path"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::{bytes::BytesMut, codec::Decoder};

    #[test]
    fn stdio_codec_rejects_oversized_frames() {
        let mut codec = JsonRpcMessageCodec::<RxJsonRpcMessage<RoleServer>>::new_with_max_length(
            MAX_MCP_FRAME_BYTES,
        );
        let mut frame = BytesMut::from(vec![b'x'; MAX_MCP_FRAME_BYTES + 1].as_slice());
        frame.extend_from_slice(b"\n");
        assert!(codec.decode(&mut frame).is_err());
    }
}
