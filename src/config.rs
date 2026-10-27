use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub delay_ms: u32,
    pub volume_percent: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            delay_ms: 0,
            volume_percent: 100,
        }
    }
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .context("finding the user's config directory")?
            .join("Sidetone")
            .join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let config: Self = toml::from_str(&text).context("parsing Sidetone config.toml")?;
                Ok(config)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let config = Self::default();
                config.save()?;
                Ok(config)
            }
            Err(error) => Err(error).context("reading Sidetone config.toml"),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let temporary = path.with_extension("toml.tmp");
        std::fs::write(&temporary, toml::to_string_pretty(self)?)?;
        std::fs::rename(&temporary, path).context("saving Sidetone config.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_replace_existing_config_without_leaving_a_temporary_file() {
        let directory = std::env::temp_dir().join(format!(
            "sidetone-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("config.toml");
        Config::default().save_to(&path).unwrap();
        let changed = Config {
            delay_ms: 375,
            volume_percent: 137,
        };
        changed.save_to(&path).unwrap();
        assert_eq!(
            toml::from_str::<Config>(&std::fs::read_to_string(&path).unwrap()).unwrap(),
            changed
        );
        assert!(!path.with_extension("toml.tmp").exists());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn config_accepts_custom_nonnegative_integers() {
        let defaults: Config = toml::from_str("").unwrap();
        assert_eq!(defaults.delay_ms, 0);
        assert_eq!(defaults.volume_percent, 100);
        let partial: Config = toml::from_str("delay_ms = 75").unwrap();
        assert_eq!(partial.delay_ms, 75);
        assert_eq!(partial.volume_percent, 100);
        for delay_ms in [0, 1, 137, 200, 375, 1_000] {
            for volume_percent in [0, 1, 137, 200, 250] {
                let config = Config {
                    delay_ms,
                    volume_percent,
                };
                assert_eq!(
                    toml::from_str::<Config>(&toml::to_string(&config).unwrap()).unwrap(),
                    config
                );
            }
        }
        assert!(toml::from_str::<Config>("volume_percent = -25").is_err());
        assert!(toml::from_str::<Config>("delay_ms = -1").is_err());
    }
}
