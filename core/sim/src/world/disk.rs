// SPDX-License-Identifier: Apache-2.0

//! A device's synced folder in memory: entries keyed by inode, the parked directory, and the disk
//! side of every effect (docs/design/sync-engine.md sections 2.5, 5.2 and 5.3). An effect checks
//! the state it expects, sends the entry it displaces to quarantine, gives the new entry its
//! name, and sends the quarantined entry to the local trash. A crash can stop it between these
//! steps. Recovery then decides from the target, the staged download and the quarantine, as
//! `flarebox_fs::Space::recover` does, and the disk keeps no record of the effects it has done.
//!
//! Two things differ from the fs driver. The disk checks a displaced entry before it moves it to
//! quarantine, so a quarantined entry is never returned to its name. And it refuses to remove a
//! folder that has entries before it moves the folder, so a folder with entries is never in
//! quarantine.

use std::collections::BTreeMap;

use flarebox_engine::effects::{Base, Displaced, Effect, EffectRecord, Location};
use flarebox_engine::tree::{Content, FileIdentity, same_content};

/// Where an entry sits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Place {
    Root,
    /// Inside the directory with this inode.
    In(u64),
    /// In the parked directory, under this name.
    Parked(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub place: Place,
    pub name: String,
    /// The file's content, or `None` for a directory.
    pub content: Option<Content>,
    /// The manifest digest of every content this file has held, so that a user overwrite never
    /// brings back bytes the file held before.
    pub held: Vec<Vec<u8>>,
}

/// Why the disk did not do an effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// An entry the effect names is gone, or is another entry now.
    Moved,
    /// The file an effect displaces holds other content.
    Changed,
    /// The target name is taken.
    Taken,
    /// The directory to remove has entries.
    NotEmpty,
    /// The target is inside the directory that moves.
    Cycle,
    /// Recovery found that the effect stopped before it displaced anything, or that its target
    /// is not in the effect's final state any more.
    Interrupted,
}

/// How far an effect gets on the disk before the device crashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cut {
    /// The entry it displaces is in quarantine, its download is staged, and the new entry has no
    /// name yet. An effect that neither displaces nor downloads has changed nothing.
    Quarantined,
    /// The new entry has its name, or found it taken. A displaced entry is still in quarantine.
    Placed,
    /// The effect is complete.
    Done,
}

/// What an effect, or the recovery of one, did to the disk.
#[derive(Clone, Debug, PartialEq)]
pub struct Applied {
    /// The identity of the entry the effect created, or why the disk did not do the effect.
    pub outcome: Result<Option<FileIdentity>, Refused>,
    /// The content of the file the effect sent to the local trash.
    pub trashed: Option<Content>,
}

pub fn identity(ino: u64) -> FileIdentity {
    FileIdentity {
        dev: 1,
        ino,
        birth_ns: Some(1_700_000_000_000_000_000),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Disk {
    entries: BTreeMap<u64, Entry>,
    inos: u64,
    /// The download of the effect in flight is staged and has no name in the folder yet.
    pub(super) staged: bool,
    /// The entry that the effect in flight displaced and has not sent to the local trash yet.
    pub(super) quarantine: Option<Entry>,
}

impl Disk {
    pub fn entries(&self) -> &BTreeMap<u64, Entry> {
        &self.entries
    }

    pub fn children<'a>(&'a self, place: &'a Place) -> impl Iterator<Item = u64> + 'a {
        self.entries
            .iter()
            .filter(move |(_, entry)| entry.place == *place)
            .map(|(ino, _)| *ino)
    }

    pub fn child(&self, place: &Place, name: &str) -> Option<u64> {
        self.children(place)
            .find(|ino| self.entries[ino].name == name)
    }

    /// The names from the root to the entry, or `None` for an entry in the parked directory.
    pub fn path(&self, ino: u64) -> Option<Vec<String>> {
        let mut names = Vec::new();
        let mut current = ino;
        loop {
            let entry = self.entries.get(&current)?;
            names.push(entry.name.clone());
            match &entry.place {
                Place::Root => break,
                Place::In(parent) => current = *parent,
                Place::Parked(_) => return None,
            }
        }
        names.reverse();
        Some(names)
    }

    /// The entries the user sees, which leaves out the parked directory, in inode order.
    pub fn visible(&self) -> Vec<u64> {
        self.entries
            .keys()
            .copied()
            .filter(|ino| self.path(*ino).is_some())
            .collect()
    }

    /// True when `ancestor` is `place` or holds it.
    pub fn inside(&self, place: &Place, ancestor: u64) -> bool {
        let mut current = place;
        while let Place::In(ino) = current {
            if *ino == ancestor {
                return true;
            }
            current = &self.entries[ino].place;
        }
        false
    }

    /// True when some file, parked, in quarantine or neither, holds the write with this tag.
    pub fn holds(&self, write: i64) -> bool {
        (self.entries.values())
            .chain(&self.quarantine)
            .any(|entry| entry.content.as_ref().is_some_and(|c| c.mtime_ns == write))
    }

    /// Creates an entry, unless the name is taken.
    pub fn create(&mut self, place: Place, name: &str, content: Option<Content>) -> Option<u64> {
        if self.child(&place, name).is_some() {
            return None;
        }
        self.inos += 1;
        let held = content.iter().map(|c| c.manifest_digest.clone()).collect();
        let entry = Entry {
            place,
            name: name.to_owned(),
            content,
            held,
        };
        self.entries.insert(self.inos, entry);
        Some(self.inos)
    }

    /// Replaces a file's content in place and returns the content it held.
    pub fn overwrite(&mut self, ino: u64, content: Content) -> Option<Content> {
        let entry = self.entries.get_mut(&ino)?;
        entry.held.push(content.manifest_digest.clone());
        entry.content.replace(content)
    }

    /// Moves an entry, unless the name is taken or the target is inside the entry.
    pub fn rename(&mut self, ino: u64, place: Place, name: &str) -> Result<(), Refused> {
        if self.child(&place, name).is_some() {
            return Err(Refused::Taken);
        }
        if self.inside(&place, ino) {
            return Err(Refused::Cycle);
        }
        let entry = self.entries.get_mut(&ino).ok_or(Refused::Moved)?;
        entry.place = place;
        entry.name = name.to_owned();
        Ok(())
    }

    /// Deletes an entry with everything in it and returns the contents of the deleted files.
    pub fn delete(&mut self, ino: u64) -> Vec<Content> {
        let doomed: Vec<u64> = self
            .entries
            .iter()
            .filter(|(other, entry)| **other == ino || self.inside(&entry.place, ino))
            .map(|(other, _)| *other)
            .collect();
        doomed
            .iter()
            .filter_map(|ino| self.entries.remove(ino)?.content)
            .collect()
    }

    /// Does one effect the way section 5.2 asks, as far as `until`: every directory on the way
    /// has the identity the effect names, a displaced file has the identity and content the
    /// engine last saw, and no create or rename replaces an entry. A displaced entry goes to
    /// the local trash also when the new entry finds its name taken.
    pub fn apply(&mut self, record: &EffectRecord, until: Cut) -> Applied {
        let mut trashed = None;
        let outcome = self.change(&record.effect, until, &mut trashed);
        if until == Cut::Done {
            self.staged = false;
        }
        Applied { outcome, trashed }
    }

    fn change(
        &mut self,
        effect: &Effect,
        until: Cut,
        trashed: &mut Option<Content>,
    ) -> Result<Option<FileIdentity>, Refused> {
        match effect {
            Effect::CreateDir { at } => {
                let (place, name) = self.locate(at)?;
                if until == Cut::Quarantined {
                    return Ok(None);
                }
                let ino = self.create(place, &name, None).ok_or(Refused::Taken)?;
                Ok(Some(identity(ino)))
            }
            Effect::WriteFile {
                at,
                content,
                replaces,
            } => {
                self.staged = true;
                let displaced = replaces.as_ref().map(|d| self.displaced(d)).transpose()?;
                let (place, name) = self.locate(at)?;
                if let Some(ino) = displaced {
                    self.quarantine = self.entries.remove(&ino);
                }
                if until == Cut::Quarantined {
                    return Ok(None);
                }
                let ino = self.create(place, &name, Some(content.clone()));
                self.staged = ino.is_none();
                if until == Cut::Done {
                    *trashed = self.quarantine.take().and_then(|entry| entry.content);
                }
                ino.map(|ino| Some(identity(ino))).ok_or(Refused::Taken)
            }
            Effect::Move {
                from,
                identity: moved,
                to,
            } => {
                let ino = self.at(from)?.filter(|ino| *ino == moved.ino);
                let ino = ino.ok_or(Refused::Moved)?;
                let (place, name) = self.locate(to)?;
                if until == Cut::Quarantined {
                    return Ok(None);
                }
                self.rename(ino, place, &name)?;
                Ok(None)
            }
            Effect::RemoveFile { displaced } => {
                let ino = self.displaced(displaced)?;
                self.quarantine = self.entries.remove(&ino);
                if until == Cut::Done {
                    *trashed = self.quarantine.take().and_then(|entry| entry.content);
                }
                Ok(None)
            }
            Effect::RemoveDir { at, identity: gone } => {
                let ino = self.at(at)?.filter(|ino| *ino == gone.ino);
                let ino = ino.ok_or(Refused::Moved)?;
                if self.entries[&ino].content.is_some() {
                    return Err(Refused::Changed);
                }
                if self.children(&Place::In(ino)).next().is_some() {
                    return Err(Refused::NotEmpty);
                }
                self.quarantine = self.entries.remove(&ino).filter(|_| until != Cut::Done);
                Ok(None)
            }
        }
    }

    /// Ends the effect in flight after a crash (section 5.3). An entry in quarantine means that
    /// the effect displaced it: the staged download gets its name if it has none yet, and the
    /// entry goes to the local trash. With nothing in quarantine and nothing staged, the effect
    /// is done when its target is in the effect's final state. Anything else is an effect that
    /// stopped early or whose target the user has changed since.
    pub fn recover(&mut self, record: &EffectRecord) -> Applied {
        let staged = std::mem::take(&mut self.staged);
        let Some(entry) = self.quarantine.take() else {
            let outcome = match self.finished(&record.effect) {
                Some(identity) if !staged => Ok(identity),
                _ => Err(Refused::Interrupted),
            };
            return Applied {
                outcome,
                trashed: None,
            };
        };
        let outcome = match &record.effect {
            Effect::WriteFile { at, content, .. } if staged => {
                self.locate(at).and_then(|(place, name)| {
                    let ino = self.create(place, &name, Some(content.clone()));
                    Ok(Some(identity(ino.ok_or(Refused::Taken)?)))
                })
            }
            Effect::WriteFile { at, content, .. } => {
                (self.written(at, content).map(Some)).ok_or(Refused::Taken)
            }
            _ => Ok(None),
        };
        Applied {
            outcome,
            trashed: entry.content,
        }
    }

    /// The identity of the entry an effect made, when the disk is in the effect's final state.
    fn finished(&self, effect: &Effect) -> Option<Option<FileIdentity>> {
        match effect {
            Effect::CreateDir { at } => {
                let ino = self.at(at).ok()??;
                (self.entries[&ino].content.is_none()).then(|| Some(identity(ino)))
            }
            Effect::WriteFile { at, content, .. } => self.written(at, content).map(Some),
            Effect::Move {
                identity: moved,
                to,
                ..
            } => (self.at(to).ok()? == Some(moved.ino)).then_some(None),
            Effect::RemoveFile {
                displaced: Displaced { at, .. },
            }
            | Effect::RemoveDir { at, .. } => self.at(at).ok()?.is_none().then_some(None),
        }
    }

    /// The identity of the file at `at`, when it holds `content`.
    fn written(&self, at: &Location, content: &Content) -> Option<FileIdentity> {
        let ino = self.at(at).ok()??;
        let held = self.entries[&ino].content.as_ref()?;
        same_content(held, content).then(|| identity(ino))
    }

    /// The file an effect displaces, when it still is the file the engine saw.
    fn displaced(&self, displaced: &Displaced) -> Result<u64, Refused> {
        let ino = self
            .at(&displaced.at)?
            .filter(|ino| *ino == displaced.identity.ino);
        let ino = ino.ok_or(Refused::Moved)?;
        match &self.entries[&ino].content {
            Some(content) if same_content(content, &displaced.content) => Ok(ino),
            _ => Err(Refused::Changed),
        }
    }

    /// The entry at a location, if one is there.
    fn at(&self, at: &Location) -> Result<Option<u64>, Refused> {
        let (place, name) = self.locate(at)?;
        Ok(match place {
            Place::Parked(_) => self.children(&place).next(),
            place => self.child(&place, &name),
        })
    }

    /// The directory a location is in and the name it has there. An entry in the parked directory
    /// is named after the node it is parked as.
    fn locate(&self, at: &Location) -> Result<(Place, String), Refused> {
        let (base, dirs, name) = match at {
            Location::Parked(node) => return Ok((Place::Parked(node.clone()), node.clone())),
            Location::At { base, dirs, name } => (base, dirs, name),
        };
        let mut place = match base {
            Base::Root => Place::Root,
            Base::Parked { node, identity } => {
                let parked = self.children(&Place::Parked(node.clone())).next();
                let ino = parked.filter(|ino| *ino == identity.ino);
                Place::In(ino.ok_or(Refused::Moved)?)
            }
        };
        for dir in dirs {
            let ino = self.child(&place, &dir.name);
            let ino =
                ino.filter(|ino| *ino == dir.identity.ino && self.entries[ino].content.is_none());
            place = Place::In(ino.ok_or(Refused::Moved)?);
        }
        Ok((place, name.clone()))
    }
}
