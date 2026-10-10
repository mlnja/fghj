use serde::Serialize;

/// Everything about a Docker volume fghj *wants* to exist — currently just
/// its derived name (see `runs::naming::derive_volume_name`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolumeDesired {
    pub name: String,
}

/// Whether the Docker-polling effect has actually observed this volume to
/// exist. Unlike `ContainerObserved::sync`, there's no "unknown" state
/// worth spelling out separately here — a volume either currently exists
/// in Docker or it doesn't; `false` covers both "never checked yet" and
/// "checked, and it's gone."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct VolumeObserved {
    pub exists: bool,
}

/// One volume's full reducer-owned state, mirroring `ContainerInfo`'s
/// desired/observed split.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolumeInfo {
    pub desired: VolumeDesired,
    pub observed: VolumeObserved,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_volume_is_not_yet_observed_to_exist() {
        let info = VolumeInfo {
            desired: VolumeDesired {
                name: "fghj-vol-web".into(),
            },
            observed: VolumeObserved::default(),
        };
        assert!(!info.observed.exists);
    }
}
