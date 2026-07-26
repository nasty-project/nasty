use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Stable identity of a block subvolume, independent of its pool name,
/// subvolume name, mount point, and current loop-device number.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct BlockVolumeId {
    pub filesystem_uuid: String,
    pub subvolume_id: u32,
}

/// Runtime result of restoring block-subvolume loop devices.
#[derive(Debug, Clone, Default)]
pub struct BlockDeviceMappings {
    /// Stable identity to the loop device attached during this boot.
    pub current: HashMap<BlockVolumeId, String>,
}

impl BlockDeviceMappings {
    pub fn is_empty(&self) -> bool {
        self.current.is_empty()
    }
}
