use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolumeDesired {
    pub name: String,
}

/// `false` covers both "never checked" and "gone".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct VolumeObserved {
    pub exists: bool,
}

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
