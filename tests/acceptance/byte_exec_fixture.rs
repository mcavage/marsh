//! Credential-free native byte oracle for the final-boundary Docker probe.
//! Build with official rustc 1.95; this is a fixture, never an implementation.
use std::{
    io::{IsTerminal, Read, Write},
    os::unix::ffi::OsStrExt,
};
fn field(output: &mut impl Write, bytes: &[u8]) {
    output
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    output.write_all(bytes).unwrap();
}
fn main() {
    let argv: Vec<_> = std::env::args_os().skip(1).collect();
    if argv.iter().any(|word| word == "tty-probe") {
        let last = argv
            .last()
            .unwrap()
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        println!(
            "TTY:{}:{}:{}:{last}",
            u8::from(std::io::stdin().is_terminal()),
            u8::from(std::io::stdout().is_terminal()),
            u8::from(std::io::stderr().is_terminal())
        );
        std::process::exit(37);
    }
    let waiting = argv.iter().any(|word| word == "wait-signal");
    let mut stdin = Vec::new();
    if !waiting {
        std::io::stdin().read_to_end(&mut stdin).unwrap();
    }
    let mut output = std::io::stdout().lock();
    output.write_all(b"BYTEPROBE1").unwrap();
    output
        .write_all(&(argv.len() as u32).to_be_bytes())
        .unwrap();
    for word in &argv {
        field(&mut output, word.as_bytes());
    }
    for name in [
        "PROBE_VALUE",
        "STATIC_IMAGE_VALUE",
        "HOME",
        "USER",
        "LOGNAME",
    ] {
        field(
            &mut output,
            std::env::var_os(name).unwrap_or_default().as_bytes(),
        );
    }
    field(
        &mut output,
        std::env::current_dir().unwrap().as_os_str().as_bytes(),
    );
    field(&mut output, &stdin);
    output.flush().unwrap();
    std::io::stderr().write_all(b"stderr\0\xff\xfe\n").unwrap();
    if waiting {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    std::fs::write(
        std::ffi::OsStr::from_bytes(b"observed-\xff.bin"),
        b"file\0\xff\xfe\n",
    )
    .unwrap();
    std::process::exit(37);
}
