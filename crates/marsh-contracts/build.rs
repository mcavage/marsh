// Release packaging sets MARSH_RELEASE_PACKAGING=1 (CI's host and guest
// builds). That turns on a cfg which refuses the test-breaks feature at compile
// time (src/test_breaks.rs). Nothing else sets it.
//
// The crate's `links` key lets this script tell the build scripts of the crates
// that depend on it directly whether `test-breaks` is on
// (DEP_MARSH_CONTRACTS_TEST_BREAKS=1): the input to `marsh-provenance`.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(marsh_release_packaging)");
    println!("cargo::rerun-if-env-changed=MARSH_RELEASE_PACKAGING");
    if std::env::var_os("MARSH_RELEASE_PACKAGING").is_some_and(|value| value == "1") {
        println!("cargo::rustc-cfg=marsh_release_packaging");
    }
    if std::env::var_os("CARGO_FEATURE_TEST_BREAKS").is_some() {
        println!("cargo::metadata=test_breaks=1");
    }
}
