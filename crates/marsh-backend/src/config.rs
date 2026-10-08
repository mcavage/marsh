//! Host-side backend configuration.

#[derive(Clone, Debug, Default)]
pub struct EnvironmentConfig {
    pub drop: Vec<String>,
    pub set: marsh_contracts::ExportedEnvironment,
}
