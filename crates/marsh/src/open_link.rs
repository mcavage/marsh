//! `xdg-open URL`, `open URL`, and `marsh open URL` in the shell VM and in
//! jobs: ask the daemon to open a web link on the Mac, and always print it
//! (`docs/agents.md`, `docs/security.md`).

use marsh_contracts::terminal;
use std::ffi::OsString;
use std::io::IsTerminal as _;

pub const HELP: &str = "Usage: xdg-open URL | open URL | marsh open URL

Open an http:// or https:// link in your Mac's browser. The link opens only
while this session's terminal is attached and the program was started from
it; otherwise it is printed for you to click. `marsh config open-urls off`
on the Mac turns opening off.
";

/// Where the request comes from, for the refusal text.
#[derive(Clone, Copy, Debug)]
pub enum Place {
    Shell,
    Job,
}

/// Run the command; `client` reaches the daemon only for an accepted link.
pub fn run(
    arguments: &[OsString],
    place: Place,
    client: impl FnOnce() -> Result<marsh_daemon::Client, String>,
) -> i32 {
    let url = match arguments {
        [flag] if matches!(flag.to_str(), Some("-h" | "--help")) => {
            print!("{HELP}");
            return 0;
        }
        [url] => url.to_string_lossy().into_owned(),
        _ => {
            eprint!("{HELP}");
            return 1;
        }
    };
    if terminal::url_refusal(&url).is_some() {
        let from = match place {
            Place::Shell => "the shell VM",
            Place::Job => "a job",
        };
        eprintln!(
            "marsh: can't open {} from {from}; open it on your Mac",
            url.escape_debug()
        );
        return 1;
    }
    let link = if std::io::stderr().is_terminal() {
        terminal::hyperlink(&url)
    } else {
        url.clone()
    };
    let refusal = match client()
        .and_then(|client| client.open_url(&url).map_err(|error| error.to_string()))
    {
        Ok(None) => {
            eprintln!("marsh: opened {link} on your Mac");
            return 0;
        }
        Ok(Some(reason)) | Err(reason) => reason,
    };
    eprintln!("marsh: not opened on your Mac ({refusal}); open it yourself: {link}");
    // xdg-open's "the action failed": the link was printed, not opened.
    4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_links_never_reach_the_daemon() {
        for url in ["file:///etc/passwd", "/tmp/x.html", "vscode://x"] {
            let status = run(&[url.into()], Place::Job, || {
                panic!("the daemon is not asked about {url}")
            });
            assert_eq!(status, 1);
        }
        assert_eq!(
            run(&["-a".into(), "Safari".into()], Place::Shell, || panic!()),
            1
        );
    }

    #[test]
    fn an_unreachable_daemon_prints_the_link_and_fails() {
        let status = run(&["https://example.com".into()], Place::Job, || {
            Err("no capability socket".into())
        });
        assert_eq!(status, 4);
    }
}
