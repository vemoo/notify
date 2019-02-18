extern crate walkdir;

use self::walkdir::WalkDir;

use crate::{op, Error, RawEvent, RecursiveMode, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::iter::Iterator;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    Add,
    Remove,
    RenameFrom,
}

fn event_action(ev: &RawEvent) -> Option<(Action, &Path)> {
    let path = ev.path.as_ref()?;
    let op = ev.op.as_ref().ok()?;
    if op.contains(op::REMOVE) {
        Some((Action::Remove, path))
    } else {
        let is_dir = fs::metadata(path).ok().map(|m| m.is_dir());
        if op.contains(op::CREATE) {
            if is_dir == Some(true) {
                Some((Action::Add, path))
            } else {
                None
            }
        } else if op.contains(op::RENAME) {
            if is_dir.is_none() {
                Some((Action::RenameFrom, path))
            } else if is_dir == Some(true) {
                Some((Action::Add, path))
            } else {
                // if it's a file we shouldn't do anything for renames
                None
            }
        } else {
            None
        }
    }
}

fn new_create(path: impl Into<PathBuf>) -> RawEvent {
    RawEvent {
        path: Some(path.into()),
        op: Ok(op::CREATE),
        cookie: None,
    }
}

// TODO return Result
pub trait WatcherInternal {
    /// If it returns `false` it means it's not natively supported
    fn add_recursive_watch(&mut self, dir: &Path) -> bool;
    fn add_non_recursive_watch(&mut self, dir: &Path, is_root: bool);
    fn remove_non_recursive_watch(&mut self, dir: &Path);
    fn send(&mut self, event: RawEvent);
}

/// A watch added by the user
/// Should contain the root path in `paths`
struct RootWatch {
    mode: RecursiveMode,
    is_native_recursive: bool,
    paths: HashSet<PathBuf>,
}

impl RootWatch {
    fn add_watch(
        &mut self,
        dir: &Path,
        watcher: &mut impl WatcherInternal,
        is_root: bool,
        emit_for_contents: bool,
    ) {
        match &self.mode {
            RecursiveMode::Filtered(filter) => {
                self.paths.insert(dir.to_path_buf());
                watcher.add_non_recursive_watch(dir, is_root);
                for e in WalkDir::new(dir)
                    .min_depth(1)
                    .follow_links(filter.follow_links)
                    .into_iter()
                    .filter_entry(|e| {
                        if e.file_type().is_dir() {
                            (filter.filter_dir)(e.path())
                        } else {
                            true
                        }
                    })
                    .filter_map(|e| e.ok())
                {
                    if e.file_type().is_dir() {
                        self.paths.insert(e.path().to_path_buf());
                        watcher.add_non_recursive_watch(e.path(), false);
                    }
                    if emit_for_contents {
                        watcher.send(new_create(e.into_path()));
                    }
                }
            }
            RecursiveMode::Recursive => {
                if !self.is_native_recursive {
                    // simulate it
                    self.paths.insert(dir.to_path_buf());
                    watcher.add_non_recursive_watch(dir, is_root);
                    for e in WalkDir::new(dir)
                        .min_depth(1)
                        .into_iter()
                        .filter_map(|e| e.ok())
                    {
                        if e.file_type().is_dir() {
                            self.paths.insert(e.path().to_path_buf());
                            watcher.add_non_recursive_watch(e.path(), false);
                        }
                        if emit_for_contents {
                            watcher.send(new_create(e.into_path()));
                        }
                    }
                }
            }
            RecursiveMode::NonRecursive => {
                if is_root {
                    self.paths.insert(dir.to_path_buf());
                    watcher.add_non_recursive_watch(dir, is_root);
                }
            }
        }
    }

    fn remove_watch(&mut self, dir: &Path, watcher: &mut impl WatcherInternal) {
        let mut remove_list = Vec::new();
        for path in &self.paths {
            if path.starts_with(dir) {
                remove_list.push(path.to_path_buf());
            }
        }
        for path in remove_list {
            watcher.remove_non_recursive_watch(&path);
            self.paths.remove(&path);
        }
    }

    fn remove_all(self, watcher: &mut impl WatcherInternal) {
        for path in &self.paths {
            watcher.remove_non_recursive_watch(&path);
        }
    }
}

pub struct RecursionAdapter {
    roots: HashMap<PathBuf, RootWatch>,
}

impl RecursionAdapter {
    pub fn new() -> RecursionAdapter {
        RecursionAdapter {
            roots: HashMap::new(),
        }
    }

    pub fn handle_event(&mut self, ev: RawEvent, watcher: &mut impl WatcherInternal) {
        if let Some((action, dir)) = event_action(&ev) {
            // special case for root
            if self.roots.contains_key(dir) {
                // if it's a `Action::RenameFrom` we still want to watch it
                // and `Action::Create` shouldn't happen
                if action == Action::Remove {
                    // ok to unwrap because of `contains_key` check
                    let root_watch = self.roots.remove(dir).unwrap();
                    root_watch.remove_all(watcher);
                }
            } else {
                // find containing root
                if let Some(root_watch) = self.find_root(dir) {
                    match action {
                        Action::Add => {
                            root_watch.add_watch(dir, watcher, false, false); // TODO change emit_for_contents to true
                        }
                        Action::Remove | Action::RenameFrom => {
                            // If it's `Action::RenameFrom` remove watch
                            // because it could have moved outside the root or not match the filter anymore
                            // If we should still be watching it we will receive another event with `Action::Add`
                            root_watch.remove_watch(dir, watcher);
                        }
                    }
                }
            }
        }

        watcher.send(ev);
    }

    pub fn add_root(
        &mut self,
        path: PathBuf,
        mode: RecursiveMode,
        watcher: &mut impl WatcherInternal,
    ) -> Result<()> {
        // TODO what if `path` already exists with a different `RecursiveMode`
        if self.roots.contains_key(&path) {
            return Ok(());
        }

        // ensure it exists
        let _ = fs::metadata(&path).map_err(Error::Io)?;

        let is_recursive = if let RecursiveMode::Recursive = mode {
            true
        } else {
            false
        };

        let mut root_watch = RootWatch {
            mode,
            is_native_recursive: false,
            paths: HashSet::new(),
        };

        if is_recursive && watcher.add_recursive_watch(&path) {
            root_watch.paths.insert(path.clone());
            root_watch.is_native_recursive = true;
        } else {
            root_watch.add_watch(&path, watcher, true, false);
        }

        self.roots.insert(path, root_watch);

        Ok(())
    }

    pub fn remove_root(&mut self, path: &Path, watcher: &mut impl WatcherInternal) -> Result<()> {
        if let Some(root_watch) = self.roots.remove(path) {
            if root_watch.paths.is_empty() {
                // it should contain at least the root path
                // otherwise it means that the root directory was removed
                Err(Error::WatchNotFound)
            } else {
                root_watch.remove_all(watcher);
                Ok(())
            }
        } else {
            Err(Error::WatchNotFound)
        }
    }

    pub fn remove_all(&mut self, watcher: &mut impl WatcherInternal) {
        for (_p, r) in self.roots.drain() {
            r.remove_all(watcher);
        }
    }

    fn find_root(&mut self, path: &Path) -> Option<&mut RootWatch> {
        let mut parent = path.parent();

        // workaround for "cannot borrow `self.roots` as mutable more than once at a time"
        while let Some(path) = parent {
            if self.roots.contains_key(path) {
                break;
            }

            parent = path.parent();
        }

        let parent = parent?;
        self.roots.get_mut(parent)
    }
}
