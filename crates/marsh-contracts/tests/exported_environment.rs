//! Exported worker environment: raw values, unchanged authority/capacity policy.
use marsh_contracts::{ExportedEnvironment, JobSpecError, validate_exported_environment};
use std::collections::BTreeMap;

#[test]
fn raw_values_round_trip_as_one_byte_valued_wire_representation() {
    let environment: ExportedEnvironment = BTreeMap::from([
        ("ALL_NON_NUL".into(), (1..=255).collect()),
        ("EMPTY".into(), Vec::new()),
        ("CARGO_BIN_EXE_tool-name.ext".into(), vec![0xff, 0xfe]),
    ]);
    validate_exported_environment(&environment).unwrap();
    let wire = serde_json::to_vec(&environment).unwrap();
    let decoded: ExportedEnvironment = serde_json::from_slice(&wire).unwrap();
    assert_eq!(decoded, environment);
    let json: serde_json::Value = serde_json::from_slice(&wire).unwrap();
    assert_eq!(
        json["CARGO_BIN_EXE_tool-name.ext"],
        serde_json::json!([255, 254])
    );
    // Old text values fail closed, rather than silently selecting a fallback.
    assert!(serde_json::from_slice::<ExportedEnvironment>(br#"{"VALUE":"text"}"#).is_err());
}

#[test]
fn exported_environment_rejects_only_existing_name_and_nul_boundaries() {
    for name in [
        "",
        "1NUMBER",
        "N=V",
        "N\0V",
        "RAW_é",
        "HOME",
        "USER",
        "LOGNAME",
        "MARSH_PRIVATE",
        "SBX_PRIVATE",
        "DOCKER_HOST",
        "CONTAINERD_ADDRESS",
        "HTTP_PROXY",
        "https_proxy",
        "NODE_EXTRA_CA_CERTS",
        "MCP_GATEWAY_URL",
        "MCP_SENTINEL_TOKEN_NAME",
        "NODE_USE_ENV_PROXY",
    ] {
        assert_eq!(
            validate_exported_environment(&BTreeMap::from([(name.into(), vec![0xff])])),
            Err(JobSpecError::InvalidExportedEnvironment),
            "name {name:?}",
        );
    }
    assert_eq!(
        validate_exported_environment(&BTreeMap::from([("VALUE".into(), vec![1, 0, 255])])),
        Err(JobSpecError::InvalidExportedEnvironment),
    );
    validate_exported_environment(&BTreeMap::from([("_a-b.c9".into(), vec![255])])).unwrap();
}

#[test]
fn byte_limits_keep_exact_preexisting_capacities() {
    let mut environment: ExportedEnvironment =
        BTreeMap::from([("N".repeat(128), vec![255; 16 * 1024])]);
    validate_exported_environment(&environment).unwrap();
    environment.values_mut().next().unwrap().push(255);
    assert!(validate_exported_environment(&environment).is_err());
    assert!(validate_exported_environment(&BTreeMap::from([("N".repeat(129), vec![])])).is_err());

    let mut environment: ExportedEnvironment = (0..256)
        .map(|index| (format!("N_{index:03}"), Vec::new()))
        .collect();
    validate_exported_environment(&environment).unwrap();
    environment.insert("EXTRA".into(), Vec::new());
    assert!(validate_exported_environment(&environment).is_err());

    let mut environment: ExportedEnvironment = BTreeMap::from([
        ("A".into(), vec![255; 16 * 1024]),
        ("B".into(), vec![255; 16 * 1024]),
        ("C".into(), vec![255; 16 * 1024]),
        ("D".into(), vec![255; 16 * 1024 - 4]),
    ]);
    validate_exported_environment(&environment).unwrap();
    // Byte arrays remain below the unchanged 1 MiB control-frame bound even
    // when every admitted byte needs the longest decimal JSON byte spelling.
    assert!(serde_json::to_vec(&environment).unwrap().len() < 1024 * 1024);
    environment.get_mut("D").unwrap().push(255);
    assert!(validate_exported_environment(&environment).is_err());
}
