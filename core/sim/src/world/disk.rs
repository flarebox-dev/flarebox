// SPDX-License-Identifier: Apache-2.0

//! A device's synced folder in memory: entries keyed by inode, the parked directory, and the disk
//! side of every effect (docs/design/sync-engine.md sections 2.5 and 5.2). An effect checks the
//! state it expects and then changes the disk in one step, or changes nothing.

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
}

/// What an effect did to the disk.
#[derive(Clone, Debug, PartialEq)]
pub struct Applied {
    /// The identity of the entry the effect created.
    pub identity: Option<FileIdentity>,
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
    /// The last effect that reached the disk, with the entry it created. It stands for the
    /// evidence the fs driver leaves for recovery after a crash (section 5.3).
    applied: Option<(String, Option<FileIdentity>)>,
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

    /// True when some file, parked or not, holds the write with this tag.
    pub fn holds(&self, write: i64) -> bool {
        self.entries
            .values()
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

    /// The outcome of the effect with this ID, if it reached the disk and no effect did since.
    pub fn applied(&self, effect_id: &str) -> Option<Option<FileIdentity>> {
        self.applied
            .as_ref()
            .filter(|(id, _)| id == effect_id)
            .map(|(_, identity)| *identity)
    }

    /// Does one effect the way section 5.2 asks: every directory on the way has the identity the
    /// effect names, a displaced file has the identity and content the engine last saw, and no
    /// create or rename replaces an entry.
    pub fn apply(&mut self, record: &EffectRecord) -> Result<Applied, Refused> {
        let mut applied = Applied {
            identity: None,
            trashed: None,
        };
        match &record.effect {
            Effect::CreateDir { at } => {
                let (place, name) = self.locate(at)?;
                let ino = self.create(place, &name, None).ok_or(Refused::Taken)?;
                applied.identity = Some(identity(ino));
            }
            Effect::WriteFile {
                at,
                content,
                replaces,
            } => {
                let displaced = replaces.as_ref().map(|d| self.displaced(d)).transpose()?;
                let (place, name) = self.locate(at)?;
                if self
                    .child(&place, &name)
                    .is_some_and(|ino| Some(ino) != displaced)
                {
                    return Err(Refused::Taken);
                }
                applied.trashed = displaced.and_then(|ino| self.entries.remove(&ino)?.content);
                let ino = self.create(place, &name, Some(content.clone()));
                applied.identity = Some(identity(ino.ok_or(Refused::Taken)?));
            }
            Effect::Move {
                from,
                identity: moved,
                to,
            } => {
                let ino = self.at(from)?.filter(|ino| *ino == moved.ino);
                let ino = ino.ok_or(Refused::Moved)?;
                let (place, name) = self.locate(to)?;
                self.rename(ino, place, &name)?;
            }
            Effect::RemoveFile { displaced } => {
                let ino = self.displaced(displaced)?;
                applied.trashed = self.entries.remove(&ino).and_then(|entry| entry.content);
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
                self.entries.remove(&ino);
            }
        }
        self.applied = Some((record.effect_id.clone(), applied.identity));
        Ok(applied)
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
