use crate::model::{Session, Settings};
use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct Storage {
    pub root: PathBuf,
}

impl Storage {
    pub fn new() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "pomidorro", "Pomidorro")
            .context("Не найден домашний каталог")?;
        let root = dirs.data_dir().to_path_buf();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }
    pub fn settings(&self) -> Result<Settings> {
        let path = self.root.join("settings.toml");
        if !path.exists() {
            let settings = Settings::default();
            self.save_settings(&settings)?;
            return Ok(settings);
        }
        let settings: Settings =
            toml::from_str(&fs::read_to_string(&path)?).context("Некорректный settings.toml")?;
        settings.validate()?;
        Ok(settings)
    }
    pub fn sessions(&self) -> Result<Vec<Session>> {
        let path = self.root.join("sessions.json");
        if !path.exists() {
            return Ok(Vec::new());
        }
        serde_json::from_slice(&fs::read(path)?).context("Не удалось прочитать историю")
    }
    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        settings.validate()?;
        atomic_write(
            &self.root.join("settings.toml"),
            toml::to_string_pretty(settings)?.as_bytes(),
        )
    }
    pub fn save_sessions(&self, sessions: &[Session]) -> Result<()> {
        atomic_write(
            &self.root.join("sessions.json"),
            &serde_json::to_vec_pretty(sessions)?,
        )
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    fs::write(&temp, bytes)?;
    fs::rename(temp, path)?;
    Ok(())
}
