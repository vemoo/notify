extern crate walkdir;

use self::walkdir::WalkDir;

use crate::{op, Error, RawEvent, RecursiveMode, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::iter::Iterator;
use std::path::{Path, PathBuf};

enum Action {
    Watch,
    Unwatch,
}

fn event_action(ev: &RawEvent) -> Option<(Action, &Path)> {
    let path = ev.path.as_ref()?;
    let op = ev.op.as_ref().ok()?;
    if op.contains(op::REMOVE) {
        Some((Action::Unwatch, path))
    } else {
        let is_dir = fs::metadata(path).ok().map(|m| m.is_dir());
        if op.contains(op::CREATE) {
            if is_dir == Some(true) {
                Some((Action::Watch, path))
            } else {
                None
            }
        } else if op.contains(op::RENAME) {
            if is_dir.is_none() {
                Some((Action::Unwatch, path))
            } else if is_dir == Some(true) {
                Some((Action::Watch, path))
            } else {
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
    /// If it returns `None` it means it's not supported (natively)
    fn add_recursive_watch(&mut self, dir: &Path) -> Option<()>;
    fn add_non_recursive_watch(&mut self, dir: &Path, is_root: bool);
    fn remove_non_recursive_watch(&mut self, dir: &Path);
    fn send(&mut self, event: RawEvent);
}

struct RootWatch {
    mode: RecursiveMode,
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
                if watcher.add_recursive_watch(dir).is_some() {
                    // is supported
                } else {
                    // simulate it
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
            RecursiveMode::NonRecursive => {}
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
            if let Some(nested) = self.find_root(dir) {
                match action {
                    Action::Watch => {
                        nested.add_watch(dir, watcher, false, false); // TODO change emit_for_contents to true
                    }
                    Action::Unwatch => {
                        // println!("UNWATCH: {:?}", ev);
                        nested.remove_watch(dir, watcher);
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

        let _ = fs::metadata(&path).map_err(Error::Io)?;

        let mut nested = RootWatch {
            mode,
            paths: HashSet::new(),
        };
        nested.add_watch(&path, watcher, true, false);

        self.roots.insert(path, nested);

        Ok(())
    }

    pub fn remove_root(&mut self, path: &Path, watcher: &mut impl WatcherInternal) -> Result<()> {
        if let Some(nested) = self.roots.remove(path) {
            for path in nested.paths.iter() {
                watcher.remove_non_recursive_watch(path);
            }
            Ok(())
        } else {
            Err(Error::WatchNotFound)
        }
    }

    pub fn remove_all(&mut self, watcher: &mut impl WatcherInternal) {
        for (_p, r) in self.roots.drain() {
            for path in r.paths.iter() {
                watcher.remove_non_recursive_watch(path);
            }
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
