#![cfg(unix)]

use serde::{Deserialize, Serialize};
use std::{
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    path::PathBuf,
};

#[derive(Deserialize, Serialize)]
struct WorkingDirectory {
    #[serde(with = "marsh_contracts::byte_path")]
    path: PathBuf,
}

#[derive(Deserialize, Serialize)]
struct OptionalWorkingDirectory {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "marsh_contracts::byte_path::optional"
    )]
    path: Option<PathBuf>,
}

#[test]
fn optional_cwd_accepts_only_absence_or_the_same_byte_array() {
    let missing: OptionalWorkingDirectory = serde_json::from_str("{}").unwrap();
    assert!(missing.path.is_none());
    assert_eq!(serde_json::to_string(&missing).unwrap(), "{}");
    for invalid in [r#"{"path":null}"#, r#"{"path":"/text"}"#, r#"{"path":[]}"#] {
        assert!(serde_json::from_str::<OptionalWorkingDirectory>(invalid).is_err());
    }
    let raw = b"/project/raw-\xff\xfe";
    let present: OptionalWorkingDirectory =
        serde_json::from_value(serde_json::json!({"path": raw})).unwrap();
    assert_eq!(present.path.as_ref().unwrap().as_os_str().as_bytes(), raw);
    assert_eq!(
        serde_json::to_value(present).unwrap(),
        serde_json::json!({"path": raw})
    );
}

#[test]
fn raw_absolute_path_roundtrips_without_a_string_fallback() {
    let original = b"/approved/project/raw-\xff\xfe-\xef\xbf\xbd";
    let value = WorkingDirectory {
        path: std::ffi::OsString::from_vec(original.to_vec()).into(),
    };
    let json = serde_json::to_vec(&value).unwrap();
    let restored: WorkingDirectory = serde_json::from_slice(&json).unwrap();
    assert_eq!(restored.path.as_os_str().as_bytes(), original);
    assert!(serde_json::from_slice::<WorkingDirectory>(br#"{"path":"/string/path"}"#).is_err());
}

#[test]
fn cwd_codec_bounds_do_not_create_a_path_authority_bypass() {
    for bytes in [
        b"".as_slice(),
        b"relative",
        b"/approved/../outside",
        b"/nul\0path",
    ] {
        let value = WorkingDirectory {
            path: std::ffi::OsString::from_vec(bytes.to_vec()).into(),
        };
        assert!(serde_json::to_vec(&value).is_err());
        assert!(
            serde_json::from_value::<WorkingDirectory>(serde_json::json!({"path": bytes})).is_err()
        );
    }
    let mut bytes = vec![b'a'; 4096];
    bytes[0] = b'/';
    let value: WorkingDirectory =
        serde_json::from_value(serde_json::json!({"path": bytes})).unwrap();
    assert_eq!(value.path.as_os_str().as_bytes().len(), 4096);
    bytes.push(b'a');
    assert!(
        serde_json::from_value::<WorkingDirectory>(serde_json::json!({"path": bytes})).is_err()
    );
    assert!(
        serde_json::from_value::<WorkingDirectory>(serde_json::json!({"path": [47, 256]})).is_err()
    );
}
