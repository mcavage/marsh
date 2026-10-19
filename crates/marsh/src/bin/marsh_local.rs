//! The static job artifact (`docs/design/processes.md` s4): bound read-only at
//! `/run/marsh/marsh` in every Kit job. As a `#!/run/marsh/marsh --link=NAME`
//! stub it is a link or the in-container `marsh` (a job's shells are the
//! image's own); invoked directly it is Brush (compatibility tests).

fn main() {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    // Inside a daemon-owned cloud sandbox only: the host drives these steps.
    if arguments
        .get(1)
        .is_some_and(|word| word == "--internal-cloud")
    {
        std::process::exit(marsh::cloud_helper::main(&arguments[2..]));
    }
    std::process::exit(marsh::job::main());
}
