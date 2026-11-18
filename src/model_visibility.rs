use serde::{Deserialize, Serialize};
use std::{fs, io, path::PathBuf, sync::Mutex};

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub(crate) struct VisibilitySetting {
    pub free_models_only: bool,
}

#[derive(Debug)]
pub(crate) struct ModelVisibility {
    path: Option<PathBuf>,
    setting: Mutex<VisibilitySetting>,
}

impl ModelVisibility {
    pub fn memory(free_models_only: bool) -> Self {
        Self {
            path: None,
            setting: Mutex::new(VisibilitySetting { free_models_only }),
        }
    }

    pub fn load(path: PathBuf, free_models_only: bool) -> io::Result<Self> {
        let setting = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                VisibilitySetting { free_models_only }
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            path: Some(path),
            setting: Mutex::new(setting),
        })
    }

    pub fn get(&self) -> bool {
        self.setting.lock().unwrap().free_models_only
    }

    pub fn set(&self, setting: VisibilitySetting) -> io::Result<()> {
        let mut active = self.setting.lock().unwrap();
        if let Some(path) = &self.path {
            crate::aliases::persist_json(path, &setting)?;
        }
        *active = setting;
        Ok(())
    }
}
