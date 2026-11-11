// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Where one profile keeps its files. A profile is one device credential and the stores it writes; the Linux CLI
/// uses the XDG base directories, and the macOS app can pass its own container to [`Paths::under`].
///
/// - The configuration file is edited by the person and may be shared between machines. It holds no secrets and no
///   sync state, and it is never synced.
/// - The state directory belongs to this machine and is never copied: the credential, the profile lock and one
///   store per linked space. A copied credential or store breaks the rule that one credential writes one store
///   (H6 of reports/BOX-71, RFC 0002 section 6).
/// - The runtime directory holds the daemon's socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathsError {
    #[error("HOME is not set to an absolute path, and no XDG directory replaces it")]
    NoHome,
    #[error("{0:?} is not a space id")]
    BadSpaceId(String),
}

impl Paths {
    /// The XDG base directories (`XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`) with their defaults under
    /// `HOME`. Relative values are ignored, as the XDG specification requires. Without `XDG_RUNTIME_DIR` the socket
    /// lives in the state directory.
    pub fn from_env() -> Result<Self, PathsError> {
        Self::from_vars(|name| std::env::var_os(name))
    }

    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Result<Self, PathsError> {
        let absolute = |name: &str| {
            var(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let home = absolute("HOME");
        let base = |name: &str, default: &str| {
            absolute(name)
                .or_else(|| home.as_ref().map(|home| home.join(default)))
                .ok_or(PathsError::NoHome)
        };
        let config_dir = base("XDG_CONFIG_HOME", ".config")?.join("koidrive");
        let state_dir = base("XDG_STATE_HOME", ".local/state")?.join("koidrive");
        let runtime_dir = absolute("XDG_RUNTIME_DIR")
            .map(|dir| dir.join("koidrive"))
            .unwrap_or_else(|| state_dir.clone());
        Ok(Paths {
            config_file: config_dir.join("config.toml"),
            state_dir,
            runtime_dir,
        })
    }

    /// Everything under one directory, for an app container or a test.
    pub fn under(dir: &Path) -> Self {
        Paths {
            config_file: dir.join("config.toml"),
            state_dir: dir.join("state"),
            runtime_dir: dir.join("state"),
        }
    }

    pub fn credential(&self) -> PathBuf {
        self.state_dir.join("credential.json")
    }

    pub fn lock(&self) -> PathBuf {
        self.state_dir.join("lock")
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("daemon.sock")
    }

    pub fn spaces(&self) -> PathBuf {
        self.state_dir.join("spaces")
    }

    /// The store of one space. Space ids come from the server, so only ids made of hex digits and dashes name a
    /// directory.
    pub fn store(&self, space_id: &str) -> Result<PathBuf, PathsError> {
        if space_id.is_empty() || !space_id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Err(PathsError::BadSpaceId(space_id.to_owned()));
        }
        Ok(self.spaces().join(space_id).join("store.sqlite"))
    }

    /// The directories a linked folder must neither contain nor sit inside, so that the credential, the stores and
    /// the configuration are never synced.
    pub fn private_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.state_dir.clone(), self.runtime_dir.clone()];
        dirs.extend(self.config_file.parent().map(Path::to_path_buf));
        dirs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn defaults_live_under_home() {
        let paths = Paths::from_vars(vars(&[("HOME", "/home/ada")])).unwrap();
        assert_eq!(
            paths.config_file,
            Path::new("/home/ada/.config/koidrive/config.toml")
        );
        assert_eq!(
            paths.state_dir,
            Path::new("/home/ada/.local/state/koidrive")
        );
        assert_eq!(
            paths.socket(),
            Path::new("/home/ada/.local/state/koidrive/daemon.sock")
        );
        assert_eq!(
            paths.credential(),
            Path::new("/home/ada/.local/state/koidrive/credential.json")
        );
    }

    #[test]
    fn xdg_directories_win_and_relative_ones_are_ignored() {
        let paths = Paths::from_vars(vars(&[
            ("HOME", "/home/ada"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_STATE_HOME", "relative/state"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]))
        .unwrap();
        assert_eq!(paths.config_file, Path::new("/cfg/koidrive/config.toml"));
        assert_eq!(
            paths.state_dir,
            Path::new("/home/ada/.local/state/koidrive")
        );
        assert_eq!(
            paths.socket(),
            Path::new("/run/user/1000/koidrive/daemon.sock")
        );
    }

    #[test]
    fn no_home_and_no_xdg_is_an_error() {
        assert_eq!(
            Paths::from_vars(vars(&[("HOME", "relative")])),
            Err(PathsError::NoHome)
        );
    }

    #[test]
    fn only_space_ids_name_store_directories() {
        let paths = Paths::under(Path::new("/p"));
        assert_eq!(
            paths.store("0190f0a0-0000-7000-8000-00000000000a").unwrap(),
            Path::new("/p/state/spaces/0190f0a0-0000-7000-8000-00000000000a/store.sqlite")
        );
        for bad in ["", "..", "a/b", "../etc", "space name"] {
            assert!(paths.store(bad).is_err(), "{bad}");
        }
    }
}
