//! `xdg-open URL` from a shell or job: the one host action a guest can ask
//! for without a registered command (`docs/security.md`). The daemon opens an
//! `http(s)` link with the Mac's opener only for a session whose terminal is
//! attached right now, and only a few times per window.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// At most this many links per session in each [`WINDOW`].
pub const BURST: usize = 3;
pub const WINDOW: Duration = Duration::from_secs(10);
/// Origins a receipt keeps (`JobReceipt::opened`).
pub const MAX_RECORDED: usize = 16;
/// `open_urls` in `$MARSH_HOME/config.json`, written by `marsh config open-urls`.
pub const CONFIG_KEY: &str = "open_urls";

/// How this daemon opens a link on the Mac.
#[derive(Clone, Debug)]
pub struct Opener {
    /// `/usr/bin/open`, or `MARSH_OPEN_COMMAND` at daemon start.
    pub program: PathBuf,
    /// `MARSH_OPEN_URLS=0` (or `off`, `false`, `no`) at daemon start.
    pub disabled_by_environment: bool,
    /// The scope's `config.json`, read on each request.
    pub config: PathBuf,
}

impl Opener {
    /// The production opener: `MARSH_OPEN_COMMAND` when set, else macOS
    /// `open`; `None` where neither exists.
    #[must_use]
    pub fn from_environment(
        command: Option<&OsStr>,
        switch: Option<&OsStr>,
        scope_root: &Path,
    ) -> Option<Self> {
        let program = match command.filter(|value| !value.is_empty()) {
            Some(program) => PathBuf::from(program),
            None if cfg!(target_os = "macos") => PathBuf::from("/usr/bin/open"),
            None => return None,
        };
        Some(Self {
            program,
            disabled_by_environment: switch.is_some_and(|value| {
                matches!(
                    value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
                    "0" | "off" | "false" | "no"
                )
            }),
            config: scope_root.join("config.json"),
        })
    }

    fn enabled(&self) -> bool {
        !self.disabled_by_environment && configured(&self.config)
    }
}

/// `open_urls` in a scope's `config.json`; missing or unreadable is on.
#[must_use]
pub fn configured(config: &Path) -> bool {
    std::fs::read(config)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.get(CONFIG_KEY).and_then(serde_json::Value::as_bool))
        .unwrap_or(true)
}

/// Which sessions and job trees are on a terminal now, and recent opens.
#[derive(Debug, Default)]
pub(crate) struct Links {
    pub(crate) opener: Option<Opener>,
    /// Sessions whose shell is running on an interactive terminal.
    pub(crate) terminal_shells: BTreeSet<String>,
    /// Root jobs a terminal session started on its terminal.
    pub(crate) terminal_jobs: BTreeSet<String>,
    recent: BTreeMap<String, VecDeque<Instant>>,
}

impl Links {
    /// Take one open from `session`'s budget, or say why not.
    pub(crate) fn admit(&mut self, session: &str, now: Instant) -> Result<PathBuf, String> {
        let Some(opener) = &self.opener else {
            return Err("this marsh cannot open links on the host".into());
        };
        if !opener.enabled() {
            return Err("opening links is turned off (marsh config open-urls)".into());
        }
        let program = opener.program.clone();
        let recent = self.recent.entry(session.to_owned()).or_default();
        while recent
            .front()
            .is_some_and(|opened| now.duration_since(*opened) >= WINDOW)
        {
            recent.pop_front();
        }
        if recent.len() >= BURST {
            return Err(format!(
                "more than {BURST} links in {} seconds",
                WINDOW.as_secs()
            ));
        }
        recent.push_back(now);
        Ok(program)
    }

    pub(crate) fn forget_session(&mut self, session: &str) {
        self.terminal_shells.remove(session);
        self.recent.remove(session);
    }
}

/// Who asked: the transport's session (a shell's relay) or job (`cap.sock`).
#[derive(Clone, Copy, Debug)]
pub enum Caller<'a> {
    Session(&'a str),
    Job(&'a str),
}

impl crate::DaemonStore {
    /// Record whether `session`'s shell is on an interactive terminal now.
    pub fn set_terminal_shell(&self, session: &str, attached: bool) {
        let mut state = self.lock();
        if attached {
            state.links.terminal_shells.insert(session.to_owned());
        } else {
            state.links.forget_session(session);
        }
    }

    /// A root job a terminal session started on its terminal: it and its
    /// descendants may open links while it runs.
    pub fn mark_terminal_job(&self, job_id: &str) {
        let mut state = self.lock();
        let crate::State { jobs, links, .. } = &mut *state;
        links
            .terminal_jobs
            .retain(|id| jobs.get(id).is_some_and(running));
        links.terminal_jobs.insert(job_id.to_owned());
    }

    /// Install how this daemon opens links (`None`: never).
    pub fn set_url_opener(&self, opener: Option<Opener>) {
        self.lock().links.opener = opener;
    }

    /// Serve `OpenUrl`: open an accepted link on the Mac when the caller is
    /// on an attached terminal and within budget; otherwise say why not.
    pub(crate) fn open_url(&self, caller: Caller<'_>, url: &str) -> crate::PublicReply {
        let refused = |reason: String| crate::PublicReply::UrlOpen {
            opened: false,
            reason: Some(reason),
        };
        if let Some(reason) = marsh_contracts::terminal::url_refusal(url) {
            return refused(reason.into());
        }
        let admitted = {
            let mut state = self.lock();
            let (session, job) = match caller {
                Caller::Session(session) => (session.to_owned(), None),
                Caller::Job(job) => {
                    let Some(receipt) = state.jobs.get(job) else {
                        return refused("the job is gone".into());
                    };
                    let root = receipt
                        .lineage
                        .as_ref()
                        .map_or(job, |lineage| lineage.root.as_str());
                    let on_terminal = receipt.placement == crate::Placement::Local
                        && running(receipt)
                        && state.links.terminal_jobs.contains(root)
                        && state.jobs.get(root).is_some_and(running);
                    if !on_terminal {
                        return refused("this job was not started on your terminal".into());
                    }
                    (receipt.session_id.clone(), Some(job.to_owned()))
                }
            };
            if !state.links.terminal_shells.contains(&session) {
                return refused("the session is not attached to a terminal".into());
            }
            match state.links.admit(&session, Instant::now()) {
                Ok(program) => (program, job),
                Err(reason) => return refused(reason),
            }
        };
        let (program, job) = admitted;
        if let Err(error) = spawn(&program, url) {
            return refused(format!("cannot run {}: {error}", program.display()));
        }
        // Journaled with the job's next transition.
        let origin = marsh_contracts::terminal::url_origin(url);
        if let Some(job) = &job
            && let Some(receipt) = self.lock().jobs.get_mut(job)
            && receipt.opened.len() < MAX_RECORDED
            && !receipt.opened.contains(&origin)
        {
            receipt.opened.push(origin);
        }
        crate::PublicReply::UrlOpen {
            opened: true,
            reason: None,
        }
    }
}

fn running(job: &crate::JobReceipt) -> bool {
    matches!(
        job.state,
        crate::JobState::Queued | crate::JobState::Running
    )
}

/// Run `program URL` with no shell and no inherited stdio; reaped in the
/// background.
pub(crate) fn spawn(program: &Path, url: &str) -> std::io::Result<()> {
    let mut child = Command::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(config: &Path) -> Links {
        Links {
            opener: Some(Opener {
                program: PathBuf::from("/usr/bin/true"),
                disabled_by_environment: false,
                config: config.to_owned(),
            }),
            ..Links::default()
        }
    }

    #[test]
    fn a_session_gets_a_few_links_per_window() {
        let root = tempfile::tempdir().unwrap();
        let mut links = links(&root.path().join("config.json"));
        let start = Instant::now();
        for _ in 0..BURST {
            assert!(links.admit("s", start).is_ok());
        }
        assert!(links.admit("s", start).unwrap_err().contains("more than"));
        assert!(links.admit("other", start).is_ok());
        assert!(links.admit("s", start + WINDOW).is_ok());
    }

    #[test]
    fn config_and_environment_turn_opening_off() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        std::fs::write(&config, r#"{"open_urls":false}"#).unwrap();
        let mut links = links(&config);
        assert!(
            links
                .admit("s", Instant::now())
                .unwrap_err()
                .contains("turned off")
        );
        std::fs::write(&config, r#"{"shell":"zsh"}"#).unwrap();
        assert!(links.admit("s", Instant::now()).is_ok());
        let off = Opener::from_environment(
            Some(OsStr::new("/bin/echo")),
            Some(OsStr::new("0")),
            root.path(),
        )
        .unwrap();
        assert!(off.disabled_by_environment);
        assert_eq!(off.program, Path::new("/bin/echo"));
        assert!(Links::default().admit("s", Instant::now()).is_err());
    }
}
