/// How fghj was installed, for the commands hints tell people to run.
pub trait Packaging: Send + Sync {
    /// Starts fghjd as a service, e.g. `` `sudo brew services start fghj` ``.
    fn start_service_hint(&self) -> &'static str;

    /// Removes the binaries, e.g. `` `brew uninstall fghj` ``.
    fn uninstall_hint(&self) -> &'static str;

    /// Installs the `cue` CLI, e.g. `` `brew install cue` ``.
    fn install_cue_hint(&self) -> &'static str;
}
