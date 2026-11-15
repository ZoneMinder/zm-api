use std::{fs, path::PathBuf};

use serde::Deserialize;

use crate::util;

#[derive(Debug, Deserialize, Clone)]
pub struct SecretConfig {
    pub private_access_key: PathBuf,
    pub public_access_key: PathBuf,
    pub private_refresh_key: PathBuf,
    pub public_refresh_key: PathBuf,
}

/// Whether a file of this mode can be read by users other than its owner.
fn others_can_read(mode: u32) -> bool {
    mode & 0o044 != 0
}

/// Read a signing key, and say so in the log if other local users can read
/// it too: whoever holds it can mint a token for any user. It is a warning
/// and not a refusal, so an install that has run this way still starts.
fn read_private(path: PathBuf) -> Result<String, std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    let key = fs::read_to_string(&path)?;
    if fs::metadata(&path).is_ok_and(|m| others_can_read(m.permissions().mode())) {
        tracing::warn!(
            "the signing key {} is readable by other users; run `chmod 600` on it",
            path.display()
        );
    }
    Ok(key)
}

impl SecretConfig {
    pub fn read_private_access_key(&self) -> Result<String, std::io::Error> {
        read_private(util::dir::get_project_root()?.join(&self.private_access_key))
    }

    pub fn read_public_access_key(&self) -> Result<String, std::io::Error> {
        fs::read_to_string(util::dir::get_project_root()?.join(&self.public_access_key))
    }

    pub fn read_private_refresh_key(&self) -> Result<String, std::io::Error> {
        read_private(util::dir::get_project_root()?.join(&self.private_refresh_key))
    }

    pub fn read_public_refresh_key(&self) -> Result<String, std::io::Error> {
        fs::read_to_string(util::dir::get_project_root()?.join(&self.public_refresh_key))
    }
}

#[cfg(test)]
mod tests {
    use crate::constant::CONFIG;

    #[test]
    fn a_key_only_its_owner_can_read_is_the_only_safe_one() {
        use super::others_can_read;
        assert!(!others_can_read(0o600));
        assert!(!others_can_read(0o400));
        assert!(others_can_read(0o640));
        assert!(others_can_read(0o644));
        assert!(others_can_read(0o604));
    }

    #[test]
    fn test_read_private_access_key() {
        let key = CONFIG.secret.read_private_access_key().unwrap();
        assert!(!key.is_empty())
    }

    #[test]
    fn test_read_public_access_key() {
        let key = CONFIG.secret.read_public_access_key().unwrap();
        assert!(!key.is_empty())
    }

    #[test]
    fn test_read_private_refresh_key() {
        let key = CONFIG.secret.read_private_refresh_key().unwrap();
        assert!(!key.is_empty())
    }

    #[test]
    fn test_read_public_refresh_key() {
        let key = CONFIG.secret.read_public_refresh_key().unwrap();
        assert!(!key.is_empty())
    }
}
