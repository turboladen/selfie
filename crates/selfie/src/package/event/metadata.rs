use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum OperationType {
    DotfileApply,
    DotfileDrift,
    DotfileTrack,
    ConfigValidate,
    PackageAudit,
    PackageCheck,
    PackageCreate,
    PackageInstall,
    PackageList,
    PackageRemove,
    PackageStatus,
    PackageUpdate,
    PackageValidate,
    SpecInfo,
    SpecList,
    SpecSearch,
    SpecValidateAll,
    SyncStatus,
    SyncPush,
    SyncPull,
}
