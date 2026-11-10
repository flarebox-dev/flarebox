// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use flarebox_net::{Api, ApiError, Credentials};
use serde::{Deserialize, Serialize};
use url::Url;

/// A device's long-lived credential, as `POST /v1/pair/complete` returned it, with the server it belongs to. It lives
/// in a file only its owner can read, in the profile's state directory and never inside a linked folder. One
/// credential is one writer: it is never copied to another machine or shared with another profile (H6 of
/// reports/BOX-71), so a second client on the same machine pairs on its own.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub server: String,
    pub user_id: String,
    pub device_id: String,
    pub device_name: String,
    pub refresh_credential: String,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("server", &self.server)
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .field("device_name", &self.device_name)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("cannot use the credential file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the credential file {path} is not valid: {reason}")]
    Invalid { path: PathBuf, reason: String },
    #[error(
        "the credential file {path} can be read by other users (mode {mode:o}); run chmod 600 on it"
    )]
    Exposed { path: PathBuf, mode: u32 },
    #[error("the credential file {path} belongs to another user")]
    NotOwned { path: PathBuf },
    #[error("this profile already has a credential at {path}")]
    Exists { path: PathBuf },
}

impl Credential {
    pub fn net(&self) -> Credentials {
        Credentials {
            user_id: self.user_id.clone(),
            device_id: self.device_id.clone(),
            refresh_credential: self.refresh_credential.clone(),
        }
    }

    pub fn api(&self) -> Result<Api, ApiError> {
        api_for(&self.server)
    }

    /// Reads the credential, or returns `None` when there is none. A file that another user owns or may read is
    /// refused rather than used.
    pub fn load(path: &Path) -> Result<Option<Self>, CredentialError> {
        let io = |source| CredentialError::Io {
            path: path.to_path_buf(),
            source,
        };
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io(err)),
        };
        let meta = file.metadata().map_err(io)?;
        if meta.uid() != rustix::process::getuid().as_raw() {
            return Err(CredentialError::NotOwned {
                path: path.to_path_buf(),
            });
        }
        if meta.mode() & 0o077 != 0 {
            return Err(CredentialError::Exposed {
                path: path.to_path_buf(),
                mode: meta.mode() & 0o777,
            });
        }
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(io)?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|err| CredentialError::Invalid {
                path: path.to_path_buf(),
                reason: err.to_string(),
            })
    }

    /// Writes the credential to a new file readable only by its owner, creating the directory with mode 0700. Fails
    /// with [`CredentialError::Exists`] instead of replacing a credential. The file appears whole or not at all.
    pub fn save_new(&self, path: &Path) -> Result<(), CredentialError> {
        let io = |source| CredentialError::Io {
            path: path.to_path_buf(),
            source,
        };
        let dir = path.parent().unwrap_or(Path::new("."));
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io)?;
        let temp = dir.join(format!(".credential.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&temp);
        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                .open(&temp)?;
            file.write_all(&serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
            fs::hard_link(&temp, path)
        })();
        let _ = fs::remove_file(&temp);
        match written {
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return Err(CredentialError::Exists {
                    path: path.to_path_buf(),
                });
            }
            other => other.map_err(io)?,
        }
        File::open(dir).and_then(|dir| dir.sync_all()).map_err(io)
    }
}

/// The API of the server at `server`. Plain `http` is accepted only on a loopback address, for a server under
/// `wrangler dev`.
pub fn api_for(server: &str) -> Result<Api, ApiError> {
    let url = Url::parse(server).map_err(|err| ApiError::Url(format!("{server}: {err}")))?;
    if url.scheme() == "http" {
        Api::local_development(url)
    } else {
        Api::new(url)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn credential() -> Credential {
        Credential {
            server: "https://files.example.com/".into(),
            user_id: "u1".into(),
            device_id: "d1".into(),
            device_name: "vm".into(),
            refresh_credential: "secret-refresh".into(),
        }
    }

    #[test]
    fn a_saved_credential_is_private_and_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/credential.json");
        assert!(Credential::load(&path).unwrap().is_none());
        credential().save_new(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(Credential::load(&path).unwrap(), Some(credential()));
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn a_credential_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential.json");
        credential().save_new(&path).unwrap();
        let other = Credential {
            device_id: "d2".into(),
            ..credential()
        };
        assert!(matches!(
            other.save_new(&path),
            Err(CredentialError::Exists { .. })
        ));
        assert_eq!(Credential::load(&path).unwrap(), Some(credential()));
    }

    #[test]
    fn a_credential_others_can_read_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential.json");
        credential().save_new(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Credential::load(&path),
            Err(CredentialError::Exposed { mode: 0o644, .. })
        ));
    }

    #[test]
    fn debug_output_leaves_out_the_refresh_credential() {
        let text = format!("{:?}", credential());
        assert!(text.contains("d1"), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn plain_http_is_only_for_loopback() {
        assert!(api_for("http://127.0.0.1:8787").is_ok());
        assert!(api_for("https://files.example.com").is_ok());
        assert!(api_for("http://files.example.com").is_err());
        assert!(api_for("files.example.com").is_err());
    }
}
