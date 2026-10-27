use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::windows::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const MODULE_HASH: &str = "2cf8a2c6cc818243b6bb32ca542d6777ba20cd3dd9137f426bcd3b392e156e9b";
const ADDON: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/sidetone-noise-cancellation.node"
));
const HOST: &[u8] = include_bytes!("../../noise-cancellation/host.cjs");
const PACKAGE: &[u8] =
    br#"{"name":"sidetone-noise-cancellation","version":"0.1.0","main":"host.cjs"}"#;
const REQUIRED: [&str; 4] = [
    "runtime/Discord.exe",
    "module/discord_krisp.node",
    "runtime/locales/en-US.pak",
    "module/krisp-nc-o-med-v7.kef",
];

#[derive(Deserialize, Serialize)]
struct Manifest {
    format: u32,
    files: Vec<PayloadFile>,
}

#[derive(Deserialize, Serialize)]
struct PayloadFile {
    path: String,
    sha256: String,
}

struct Setup<'a> {
    cancel: Option<&'a AtomicBool>,
    started: Instant,
}

impl Setup<'_> {
    fn check(&self) -> Result<()> {
        ensure!(
            !self.cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)),
            "Noise cancellation cache preparation cancelled"
        );
        ensure!(
            self.started.elapsed() < Duration::from_secs(60),
            "Noise cancellation cache preparation timed out"
        );
        Ok(())
    }

    fn hash(&self, path: &Path) -> Result<String> {
        self.check()?;
        let mut file = File::open(path).with_context(|| format!("Read {}", path.display()))?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            self.check()?;
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        Ok(format!("{:x}", hash.finalize()))
    }

    fn validate(&self, root: &Path) -> Result<bool> {
        self.check()?;
        let manifest: Manifest = serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
        if manifest.format != 1
            || !REQUIRED.iter().all(|required| {
                manifest
                    .files
                    .iter()
                    .any(|file| file.path.replace('\\', "/") == *required)
            })
        {
            return Ok(false);
        }
        for file in &manifest.files {
            let path = cache_path(root, &file.path)?;
            if self.hash(&path)? != file.sha256 {
                return Ok(false);
            }
        }
        Ok(self.hash(&cache_path(root, REQUIRED[1])?)? == MODULE_HASH)
    }

    fn find_installation(&self) -> Result<(PathBuf, PathBuf)> {
        self.check()?;
        let mut candidates = if let Some(path) = std::env::var_os("SIDETONE_DISCORD_DIR") {
            vec![PathBuf::from(path)]
        } else {
            let install = dirs::data_local_dir()
                .context("Local AppData unavailable")?
                .join("Discord");
            fs::read_dir(install)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("app-"))
                .map(|entry| entry.path())
                .collect()
        };
        candidates.sort();
        for candidate in candidates.into_iter().rev() {
            self.check()?;
            if ensure_regular(&candidate.join("Discord.exe")).is_err() {
                continue;
            }
            for entry in fs::read_dir(candidate.join("modules"))
                .into_iter()
                .flatten()
            {
                self.check()?;
                let entry = entry?;
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("discord_krisp-")
                {
                    continue;
                }
                let module = entry.path().join("discord_krisp");
                if self
                    .hash(&module.join("discord_krisp.node"))
                    .ok()
                    .as_deref()
                    == Some(MODULE_HASH)
                {
                    return Ok((candidate, module));
                }
            }
        }
        self.check()?;
        bail!(
            "No compatible noise cancellation library found in Discord. The library must match the supported hash, or an existing valid cache must be available."
        )
    }

    fn copy_payload(&self, root: &Path, source: &Path, relative: String) -> Result<PayloadFile> {
        self.check()?;
        ensure_regular(source)?;
        let destination = cache_path(root, &relative)?;
        fs::create_dir_all(destination.parent().unwrap())?;
        let temporary = temporary_path(&destination);
        ensure_not_link(&temporary)?;
        let mut input = File::open(source)?;
        let mut output = File::create(&temporary)?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            self.check()?;
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            output.write_all(&buffer[..count])?;
        }
        drop(output);
        let sha256 = format!("{:x}", hash.finalize());
        ensure!(
            sha256 == self.hash(source)? && sha256 == self.hash(&temporary)?,
            "Source changed while copying {}",
            source.display()
        );
        fs::rename(&temporary, &destination)?;
        Ok(PayloadFile {
            path: relative,
            sha256,
        })
    }

    fn rebuild(&self, root: &Path) -> Result<()> {
        let (runtime, module) = self.find_installation()?;
        let mut files = Vec::new();
        for (source, directory) in [(&runtime, "runtime"), (&module, "module")] {
            for entry in fs::read_dir(source)? {
                self.check()?;
                let entry = entry?;
                let path = entry.path();
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_str().context("Non-UTF-8 payload filename")?;
                let extension = path
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or("");
                let include = if directory == "runtime" {
                    name == "Discord.exe" || matches!(extension, "dll" | "pak" | "dat" | "bin")
                } else {
                    name == "discord_krisp.node" || extension == "kef"
                };
                if include {
                    files.push(self.copy_payload(root, &path, format!("{directory}/{name}"))?);
                }
            }
        }
        files.push(self.copy_payload(
            root,
            &runtime.join("locales/en-US.pak"),
            "runtime/locales/en-US.pak".into(),
        )?);
        ensure!(
            self.hash(&cache_path(root, REQUIRED[1])?)? == MODULE_HASH,
            "Noise cancellation library changed during cache preparation"
        );
        ensure!(
            REQUIRED
                .iter()
                .all(|required| files.iter().any(|file| file.path == *required)),
            "Discord payload is incomplete"
        );
        files.sort_by(|left, right| left.path.cmp(&right.path));
        self.check()?;
        // Publish only after every payload file was copied and verified.
        write_embedded(
            root,
            "manifest.json",
            &serde_json::to_vec_pretty(&Manifest { format: 1, files })?,
        )?;
        ensure!(self.validate(root)?, "Prepared cache failed validation");
        Ok(())
    }
}

fn ensure_not_link(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "Linked cache paths are unsupported: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn ensure_regular(path: &Path) -> Result<()> {
    ensure_not_link(path)?;
    ensure!(
        fs::metadata(path)?.is_file(),
        "Expected a file: {}",
        path.display()
    );
    Ok(())
}

fn cache_path(root: &Path, relative: &str) -> Result<PathBuf> {
    ensure!(
        !relative.is_empty() && !relative.contains(':'),
        "Invalid cache manifest path"
    );
    ensure_not_link(root)?;
    let mut path = root.to_owned();
    for component in Path::new(relative).components() {
        let Component::Normal(component) = component else {
            bail!("Invalid cache manifest path");
        };
        path.push(component);
        ensure_not_link(&path)?;
    }
    Ok(path)
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

fn write_embedded(root: &Path, relative: &str, bytes: &[u8]) -> Result<()> {
    let path = cache_path(root, relative)?;
    if fs::read(&path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    fs::create_dir_all(path.parent().unwrap())?;
    let temporary = temporary_path(&path);
    ensure_not_link(&temporary)?;
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, &path).with_context(|| format!("Update {}", path.display()))
}

pub(super) fn prepare(cancel: Option<&AtomicBool>) -> Result<PathBuf> {
    static PREPARED: OnceLock<PathBuf> = OnceLock::new();
    let setup = Setup {
        cancel,
        started: Instant::now(),
    };
    setup.check()?;
    if let Some(root) = PREPARED.get()
        && REQUIRED.iter().all(|path| root.join(path).is_file())
        && root.join("sidetone-noise-cancellation.node").is_file()
    {
        return Ok(root.clone());
    }
    let exe = std::env::current_exe()?;
    let root = exe
        .parent()
        .context("Executable directory missing")?
        .join("sidetone-cache");
    ensure_not_link(&root)?;
    fs::create_dir_all(&root).context("Create Sidetone cache next to executable")?;
    if setup.validate(&root).unwrap_or(false) {
        crate::log("Noise cancellation cache validated and reused");
    } else {
        setup.rebuild(&root)?;
        crate::log("Noise cancellation cache prepared from installed Discord");
    }
    setup.check()?;
    write_embedded(&root, "runtime/resources/app/host.cjs", HOST)?;
    write_embedded(&root, "runtime/resources/app/package.json", PACKAGE)?;
    write_embedded(&root, "sidetone-noise-cancellation.node", ADDON)?;
    // Remove obsolete setup files from caches created by earlier builds.
    for old in ["cache.ps1", "host.cjs"] {
        let path = cache_path(&root, old)?;
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    setup.check()?;
    let _ = PREPARED.set(root.clone());
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_paths_reject_escapes_and_stream_paths() {
        let root = std::env::temp_dir().join("sidetone-path-validation");
        for path in [
            "",
            "../outside",
            "runtime/../../outside",
            "C:\\outside",
            "\\\\server\\share",
            "/absolute",
            "runtime/file:stream",
        ] {
            assert!(cache_path(&root, path).is_err(), "{path}");
        }
        assert_eq!(
            cache_path(&root, "runtime/Discord.exe").unwrap(),
            root.join("runtime/Discord.exe")
        );
    }

    #[test]
    fn preparation_honors_cancellation_and_deadline() {
        let cancel = AtomicBool::new(true);
        let setup = Setup {
            cancel: Some(&cancel),
            started: Instant::now(),
        };
        assert!(setup.check().is_err());
        cancel.store(false, Ordering::Relaxed);
        assert!(setup.check().is_ok());
        let expired = Setup {
            cancel: None,
            started: Instant::now() - Duration::from_secs(61),
        };
        assert!(expired.check().is_err());
    }

    #[test]
    fn embedded_files_are_repaired_and_payload_hashes_detect_changes() {
        let root = std::env::temp_dir().join(format!(
            "sidetone-cache-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let setup = Setup {
            cancel: None,
            started: Instant::now(),
        };
        write_embedded(&root, "addon.node", b"first").unwrap();
        let path = root.join("addon.node");
        let initial = setup.hash(&path).unwrap();
        write_embedded(&root, "addon.node", b"second").unwrap();
        assert_ne!(setup.hash(&path).unwrap(), initial);
        write_embedded(&root, "addon.node", b"first").unwrap();
        assert_eq!(setup.hash(&path).unwrap(), initial);
        assert!(!temporary_path(&path).exists());
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
