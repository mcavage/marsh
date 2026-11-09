// Records whether this build enables `test-breaks`, for the executables below
// (marsh-provenance; docs/design/test-breaks.md, "Releases").
fn main() {
    marsh_provenance::build::emit();
}
