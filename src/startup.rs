use anyhow::{Context, Result};
use std::path::Path;
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_WRITE};

const NAME: &str = "Sidetone";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

fn launcher(exe: &Path) -> Result<auto_launch::AutoLaunch> {
    // Quote the path because auto-launch joins it and args with spaces.
    auto_launch::AutoLaunchBuilder::new()
        .set_app_name(NAME)
        .set_app_path(&format!("\"{}\"", exe.display()))
        .set_args(&[] as &[String])
        .build()
        .context("building the startup entry")
}

pub fn install(exe: &Path) -> Result<String> {
    launcher(exe)?.enable().context("writing startup value")?;
    Ok(format!(r"HKCU\{RUN_KEY}\{NAME}"))
}

pub fn uninstall() -> Result<String> {
    let exe = std::env::current_exe().context("finding the Sidetone executable")?;
    match launcher(&exe)?.disable() {
        Ok(()) => {}
        Err(auto_launch::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::Error::new(e).context("deleting startup value")),
    }
    // Remove the otherwise lingering Task Manager startup row.
    if let Ok(approved) =
        RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(APPROVED_KEY, KEY_WRITE)
    {
        let _ = approved.delete_value(NAME);
    }
    Ok(format!(r"HKCU\{RUN_KEY}\{NAME}"))
}
