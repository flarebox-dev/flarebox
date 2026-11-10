// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use flarebox_net::{Api, ApiError, Credentials};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::status::Account;

/// A device's long-lived credential, as `POST /v1/claim` or `POST /v1/pair/complete` returned it, with the server it
/// belongs to. It lives
/// in a file only its owner can read, in the profile's state directory and never inside a linked folder. One
/// credential is one writer: it is never copied to another machine or shared with another profile (H6 of
/// reports/BOX-71), so a second client on the same machine pairs on its own.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub server: String,
    pub user_id: String,
    /// The name given when this device claimed the server; none on a paired device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    pub device_id: String,
    pub device_name: String,
    pub refresh_credential: String,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("server", &self.server)
            .field("user_id", &self.user_id)
            .field("user_name", &self.user_name)
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
    #[error(
        "cannot keep the credential file {path} out of Time Machine backups ({source}); Flarebox does not use a credential that backups would copy. Restore write access to the file (it may be locked), keep the profile on storage that supports extended attributes, then restart Flarebox"
    )]
    NotExcludedFromBackups {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl Credential {
    pub fn account(&self) -> Account {
        Account {
            server: self.server.clone(),
            user_id: self.user_id.clone(),
            user_name: self.user_name.clone(),
            device_id: self.device_id.clone(),
            device_name: self.device_name.clone(),
        }
    }

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
    /// with [`CredentialError::Exists`] instead of replacing a credential. The file appears whole or not at all, and on
    /// macOS it is excluded from Time Machine before the credential is written to it.
    pub fn save_new(&self, path: &Path) -> Result<(), CredentialError> {
        NewCredentialFile::create(path)?.write(self)
    }

    /// Checks that the credential file at `path` is excluded from backups, and excludes it again if a write dropped
    /// the exclusion. The driver calls it each time it opens the profile, and does not use a credential it fails for.
    pub fn keep_out_of_backups(path: &Path) -> Result<(), CredentialError> {
        let file = File::open(path).map_err(|source| CredentialError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        #[cfg(target_os = "macos")]
        {
            let mut value = [0; 64];
            if let Ok(len) = rustix::fs::fgetxattr(&file, BACKUP_EXCLUSION.0, &mut value)
                && value[..len] == *BACKUP_EXCLUSION.1
            {
                return Ok(());
            }
            tracing::info!(path = %path.display(), "excluding the credential from backups again");
        }
        exclude_from_backups(&file).map_err(|source| CredentialError::NotExcludedFromBackups {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// A credential file that is ready before the credential exists: a hidden temporary file, readable only by its owner
/// and excluded from backups. Pairing and claiming create it before they ask the server for a credential, so a profile
/// that cannot hold a credential fails before the server uses up the code. Dropping it removes the temporary file.
pub(crate) struct NewCredentialFile {
    path: PathBuf,
    temp: PathBuf,
    file: File,
}

impl NewCredentialFile {
    /// Creates the temporary file for the credential file at `path`, creating the directory with mode 0700.
    pub fn create(path: &Path) -> Result<Self, CredentialError> {
        let dir = path.parent().unwrap_or(Path::new("."));
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|source| CredentialError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let temp = dir.join(format!(".credential.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&temp);
        let file = create_excluded(&temp, path).inspect_err(|_| {
            let _ = fs::remove_file(&temp);
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            temp,
            file,
        })
    }

    /// Writes `credential` and moves it into place. Fails with [`CredentialError::Exists`] instead of replacing a
    /// credential. The file appears whole or not at all.
    pub fn write(mut self, credential: &Credential) -> Result<(), CredentialError> {
        let io = |source| CredentialError::Io {
            path: self.path.clone(),
            source,
        };
        let written = (|| {
            self.file
                .write_all(&serde_json::to_vec_pretty(credential)?)?;
            self.file.sync_all()?;
            fs::hard_link(&self.temp, &self.path)
        })();
        let _ = fs::remove_file(&self.temp);
        match written {
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(CredentialError::Exists {
                    path: self.path.clone(),
                });
            }
            other => other.map_err(io)?,
        }
        let dir = self.path.parent().unwrap_or(Path::new("."));
        File::open(dir).and_then(|dir| dir.sync_all()).map_err(io)
    }
}

impl Drop for NewCredentialFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temp);
    }
}

/// The extended attribute that keeps a file out of Time Machine backups, and the value `tmutil addexclusion` writes.
#[cfg(target_os = "macos")]
pub(crate) const BACKUP_EXCLUSION: (&str, &[u8]) = (
    "com.apple.metadata:com_apple_backup_excludeItem",
    b"bplist00_\x10\x11com.apple.backupd\x08\x00\x00\x00\x00\x00\x00\x01\x01\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x1c",
);

/// Creates the empty file `temp`, readable only by its owner, and excludes it from backups before anything is written
/// to it, so that no backup can hold a copy of the credential. Errors name `path`, the credential file it becomes.
fn create_excluded(temp: &Path, path: &Path) -> Result<File, CredentialError> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(temp)
        .map_err(|source| CredentialError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    exclude_from_backups(&file).map_err(|source| CredentialError::NotExcludedFromBackups {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(file)
}

/// Keeps the credential file out of Time Machine, so that a Mac restored from a backup has no credential and pairs as
/// a new device. Nothing to do on other systems.
fn exclude_from_backups(file: &File) -> io::Result<()> {
    #[cfg(test)]
    if tests::REFUSE_EXCLUSION.get() {
        return Err(io::Error::from(io::ErrorKind::Unsupported));
    }
    #[cfg(target_os = "macos")]
    rustix::fs::fsetxattr(
        file,
        BACKUP_EXCLUSION.0,
        BACKUP_EXCLUSION.1,
        rustix::fs::XattrFlags::empty(),
    )?;
    #[cfg(not(target_os = "macos"))]
    let _ = file;
    Ok(())
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
pub(crate) mod tests {
    use std::cell::Cell;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    thread_local! {
        /// Makes this thread's backup exclusions fail, as on storage without extended attributes.
        pub(crate) static REFUSE_EXCLUSION: Cell<bool> = const { Cell::new(false) };
    }

    /// Whether `tmutil isexcluded` reports that Time Machine leaves `path` out. The temporary directory is excluded
    /// as a whole, so the tests that call it work under the home directory.
    #[cfg(target_os = "macos")]
    pub(crate) fn time_machine_excludes(path: &Path) -> bool {
        let out = std::process::Command::new("tmutil")
            .arg("isexcluded")
            .arg(path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let said = String::from_utf8(out.stdout).unwrap();
        assert!(
            said.starts_with("[Excluded]") || said.starts_with("[Included]"),
            "{said}"
        );
        said.starts_with("[Excluded]")
    }

    fn credential() -> Credential {
        Credential {
            server: "https://files.example.com/".into(),
            user_id: "u1".into(),
            user_name: Some("Ada".into()),
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
    fn a_credential_without_a_user_name_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential.json");
        let paired = Credential {
            user_name: None,
            ..credential()
        };
        paired.save_new(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("user_name"));
        assert_eq!(Credential::load(&path).unwrap(), Some(paired));
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

    #[cfg(target_os = "macos")]
    #[test]
    fn the_credential_file_is_excluded_before_its_first_byte() {
        let home = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        let temp = home.path().join(".credential.tmp");
        let file = create_excluded(&temp, &home.path().join("credential.json")).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
        assert!(time_machine_excludes(&temp));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn time_machine_leaves_out_a_new_credential() {
        let home = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        let path = home.path().join("state").join("credential.json");
        credential().save_new(&path).unwrap();
        assert!(time_machine_excludes(&path));

        Credential::keep_out_of_backups(&path).unwrap();
        assert!(time_machine_excludes(&path));
    }
}
