# Linux ARM64 guest artifacts

`make build` uses this Dockerfile to cross-build the three trusted executables
that run inside stock Docker Sandbox VMs. They are exported under
`target/libexec/marsh/`; the macOS daemon never copies its own Mach-O binary
into Linux.

Release builders set `RUST_IMAGE` to a reviewed immutable builder reference.
The default `rust:1.95-bookworm` keeps a source checkout straightforward for
local development.
