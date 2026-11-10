// SPDX-License-Identifier: Apache-2.0

//! The state file of a setup, upgrade or uninstall that may stop halfway (docs/design/cli-setup.md section 4.6). It
//! names what the run created and how far it got, so a rerun on this machine continues. It holds no secret.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;

use flarebox_cloudflare::Jurisdiction;
use flarebox_driver::Paths;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Names are chosen; nothing exists yet.
    #[default]
    Planned,
    BucketCreated,
    ScriptUploaded,
    /// The server answers on workers.dev and is waiting for this device's claim.
    Serving,
    /// An uninstall is under way; the Worker may already be gone.
    Uninstalling,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupState {
    pub account: String,
    pub script: String,
    pub bucket: String,
    pub step: Step,
    /// This CLI may have set a one-time `FLAREBOX_RECOVERY_KEY` that is not deleted yet.
    pub secret_set: bool,
    /// The setup attempt id the claims of this run send, until one claim's credential is saved.
    pub setup_attempt: Option<String>,
    /// During an uninstall, when the Worker was created, which tells it apart from a new Worker of the same name.
    #[serde(default)]
    pub script_created_on: Option<String>,
    /// The jurisdiction of the bucket, which every R2 call names.
    #[serde(default)]
    pub jurisdiction: Jurisdiction,
    /// During an uninstall, when the bucket was created, which tells it apart from a new bucket of the same name.
    #[serde(default)]
    pub bucket_created_on: Option<String>,
    /// When this deploy first tried to create its bucket, in milliseconds since the Unix epoch by this machine's clock.
    /// A bucket the create finds already there counts as this deploy's only if R2 dates it later (section 4.6).
    #[serde(default)]
    pub bucket_create_started_ms: Option<i64>,
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

    /// Writes the state to a temporary file, syncs it and renames it into place, so a crash leaves the old state or
    /// the new one.
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
            script: "flarebox".into(),
            bucket: "flarebox-2".into(),
            step: Step::ScriptUploaded,
            secret_set: true,
            setup_attempt: Some("attempt".into()),
            script_created_on: Some("2026-11-10T01:02:03.456Z".into()),
            jurisdiction: Jurisdiction::Eu,
            bucket_created_on: Some("2026-11-10T01:02:00.000Z".into()),
            bucket_create_started_ms: Some(1_794_272_400_000),
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
