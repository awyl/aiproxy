use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::PathBuf,
    sync::{Mutex, RwLock},
};

#[derive(Deserialize)]
pub(crate) struct VisibilitySetting {
    pub provider: String,
    pub free_models_only: bool,
}

#[derive(Debug)]
pub(crate) struct VisibilityDefault {
    pub state_key: String,
    pub free_only: bool,
    pub inherits_legacy: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Overrides {
    upstreams: BTreeMap<String, bool>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SavedSettings {
    PerUpstream(Overrides),
    Legacy { free_models_only: bool },
}

#[derive(Debug, Default)]
pub(crate) struct ModelVisibility {
    path: Option<PathBuf>,
    defaults: BTreeMap<String, VisibilityDefault>,
    overrides: RwLock<Overrides>,
    save_lock: Mutex<()>,
}

impl ModelVisibility {
    pub fn memory(defaults: BTreeMap<String, VisibilityDefault>) -> Self {
        Self {
            defaults,
            ..Self::default()
        }
    }

    pub fn load(path: PathBuf, defaults: BTreeMap<String, VisibilityDefault>) -> io::Result<Self> {
        let overrides = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes).map_err(io::Error::other)? {
                SavedSettings::PerUpstream(overrides) => overrides,
                SavedSettings::Legacy { free_models_only } => Overrides {
                    upstreams: defaults
                        .values()
                        .filter(|d| d.inherits_legacy)
                        .map(|d| (d.state_key.clone(), free_models_only))
                        .collect(),
                },
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Overrides::default(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path: Some(path),
            defaults,
            overrides: RwLock::new(overrides),
            save_lock: Mutex::new(()),
        })
    }

    pub fn supports(&self, provider: &str) -> bool {
        self.defaults.contains_key(provider)
    }

    pub fn free_only(&self, provider: &str) -> bool {
        self.defaults
            .get(provider)
            .is_some_and(|default| default.effective(&self.overrides.read().unwrap()))
    }

    pub fn snapshot(&self) -> BTreeMap<&str, bool> {
        let overrides = self.overrides.read().unwrap();
        self.defaults
            .iter()
            .map(|(provider, default)| (provider.as_str(), default.effective(&overrides)))
            .collect()
    }

    pub fn set(&self, setting: VisibilitySetting) -> io::Result<()> {
        let default = self.defaults.get(&setting.provider).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "upstream does not support free-only mode",
            )
        })?;
        let _save = self.save_lock.lock().unwrap();
        let mut next = Overrides {
            upstreams: self.overrides.read().unwrap().upstreams.clone(),
        };
        next.upstreams
            .insert(default.state_key.clone(), setting.free_models_only);
        if let Some(path) = &self.path {
            crate::aliases::persist_json(path, &next)?;
        }
        *self.overrides.write().unwrap() = next;
        Ok(())
    }
}

impl VisibilityDefault {
    fn effective(&self, overrides: &Overrides) -> bool {
        overrides
            .upstreams
            .get(&self.state_key)
            .copied()
            .unwrap_or(self.free_only)
    }
}
