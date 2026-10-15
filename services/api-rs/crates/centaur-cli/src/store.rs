//! On-disk profiles and credentials.
//!
//! Everything lives in one TOML file under the config directory, written with
//! owner-only permissions because it holds refresh tokens.

use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

use eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};

const CREDENTIALS_FILE: &str = "credentials.toml";
const LOCK_FILE: &str = "credentials.lock";

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct Store {
    #[serde(default)]
    pub(crate) profiles: BTreeMap<String, Profile>,
}

/// One signed-in deployment.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Profile {
    pub(crate) api_url: String,
    pub(crate) client_id: String,
    pub(crate) token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) revocation_endpoint: Option<String>,
    pub(crate) refresh_token: String,
    pub(crate) access_token: String,
    /// Unix seconds.
    pub(crate) access_token_expires_at: i64,
}

/// An exclusive hold on the credentials, released on drop. Refresh tokens
/// rotate on use, so commands refresh one at a time and each reads the token
/// the previous one stored.
pub(crate) struct StoreLock {
    _file: fs::File,
}

impl Store {
    /// Blocks until no other `centaur` process holds the lock.
    pub(crate) fn lock() -> Result<StoreLock> {
        Self::lock_in(&config_dir()?)
    }

    fn lock_in(dir: &Path) -> Result<StoreLock> {
        create_private_dir(dir)?;
        let path = dir.join(LOCK_FILE);
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .wrap_err_with(|| format!("failed to open {}", path.display()))?;
        file.lock()
            .wrap_err_with(|| format!("failed to lock {}", path.display()))?;
        Ok(StoreLock { _file: file })
    }

    pub(crate) fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join(CREDENTIALS_FILE))
    }

    pub(crate) fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }

    pub(crate) fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    fn load_from(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(raw) => {
                toml::from_str(&raw).wrap_err_with(|| format!("failed to parse {}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).wrap_err_with(|| format!("failed to read {}", path.display())),
        }
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| eyre!("{} has no parent directory", path.display()))?;
        create_private_dir(dir)?;
        let raw = toml::to_string(self).wrap_err("failed to encode credentials")?;
        // Write a sibling file and rename so a crash never leaves a truncated
        // credentials file behind.
        let staging = dir.join(format!(".{CREDENTIALS_FILE}.{}", uuid::Uuid::new_v4()));
        let result = write_private_file(&staging, raw.as_bytes())
            .and_then(|()| fs::rename(&staging, path).map_err(Into::into));
        if result.is_err() {
            let _ = fs::remove_file(&staging);
        }
        result.wrap_err_with(|| format!("failed to write {}", path.display()))
    }
}

/// `$CENTAUR_CONFIG_DIR`, else `$XDG_CONFIG_HOME/centaur`, else
/// `~/.config/centaur`.
fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = non_empty_env("CENTAUR_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = non_empty_env("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(dir).join("centaur"));
    }
    env::home_dir()
        .map(|home| home.join(".config").join("centaur"))
        .ok_or_else(|| eyre!("cannot locate a home directory; set CENTAUR_CONFIG_DIR"))
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    if dir.is_dir() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .wrap_err_with(|| format!("failed to create {}", dir.display()))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).wrap_err_with(|| format!("failed to create {}", dir.display()))
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Profile {
        Profile {
            api_url: "https://api.example.com".to_owned(),
            client_id: "moc_test".to_owned(),
            token_endpoint: "https://console.example.com/mcp/oauth/token".to_owned(),
            revocation_endpoint: None,
            refresh_token: "refresh".to_owned(),
            access_token: "access".to_owned(),
            access_token_expires_at: 1_700_000_000,
        }
    }

    #[test]
    fn missing_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::load_from(&dir.path().join(CREDENTIALS_FILE)).unwrap();
        assert!(store.profiles.is_empty());
    }

    #[test]
    fn round_trips_profiles_through_an_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(CREDENTIALS_FILE);
        let mut store = Store::default();
        store.profiles.insert("default".to_owned(), profile());
        store.save_to(&path).unwrap();
        store.save_to(&path).unwrap();

        let loaded = Store::load_from(&path).unwrap();
        assert_eq!(loaded.profiles["default"].client_id, "moc_test");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir_mode = fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn the_lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let held = Store::lock_in(dir.path()).unwrap();
        let other = fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join(LOCK_FILE))
            .unwrap();
        assert!(other.try_lock().is_err());
        drop(held);
        assert!(other.try_lock().is_ok());
    }
}
