use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum OperationType {
    ConfigValidate,
    PackageCheck,
    PackageCreate,
    PackageInfo,
    PackageInstall,
    PackageList,
    PackageValidate,
}
