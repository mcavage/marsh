//! Argv allowlist for forwarded stock `sbx` calls (docs/design/self-development.md).
//!
//! Pure classification: the broker supplies the grant view and performs the
//! stock call, inventory checks, and descriptor identity checks.

use std::{
    collections::BTreeMap,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

/// What the broker must check and do for one admitted call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// `version`, `--help`, `create --help`: no VM is named.
    PassThrough,
    /// `ls [--json]`: rows filtered to this grant.
    List { json: bool },
    /// `create --name N ...`: persist N as a grant intent first.
    Create {
        name: String,
        sources: Vec<PathBuf>,
        template: Option<TemplatePin>,
    },
    /// `run`: a fresh child (`reattach == false`, persisted as an intent
    /// like `create`) or a reattach to this grant's own VM. `interactive`
    /// (not `--detached`) gets the caller's terminal as a PTY.
    Run {
        name: String,
        reattach: bool,
        interactive: bool,
        sources: Vec<PathBuf>,
        template: Option<TemplatePin>,
    },
    /// A verb on one VM that must be this grant's, with its recorded UUID.
    Own {
        name: String,
        verb: Verb,
        sources: Vec<PathBuf>,
    },
}

/// How an admitted `--template` value reaches stock. Stock resolves a
/// locally loaded template only by its import tag, so a local template named
/// by `tag@digest` (or by tag alone) is forwarded as the tag and the created
/// VM's image digest is checked afterwards, as the product does for its own
/// shell template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplatePin {
    /// Index of the template value in the forwarded argv.
    pub index: usize,
    /// The value stock receives.
    pub forward: String,
    /// The digest the created VM's image must have (`None`: stock resolves
    /// the digest-pinned reference itself).
    pub digest: Option<String>,
}

impl TemplatePin {
    /// The argv stock receives.
    #[must_use]
    pub fn rewrite(&self, argv: &[String]) -> Vec<String> {
        let mut forwarded = argv.to_vec();
        if let Some(value) = forwarded.get_mut(self.index) {
            value.clone_from(&self.forward);
        }
        forwarded
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verb {
    Exec,
    Mount,
    Umount,
    Cp,
    Stop,
    Rm,
    Inspect,
}

/// The grant facts the policy needs.
pub struct GrantView<'a> {
    pub prefix: &'a str,
    pub roots: &'a [PathBuf],
    pub max_vms: usize,
    pub names: &'a BTreeMap<String, Option<String>>,
    /// Shell templates a child may create from (the host's own).
    pub templates: &'a [String],
    /// The caller's working directory; relative host paths resolve here
    /// (the daemon mounts `.` from inside the admitted source).
    pub cwd: Option<&'a Path>,
}

/// Resolve a host path argument the way stock does (against the cwd).
fn resolve(path: &str, grant: &GrantView<'_>) -> PathBuf {
    let path = Path::new(path);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(cwd) = grant.cwd {
        cwd.join(path)
    } else {
        return path.to_path_buf();
    };
    joined
        .components()
        .filter(|part| !matches!(part, Component::CurDir))
        .collect()
}

fn refuse<T>(message: impl Into<String>) -> Result<T, String> {
    Err(message.into())
}

/// `^prefix(x[0-9a-z]{5}-){0,2}[skr]-[0-9a-z]{8}$` (`r`: an `sbx run` child).
#[must_use]
pub fn child_name_ok(prefix: &str, name: &str) -> bool {
    let Some(mut rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let base36 = |bytes: &[u8]| {
        bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
    };
    for _ in 0..=2 {
        let bytes = rest.as_bytes();
        if bytes.len() == 10 && matches!(bytes[0], b's' | b'k' | b'r') && bytes[1] == b'-' {
            return base36(&bytes[2..]);
        }
        if bytes.len() > 7 && bytes[0] == b'x' && bytes[6] == b'-' && base36(&bytes[1..6]) {
            rest = &rest[7..];
        } else {
            return false;
        }
    }
    false
}

/// Lexical root check: absolute, normalized, and a descendant of a root.
#[must_use]
pub fn under_roots(path: &Path, roots: &[PathBuf]) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
        && roots.iter().any(|root| path.starts_with(root))
}

/// Filesystem identity of an admitted source: it must resolve to itself (no
/// symlink anywhere on the path) and lie under a root. Checked before and
/// after the stock call; a mismatch revokes the grant.
///
/// # Errors
/// Returns a refusal message for an unsafe or missing source.
pub fn source_identity(path: &Path, roots: &[PathBuf]) -> Result<(u64, u64), String> {
    if !under_roots(path, roots) {
        return refuse(format!("{} is outside the grant roots", path.display()));
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if canonical != path {
        return refuse(format!("{} is not a symlink-free path", path.display()));
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    Ok((metadata.dev(), metadata.ino()))
}

fn env_pair_ok(value: &str) -> Result<(), String> {
    match value.split_once('=') {
        Some((key, _)) if !key.is_empty() => Ok(()),
        _ => refuse("bare -e KEY (host environment) is refused; use -e KEY=VALUE"),
    }
}

fn host_side(spec: &str, grant: &GrantView<'_>) -> PathBuf {
    // HOST[:CTR[:ro|rw]] — host paths here never contain ':'.
    resolve(spec.split(':').next().unwrap_or(spec), grant)
}

fn own(name: &str, grant: &GrantView<'_>) -> Result<String, String> {
    if child_name_ok(grant.prefix, name) && grant.names.contains_key(name) {
        Ok(name.to_owned())
    } else {
        refuse(format!("{name} is not a VM of this development grant"))
    }
}

/// Classify one forwarded argv.
///
/// # Errors
/// Returns the refusal message for anything outside the allowlist.
#[allow(clippy::too_many_lines)] // One table, one match.
pub fn classify(argv: &[String], grant: &GrantView<'_>) -> Result<Action, String> {
    let words = argv.iter().map(String::as_str).collect::<Vec<_>>();
    match words.as_slice() {
        ["version" | "--help"] | ["create" | "run", "--help"] => Ok(Action::PassThrough),
        ["ls"] => Ok(Action::List { json: false }),
        ["ls", "--json"] => Ok(Action::List { json: true }),
        ["create", rest @ ..] => classify_create(rest, grant),
        ["run", rest @ ..] => classify_run(rest, grant),
        ["exec", rest @ ..] => {
            let mut index = 0;
            while index < rest.len() {
                match rest[index] {
                    "-i" | "-t" | "-it" | "-ti" | "--interactive" | "--tty" => {}
                    "-u" | "--user" | "-w" | "--workdir" | "--detach-keys" => {
                        index += 1;
                        if index >= rest.len() {
                            return refuse("exec option is missing its value");
                        }
                    }
                    "-e" | "--env" => {
                        index += 1;
                        env_pair_ok(rest.get(index).copied().unwrap_or(""))?;
                    }
                    option if option.starts_with('-') => {
                        return refuse(format!("exec option {option} is refused"));
                    }
                    name => {
                        if index + 1 >= rest.len() {
                            return refuse("exec requires a command");
                        }
                        return Ok(Action::Own {
                            name: own(name, grant)?,
                            verb: Verb::Exec,
                            sources: Vec::new(),
                        });
                    }
                }
                index += 1;
            }
            refuse("exec requires a sandbox")
        }
        [verb @ ("mount" | "umount"), name, spec] => {
            let source = host_side(spec, grant);
            if !under_roots(&source, grant.roots) {
                return refuse(format!(
                    "{verb} source {} is outside the grant roots",
                    source.display()
                ));
            }
            if spec.split(':').count() > 3
                || spec
                    .split(':')
                    .nth(2)
                    .is_some_and(|mode| mode != "ro" && mode != "rw")
            {
                return refuse(format!("{verb} spec {spec} is malformed"));
            }
            Ok(Action::Own {
                name: own(name, grant)?,
                verb: if *verb == "mount" {
                    Verb::Mount
                } else {
                    Verb::Umount
                },
                sources: vec![source],
            })
        }
        ["cp", source, destination] => {
            if source.contains(':') {
                return refuse("cp from a VM to the host is refused");
            }
            let source = &resolve(source, grant);
            if !under_roots(source, grant.roots) {
                return refuse(format!(
                    "cp source {} is outside the grant roots",
                    source.display()
                ));
            }
            let Some((name, target)) = destination.split_once(':') else {
                return refuse("cp destination must be VM:PATH");
            };
            if !target.starts_with('/') {
                return refuse("cp destination path must be absolute");
            }
            Ok(Action::Own {
                name: own(name, grant)?,
                verb: Verb::Cp,
                sources: vec![source.clone()],
            })
        }
        ["stop", name] => Ok(Action::Own {
            name: own(name, grant)?,
            verb: Verb::Stop,
            sources: Vec::new(),
        }),
        ["rm", name] | ["rm", "--force" | "-f", name] => Ok(Action::Own {
            name: own(name, grant)?,
            verb: Verb::Rm,
            sources: Vec::new(),
        }),
        ["inspect", name] | ["inspect", "--json", name] => Ok(Action::Own {
            name: own(name, grant)?,
            verb: Verb::Inspect,
            sources: Vec::new(),
        }),
        [verb, ..] => refuse(format!(
            "sbx {verb} is not available to a development grant"
        )),
        [] => refuse("empty sbx command"),
    }
}

fn classify_create(rest: &[&str], grant: &GrantView<'_>) -> Result<Action, String> {
    let mut name = None;
    let mut positionals = Vec::new();
    let mut template = None;
    let mut index = 0;
    while index < rest.len() {
        let word = rest[index];
        // argv[0] is the verb; the option's value follows it.
        let value_index = index + 2;
        let mut value = || {
            index += 1;
            rest.get(index)
                .copied()
                .ok_or_else(|| format!("create option {word} is missing its value"))
        };
        match word {
            "--quiet" | "-q" => {}
            "--pull" | "--cpus" | "-m" | "--memory" | "--deny-network" => {
                value()?;
            }
            "--skills" => skills_ok(value()?)?,
            "--name" => name = Some(value()?.to_owned()),
            "--template" | "-t" => template = Some((value_index, value()?)),
            "-e" | "--env" => env_pair_ok(value()?)?,
            option if option.starts_with('-') => {
                return refuse(format!("create option {option} is refused"));
            }
            positional => positionals.push(positional),
        }
        index += 1;
    }
    let Some(name) = name else {
        return refuse("create requires --name with this grant's prefix");
    };
    fresh_name_ok("create", &name, grant)?;
    let template = template_ok(template, grant)?;
    let Some((agent, workspaces)) = positionals.split_first() else {
        return refuse("create requires an agent");
    };
    let mut sources = Vec::new();
    agent_ok("create", agent, false, grant, &mut sources)?;
    workspaces_ok(workspaces, grant, &mut sources)?;
    Ok(Action::Create {
        name,
        sources,
        template,
    })
}

/// Stock's built-in agents. `sbx run` starts them with stock's own
/// proxy-managed credentials; no raw credential reaches the grant.
const BUILTIN_AGENTS: [&str; 12] = [
    "claude",
    "claude-bedrock",
    "codex",
    "copilot",
    "cursor",
    "devin",
    "docker-agent",
    "droid",
    "gemini",
    "kiro",
    "opencode",
    "shell",
];

/// `readwrite` would let a child write the host's shared skills store.
fn skills_ok(mode: &str) -> Result<(), String> {
    match mode {
        "off" | "readonly" => Ok(()),
        _ => refuse(format!("--skills {mode} is refused (off or readonly)")),
    }
}

fn fresh_name_ok(verb: &str, name: &str, grant: &GrantView<'_>) -> Result<(), String> {
    if !child_name_ok(grant.prefix, name) {
        return refuse(format!(
            "{verb} name {name} does not match the grant prefix {}",
            grant.prefix
        ));
    }
    if grant.names.contains_key(name) {
        return refuse(format!("{name} already exists in this grant"));
    }
    if grant.names.len() >= grant.max_vms {
        return refuse(format!(
            "development grant is at its limit of {} VMs",
            grant.max_vms
        ));
    }
    Ok(())
}

/// `template` is `(argv index of the value, value)`.
fn template_ok(
    template: Option<(usize, &str)>,
    grant: &GrantView<'_>,
) -> Result<Option<TemplatePin>, String> {
    let Some((index, template)) = template else {
        return Ok(None);
    };
    // A local template may be named by its tag alone (`--pull never`).
    let Some(allowed) = grant.templates.iter().find(|allowed| {
        *allowed == template
            || allowed
                .split_once('@')
                .is_some_and(|(tag, _)| tag == template)
    }) else {
        return refuse(format!(
            "template {template} is not the host shell template"
        ));
    };
    let local = marsh_sbx::ShellTemplateReference::parse(allowed)
        .is_ok_and(|reference| reference.requires_local_authority());
    let pin = match allowed.split_once('@') {
        Some((tag, digest)) if local => TemplatePin {
            index,
            forward: tag.to_owned(),
            digest: Some(digest.to_owned()),
        },
        _ => TemplatePin {
            index,
            forward: template.to_owned(),
            digest: None,
        },
    };
    Ok(Some(pin))
}

fn agent_ok(
    verb: &str,
    agent: &str,
    builtins: bool,
    grant: &GrantView<'_>,
    sources: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if agent == "shell" || (builtins && BUILTIN_AGENTS.contains(&agent)) {
    } else if let Some(reference) = agent.strip_prefix("oci://") {
        if !reference.contains("@sha256:") {
            return refuse(format!("{verb} OCI Kit references must be digest-pinned"));
        }
    } else if agent.starts_with('/')
        || agent.starts_with("./")
        || agent.starts_with("../")
        || agent == "."
    {
        let kit = resolve(agent, grant);
        if !under_roots(&kit, grant.roots) {
            return refuse(format!("Kit directory {agent} is outside the grant roots"));
        }
        sources.push(kit);
    } else {
        return refuse(format!("{verb} agent {agent} is refused"));
    }
    Ok(())
}

fn workspaces_ok(
    workspaces: &[&str],
    grant: &GrantView<'_>,
    sources: &mut Vec<PathBuf>,
) -> Result<(), String> {
    for workspace in workspaces {
        let path = workspace
            .strip_suffix(":ro")
            .or_else(|| workspace.strip_suffix(":rw"))
            .unwrap_or(workspace);
        let resolved = resolve(path, grant);
        if !under_roots(&resolved, grant.roots) {
            return refuse(format!("workspace {path} is outside the grant roots"));
        }
        sources.push(resolved);
    }
    Ok(())
}

/// Give a `run` without `--name` a fresh child name (`<prefix>r-<8>`), so
/// stock never picks its `<agent>-<workdir>` default or reuses a host VM.
#[must_use]
pub fn assign_run_name(argv: &[String], fresh: &str) -> Vec<String> {
    let options = argv.iter().take_while(|word| *word != "--");
    if argv.first().is_none_or(|verb| verb != "run")
        || options
            .clone()
            .any(|word| word == "--name" || word.starts_with("--name="))
    {
        return argv.to_vec();
    }
    let mut assigned = Vec::with_capacity(argv.len() + 2);
    assigned.push(argv[0].clone());
    assigned.push("--name".to_owned());
    assigned.push(fresh.to_owned());
    assigned.extend(argv[1..].iter().cloned());
    assigned
}

/// `sbx run [flags] [AGENT] [PATH...] [-- AGENT_ARGS...]`. Agent arguments
/// pass through unread. Every host path (Kit dir, workspaces, or the
/// implicit current directory) must be under the roots.
fn classify_run(rest: &[&str], grant: &GrantView<'_>) -> Result<Action, String> {
    let options_end = rest
        .iter()
        .position(|word| *word == "--")
        .unwrap_or(rest.len());
    let rest = &rest[..options_end];
    let mut name = None;
    let mut positionals = Vec::new();
    let mut template = None;
    let mut detached = false;
    let mut index = 0;
    while index < rest.len() {
        let word = rest[index];
        // argv[0] is the verb; the option's value follows it.
        let value_index = index + 2;
        let mut value = || {
            index += 1;
            rest.get(index)
                .copied()
                .ok_or_else(|| format!("run option {word} is missing its value"))
        };
        match word {
            "-d" | "--detached" => detached = true,
            "--pull" | "--cpus" | "-m" | "--memory" | "--deny-network" => {
                value()?;
            }
            "--skills" => skills_ok(value()?)?,
            "--name" => name = Some(value()?.to_owned()),
            "--template" | "-t" => template = Some((value_index, value()?)),
            "-e" | "--env" => env_pair_ok(value()?)?,
            option if option.starts_with('-') => {
                return refuse(format!("run option {option} is refused"));
            }
            positional => positionals.push(positional),
        }
        index += 1;
    }
    let Some(name) = name else {
        return refuse("run requires --name with this grant's prefix");
    };
    let reattach = grant.names.contains_key(&name);
    if reattach {
        own(&name, grant)?;
    } else {
        fresh_name_ok("run", &name, grant)?;
    }
    let template = template_ok(template, grant)?;
    let mut sources = Vec::new();
    let workspaces = match positionals.split_first() {
        Some((agent, workspaces)) => {
            agent_ok("run", agent, true, grant, &mut sources)?;
            workspaces
        }
        None if reattach => &[][..],
        None => return refuse("run requires an agent for a new sandbox"),
    };
    workspaces_ok(workspaces, grant, &mut sources)?;
    if workspaces.is_empty() && !reattach {
        // Stock mounts the current directory: the caller's, which must be
        // a host path under the roots (normally the natural project path).
        match grant.cwd {
            Some(cwd) if under_roots(cwd, grant.roots) => sources.push(cwd.to_path_buf()),
            _ => {
                return refuse(
                    "run without a workspace mounts the current directory, which is outside the grant roots",
                );
            }
        }
    }
    Ok(Action::Run {
        name,
        reattach,
        interactive: !detached,
        sources,
        template,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "marsh-xab12c-";
    const OWN: &str = "marsh-xab12c-s-0123abcd";
    const TEMPLATE: &str = "docker.io/library/marsh-shell-local:t@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TEMPLATE_TAG: &str = "docker.io/library/marsh-shell-local:t";

    fn names() -> BTreeMap<String, Option<String>> {
        BTreeMap::from([(OWN.to_owned(), Some("uuid".to_owned()))])
    }

    fn check(argv: &str, names: &BTreeMap<String, Option<String>>) -> Result<Action, String> {
        let roots = [
            PathBuf::from("/Users/u/dev/p"),
            PathBuf::from("/Users/u/Library/Caches/marsh/dev/k"),
        ];
        let templates = [TEMPLATE.to_owned()];
        let argv = argv.split(' ').map(str::to_owned).collect::<Vec<_>>();
        classify(
            &argv,
            &GrantView {
                prefix: PREFIX,
                roots: &roots,
                max_vms: 2,
                names,
                templates: &templates,
                cwd: Some(Path::new("/Users/u/dev/p/sub")),
            },
        )
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One table.
    fn allowed_and_refused_table() {
        let names = names();
        let allowed = [
            "version",
            "--help",
            "create --help",
            "ls",
            "ls --json",
            "create --quiet --name marsh-xab12c-k-zzzzzzzz --pull missing --skills off -e MARSH_SELECTED_HOME=/x oci://r/k@sha256:00 /Users/u/Library/Caches/marsh/dev/k/control/w",
            &format!(
                "create --quiet --name marsh-xab12c-s-zzzzzzzz --pull never --skills off --template {TEMPLATE} shell"
            ),
            "create --name marsh-xab12c-xqqqqq-s-zzzzzzzz shell /Users/u/dev/p:ro",
            &format!(
                "create --name marsh-xab12c-s-zzzzzzzz --pull never --template {TEMPLATE_TAG} shell"
            ),
            &format!("exec -i -u root -w / {OWN} /usr/local/bin/marsh --internal-supervisor"),
            &format!("exec -u root -e K=V {OWN} id --env-file x"),
            &format!("mount {OWN} /Users/u/dev/p:/Users/u/dev/p:rw"),
            &format!("mount {OWN} ."),
            &format!("mount {OWN} .:/home/u:ro"),
            &format!("umount {OWN} /Users/u/Library/Caches/marsh/dev/k/home:/home/u"),
            &format!("cp /Users/u/Library/Caches/marsh/dev/k/artifacts/w {OWN}:/tmp/w"),
            &format!("stop {OWN}"),
            &format!("rm --force {OWN}"),
            &format!("inspect --json {OWN}"),
            "run --help",
            "run --name marsh-xab12c-r-zzzzzzzz -d shell",
            "run --name marsh-xab12c-r-zzzzzzzz codex",
            "run --name marsh-xab12c-r-zzzzzzzz --skills off -e K=V --cpus 2 -m 4g claude /Users/u/dev/p /Users/u/dev/p/docs:ro -- --continue --env-file /etc/passwd",
            "run --name marsh-xab12c-r-zzzzzzzz -d oci://r/k@sha256:00 /Users/u/dev/p",
            "run --name marsh-xab12c-r-zzzzzzzz ./kit",
            &format!("run --name marsh-xab12c-r-zzzzzzzz -t {TEMPLATE_TAG} shell"),
            &format!("run --name {OWN}"),
            &format!("run --name {OWN} -d shell"),
        ];
        for argv in allowed {
            assert!(
                check(argv, &names).is_ok(),
                "{argv}: {:?}",
                check(argv, &names)
            );
        }
        let refused = [
            "create --name marsh-xab12c-s-zzzzzzzz --env-file /etc/passwd shell",
            "create --name marsh-xab12c-s-zzzzzzzz -e HOME shell",
            "create --name marsh-xab12c-s-zzzzzzzz --kit /Users/u/dev/p shell",
            "create --name marsh-xab12c-s-zzzzzzzz -p 80:80 shell",
            "create --name marsh-xab12c-s-zzzzzzzz --clone shell",
            "create --name marsh-xab12c-s-zzzzzzzz --template other shell",
            "create --name marsh-xab12c-s-zzzzzzzz --template docker.io/library/marsh-shell-local shell",
            "create --name marsh-xab12c-s-zzzzzzzz claude",
            "create --name marsh-xab12c-s-zzzzzzzz oci://r/k:latest",
            "create --name marsh-xab12c-s-zzzzzzzz shell /Users/u/.ssh",
            "create --name marsh-xab12c-s-zzzzzzzz shell /Users/u/dev/p/../../.ssh",
            "create --name marsh-s-zzzzzzzz shell",
            "create --name marsh-xother-s-zzzzzzzz shell",
            "create --name marsh-xab12c-xa-xb-xc-s-zzzzzzzz shell",
            "create shell",
            &format!("create --name {OWN} shell"),
            "exec -i marsh-s-00000000 sh",
            "exec -i claude-marsh sh",
            &format!("exec --env-file /x {OWN} sh"),
            &format!("exec -e HOME {OWN} sh"),
            &format!("exec --privileged {OWN} sh"),
            &format!("mount {OWN} /Users/u/.ssh:/x"),
            &format!("mount {OWN} ../../..:/x"),
            &format!("cp {OWN}:/etc/passwd /Users/u/dev/p/x"),
            &format!("cp -L /Users/u/dev/p/x {OWN}:/x"),
            "rm --force agent-c084f19e1bacb49f-project-b9fccda4",
            "stop marsh-xab12c-s-99999999",
            "template load x",
            "mcp ls",
            "policy ls",
            "secret ls",
            "ports",
            "run claude",
            "run --name marsh-xab12c-r-zzzzzzzz",
            "run --name claude-marsh codex",
            "run --name marsh-xab12c-s-99999999 --cloud codex",
            "run --name marsh-xab12c-r-zzzzzzzz --clone codex",
            "run --name marsh-xab12c-r-zzzzzzzz --kit ./mixin codex",
            "run --name marsh-xab12c-r-zzzzzzzz --kit-arg a=b codex",
            "run --name marsh-xab12c-r-zzzzzzzz --env-file /etc/passwd codex",
            "run --name marsh-xab12c-r-zzzzzzzz -e OPENAI_API_KEY codex",
            "run --name marsh-xab12c-r-zzzzzzzz --profile p codex",
            "run --name marsh-xab12c-r-zzzzzzzz -p 80:80 codex",
            "run --name marsh-xab12c-r-zzzzzzzz --static-mcp notion codex",
            "run --name marsh-xab12c-r-zzzzzzzz --skills readwrite codex",
            "run --name marsh-xab12c-r-zzzzzzzz --template other codex",
            "run --name marsh-xab12c-r-zzzzzzzz codex /Users/u/.ssh",
            "run --name marsh-xab12c-r-zzzzzzzz codex /Users/u/.ssh:ro",
            "run --name marsh-xab12c-r-zzzzzzzz /Users/u/kit",
            "run --name marsh-xab12c-r-zzzzzzzz codex ../..",
            "run --name marsh-xab12c-r-zzzzzzzz docker.io/foo/agent:latest",
            "run --name marsh-xab12c-r-zzzzzzzz https://github.com/x/kit.git",
            "run --name marsh-xab12c-r-zzzzzzzz oci://r/k:latest",
            "run --name marsh-xab12c-r-zzzzzzzz agent-c084f19e1bacb49f-project-b9fccda4",
            "create --name marsh-xab12c-s-zzzzzzzz --skills readwrite shell",
            "--cloud run -d codex",
        ];
        for argv in refused {
            assert!(check(argv, &names).is_err(), "{argv} was admitted");
        }
    }

    #[test]
    fn run_names_reattach_and_current_directory() {
        let names = names();
        let argv = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let fresh = "marsh-xab12c-r-00000000";
        assert_eq!(
            assign_run_name(&argv("run -d codex -- --name x"), fresh),
            argv(&format!("run --name {fresh} -d codex -- --name x"))
        );
        assert_eq!(
            assign_run_name(&argv(&format!("run --name {OWN}")), fresh),
            argv(&format!("run --name {OWN}"))
        );
        assert_eq!(assign_run_name(&argv("ls"), fresh), argv("ls"));
        assert_eq!(
            check(&format!("run --name {fresh} codex"), &names),
            Ok(Action::Run {
                name: fresh.into(),
                reattach: false,
                interactive: true,
                sources: vec![PathBuf::from("/Users/u/dev/p/sub")],
                template: None,
            })
        );
        assert_eq!(
            check(&format!("run --name {OWN} -d"), &names),
            Ok(Action::Run {
                name: OWN.into(),
                reattach: true,
                interactive: false,
                sources: Vec::new(),
                template: None,
            })
        );
        // Without a host-valid cwd the implicit workspace is refused.
        let roots = [PathBuf::from("/Users/u/dev/p")];
        let outside = GrantView {
            prefix: PREFIX,
            roots: &roots,
            max_vms: 2,
            names: &names,
            templates: &[],
            cwd: Some(Path::new("/Users/u")),
        };
        assert!(classify(&argv(&format!("run --name {fresh} codex")), &outside).is_err());
        let unknown = GrantView {
            cwd: None,
            ..outside
        };
        assert!(classify(&argv(&format!("run --name {fresh} codex")), &unknown).is_err());
        assert!(
            classify(
                &argv(&format!("run --name {fresh} codex /Users/u/dev/p")),
                &unknown
            )
            .is_ok()
        );
    }

    #[test]
    fn local_template_is_forwarded_by_tag_and_pinned_by_digest() {
        let names = names();
        let argv = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let pinned = TemplatePin {
            index: 6,
            forward: TEMPLATE_TAG.into(),
            digest: Some(format!("sha256:{}", "a".repeat(64))),
        };
        for value in [TEMPLATE, TEMPLATE_TAG] {
            let line = format!(
                "create --name marsh-xab12c-s-zzzzzzzz --pull never --template {value} shell"
            );
            let Ok(Action::Create { template, .. }) = check(&line, &names) else {
                panic!("{line} was refused");
            };
            assert_eq!(template.as_ref(), Some(&pinned), "{line}");
            assert_eq!(
                template.unwrap().rewrite(&argv(&line)),
                argv(&format!(
                    "create --name marsh-xab12c-s-zzzzzzzz --pull never --template {TEMPLATE_TAG} shell"
                ))
            );
        }
        let line = format!("run --name marsh-xab12c-r-zzzzzzzz -d -t {TEMPLATE} shell");
        let Ok(Action::Run { template, .. }) = check(&line, &names) else {
            panic!("{line} was refused");
        };
        assert_eq!(template, Some(TemplatePin { index: 5, ..pinned }));
        // A published digest-pinned template reaches stock unchanged.
        let published = ["docker.io/marsh/shell:v1@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned()];
        let grant = GrantView {
            prefix: PREFIX,
            roots: &[PathBuf::from("/Users/u/dev/p")],
            max_vms: 2,
            names: &names,
            templates: &published,
            cwd: None,
        };
        let line = "create --name marsh-xab12c-s-zzzzzzzz --template docker.io/marsh/shell:v1@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb shell";
        let Ok(Action::Create { template, .. }) = classify(&argv(line), &grant) else {
            panic!("{line} was refused");
        };
        assert_eq!(
            template,
            Some(TemplatePin {
                index: 4,
                forward: published[0].clone(),
                digest: None,
            })
        );
    }

    #[test]
    fn capacity_counts_intents_and_owned_names() {
        let mut names = names();
        names.insert("marsh-xab12c-k-11111111".into(), None);
        let error = check("create --name marsh-xab12c-s-zzzzzzzz shell", &names).unwrap_err();
        assert!(error.contains("limit"), "{error}");
        let error = check("run --name marsh-xab12c-r-zzzzzzzz -d shell", &names).unwrap_err();
        assert!(error.contains("limit"), "{error}");
        assert!(check(&format!("run --name {OWN} -d"), &names).is_ok());
    }

    #[test]
    fn source_identity_rejects_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();
        let roots = [root.clone()];
        assert!(source_identity(&root.join("real"), &roots).is_ok());
        assert!(source_identity(&root.join("escape"), &roots).is_err());
        assert!(source_identity(Path::new("/etc"), &roots).is_err());
    }
}
