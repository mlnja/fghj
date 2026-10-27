//! fghj on Debian and Ubuntu ships as a `.deb` from apt.mlops.ninja, and runs
//! as the `fghjd` systemd unit.

use crate::platform::Packaging;

pub(super) struct Apt;

impl Packaging for Apt {
    fn start_service_hint(&self) -> &'static str {
        "`sudo systemctl start fghjd`"
    }

    fn uninstall_hint(&self) -> &'static str {
        "`sudo apt remove fghj`"
    }

    /// Ubuntu doesn't package it.
    fn install_cue_hint(&self) -> &'static str {
        "see https://cuelang.org/docs/install/"
    }
}
