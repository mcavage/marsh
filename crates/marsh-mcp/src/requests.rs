//! The MCP tool response envelope and the typed, unknown-field-rejecting
//! request bodies of the host tools.

use super::{Deserialize, JsonSchema, Serialize, Value};

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ToolResponse {
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

impl ToolResponse {
    pub(crate) fn success(data: Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub(crate) fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error.into()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultGetRequest {
    /// Durable result cursor, full job UUID, or unique UUID prefix.
    pub(crate) selector: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SelectionRequest {
    /// `all` or a comma-separated list of lowercase Kit names.
    #[serde(default = "default_selection")]
    pub(crate) selection: String,
}

pub(crate) fn default_selection() -> String {
    "all".into()
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellRunRequest {
    /// Shell program evaluated by marsh inside the project shell VM.
    pub(crate) command: String,
    /// Wall-clock limit in milliseconds (10..=900000).
    #[serde(default)]
    pub(crate) timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QualifyRequest {
    /// Fixed qualification gate: `source`, `smoke`, `full`, or `perf`.
    pub(crate) gate: String,
    /// Wall-clock limit in milliseconds (10..=900000).
    #[serde(default)]
    pub(crate) timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationRequest {
    /// Operation identifier returned by a mutating or long-running tool.
    pub(crate) operation_id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeRequest {
    /// Opaque scope identifier returned by `scope_start`, or `default`.
    pub(crate) scope_id: String,
}

#[derive(Clone, Debug, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct EmptyRequest {}

impl<'de> Deserialize<'de> for EmptyRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct EmptyVisitor;

        impl<'de> serde::de::Visitor<'de> for EmptyVisitor {
            type Value = EmptyRequest;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an empty object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                if map.next_key::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::custom("this tool accepts no properties"));
                }
                Ok(EmptyRequest {})
            }
        }

        deserializer.deserialize_map(EmptyVisitor)
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeRunRequest {
    /// Opaque scope identifier returned by `scope_start`, or `default`.
    pub(crate) scope_id: String,
    /// Shell program evaluated by marsh inside this scope's project shell VM.
    pub(crate) command: String,
    /// Wall-clock limit in milliseconds (10..=900000).
    #[serde(default)]
    pub(crate) timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeResultGetRequest {
    /// Opaque scope identifier returned by `scope_start`, or `default`.
    pub(crate) scope_id: String,
    /// Durable result cursor, full job UUID, or unique UUID prefix.
    pub(crate) selector: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeSelectionRequest {
    /// Opaque scope identifier returned by `scope_start`, or `default`.
    pub(crate) scope_id: String,
    /// `all` or a comma-separated list of lowercase Kit names.
    #[serde(default = "default_selection")]
    pub(crate) selection: String,
}
