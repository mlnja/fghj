//! fghj on macOS ships through the `mlnja/tap` Homebrew tap.

use crate::platform::Packaging;

pub(super) struct Homebrew;

impl Packaging for Homebrew {
    fn start_service_hint(&self) -> &'static str {
        "`sudo brew services start fghj`"
    }

    fn uninstall_hint(&self) -> &'static str {
        "`brew uninstall fghj`"
    }

    fn install_cue_hint(&self) -> &'static str {
        "`brew install cue`"
    }
}
