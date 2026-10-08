//! The static job artifact (`docs/design/processes.md` s4): bound read-only at
//! `/run/marsh/marsh` in every Kit job. As a `#!/run/marsh/marsh --link=NAME`
//! stub it is a link or the in-container `marsh` (a job's shells are the
//! image's own); invoked directly it is Brush (compatibility tests).

fn main() {
    std::process::exit(marsh::job::main());
}
