//! The stock CLI surface marsh depends on, driven by captured output of every
//! supported version and historical inventory formats (`tests/data/sbx`).
use super::corpus::{self, StockVersion, VERSIONS};
use super::*;

#[test]
fn every_supported_stock_version_passes_validation() {
    for version in [&corpus::V047, &corpus::V047_DEV] {
        StockModel::of(version)
            .adapter()
            .validate_supported_version()
            .unwrap_or_else(|error| panic!("{}: {error}", version.name));
    }
}

static TOO_OLD: StockVersion = StockVersion {
    name: "v0.46.9",
    version_output: b"sbx version: v0.46.9 0000000000000000000000000000000000000000\n",
    ..corpus::V047
};

static NO_LOCAL_KITS: StockVersion = StockVersion {
    name: "no-kit-reference",
    create_help: "Create a sandbox for an agent\n\nUsage:\n  sbx create AGENT [flags]\n\nFlags:\n  -h, --help   help for create\n",
    ..corpus::V047
};

#[test]
fn validation_rejects_an_old_release_and_a_create_without_local_kits() {
    assert!(matches!(
        StockModel::of(&TOO_OLD)
            .adapter()
            .validate_supported_version(),
        Err(SbxError::UnsupportedVersion { .. })
    ));
    assert!(matches!(
        StockModel::of(&NO_LOCAL_KITS)
            .adapter()
            .validate_supported_version(),
        Err(SbxError::MissingCapability("local Kit v3 create"))
    ));
}

#[test]
fn version_records_accept_stable_and_coherent_nightlies_from_047_only() {
    let commit = "1111111111111111111111111111111111111111";
    for (record, accepted) in [
        (format!("sbx version: v0.47.0 {commit}"), true),
        (format!("sbx version: v0.47.1-1-g1111111 {commit}"), true),
        (format!("sbx version: v1.0.0 {commit}"), true),
        (
            "sbx version: v0.47.0-239-g3fd12a12a 3fd12a12a0000000000000000000000000000000".into(),
            true,
        ),
        (format!("sbx version v0.47.0 {commit}"), true),
        (format!("sbx version: v0.46.0 {commit}"), false),
        (format!("sbx version: v0.44.9-999-g1111111 {commit}"), false),
        (format!("sbx version v0.44.9 {commit}"), false),
        // Abbreviated commit disagrees with the full commit.
        (format!("sbx version: v0.47.1-1-g2222222 {commit}"), false),
        // Zero distance is not a nightly.
        (format!("sbx version: v0.47.1-0-g1111111 {commit}"), false),
        (
            "sbx version: v0.47.1 111111111111111111111111111111111111111".into(),
            false,
        ),
        (format!("sbx versionv0.47.0 {commit}"), false),
        ("not an sbx version".into(), false),
        (
            "warning mentions v99.0.0 but no version record".into(),
            false,
        ),
    ] {
        assert_eq!(supported_sbx_version(&record), accepted, "{record}");
    }
}

#[test]
fn every_captured_inventory_document_decodes() {
    for version in VERSIONS {
        let inventory = StockInventory::decode(version.ls_json)
            .unwrap_or_else(|error| panic!("{}: {error}", version.name));
        // Every captured row names a VM marsh must be able to look up.
        let listed: serde_json::Value = serde_json::from_slice(version.ls_json).unwrap();
        let rows = listed
            .get("sandboxes")
            .unwrap_or(&listed)
            .as_array()
            .unwrap()
            .clone();
        assert!(!rows.is_empty(), "{}", version.name);
        for row in rows {
            let name = row["name"].as_str().unwrap();
            let id = row["id"].as_str().unwrap();
            assert!(inventory.get(name).is_some(), "{}: {name}", version.name);
            assert!(inventory.contains_uuid(id), "{}: {id}", version.name);
        }
    }
}

#[test]
fn shell_template_download_hint_reads_every_template_listing() {
    for version in VERSIONS {
        let listed: serde_json::Value = serde_json::from_slice(version.template_ls_json).unwrap();
        let row = listed.get("images").unwrap_or(&listed).as_array().unwrap()[0].clone();
        let adapter = StockModel::of(version).adapter();
        let shell = ShellFixture::new();
        let mut spec = shell.spec.clone();
        spec.image = OciImage::parse(format!(
            "{}:{}@sha256:{}",
            row["repository"].as_str().unwrap(),
            row["tag"].as_str().unwrap(),
            "b".repeat(64)
        ))
        .unwrap();
        assert!(
            !adapter.shell_image_download_needed(&spec),
            "{}",
            version.name
        );
        spec.image = OciImage::parse(format!(
            "{}:never-pulled@sha256:{}",
            row["repository"].as_str().unwrap(),
            "b".repeat(64)
        ))
        .unwrap();
        assert!(
            adapter.shell_image_download_needed(&spec),
            "{}",
            version.name
        );
    }
}

/// Every stock call of a cold Kit, local Kit and shell is accepted by each
/// version's captured flag set (the model rejects unknown flags).
#[test]
fn cold_preparation_preserves_captured_stock_cli_format_coverage() {
    for version in VERSIONS {
        let model = StockModel::of(version);
        let adapter = model.adapter();
        let kit = KitFixture::published("marsh-kit-published");
        let local = KitFixture::local("marsh-kit-local");
        let shell = ShellFixture::new();
        assert!(adapter.ensure_kit_vm(&kit.spec).unwrap().cold_started);
        assert!(adapter.ensure_kit_vm(&local.spec).unwrap().cold_started);
        assert!(adapter.ensure_shell_vm(&shell.spec).unwrap().cold_started);
        assert!(adapter.reset_kit_vm(&kit.spec).unwrap(), "{}", version.name);
        assert!(
            adapter.reset_shell_vm(&shell.spec).unwrap(),
            "{}",
            version.name
        );
        assert_eq!(model.vm_names(), ["marsh-kit-local"], "{}", version.name);
    }
}
