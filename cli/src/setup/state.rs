// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;

use koidrive_cloudflare::Jurisdiction;
use koidrive_driver::Paths;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    #[default]
    Planned,
    BucketCreated,
    ScriptUploaded,
    Serving,
    Uninstalling,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupState {
    pub account: String,
    pub script: String,
    pub bucket: String,
    pub step: Step,
    pub secret_set: bool,
    pub setup_attempt: Option<String>,
    #[serde(default)]
    pub script_created_on: Option<String>,
    #[serde(default)]
    pub jurisdiction: Jurisdiction,
    #[serde(default)]
    pub bucket_created_on: Option<String>,
}

pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    pub fn new(paths: &Paths) -> Self {
        StateFile {
            path: paths.state_dir.join("setup-state.json"),
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn load(&self) -> Result<Option<SetupState>, String> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                format!(
                    "{} is not a valid setup state ({error}); delete it to start over",
                    self.path.display()
                )
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("cannot read {}: {error}", self.path.display())),
        }
    }

    pub fn save(&self, state: &SetupState) -> Result<(), String> {
        let write = || -> io::Result<()> {
            let dir = self.path.parent().expect("the state file has a directory");
            DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            let temporary = self.path.with_extension("json.tmp");
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(state).expect("the state serializes"))?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            fs::File::open(dir)?.sync_all()
        };
        write().map_err(|error| format!("cannot write {}: {error}", self.path.display()))
    }

    pub fn remove(&self) -> Result<(), String> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("cannot delete {}: {error}", self.path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_loads_and_removes_the_state_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let file = StateFile::new(&paths);
        assert_eq!(file.load().unwrap(), None);
        let state = SetupState {
            account: "account".into(),
            script: "koidrive".into(),
            bucket: "koidrive-2".into(),
            step: Step::ScriptUploaded,
            secret_set: true,
            setup_attempt: Some("attempt".into()),
            script_created_on: Some("2026-11-10T01:02:03.456Z".into()),
            jurisdiction: Jurisdiction::Eu,
            bucket_created_on: Some("2026-11-10T01:02:00.000Z".into()),
        };
        file.save(&state).unwrap();
        assert_eq!(file.load().unwrap(), Some(state));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(file.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        file.remove().unwrap();
        file.remove().unwrap();
        assert_eq!(file.load().unwrap(), None);
    }
}
