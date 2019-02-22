extern crate walkdir;

use self::walkdir::WalkDir;

use crate::{debounce::EventTx, op, Error, FilterItem, RawEvent, RecursiveMode, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::iter::Iterator;
use std::path::{Path, PathBuf};

struct EventInfo {
    path: PathBuf,
    file_type: Option<fs::FileType>,
    action: Action,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
enum Action {
    Add,
    Remove,
    RenameFrom,
    RenameTo,
    Other,
}

fn event_info(ev: &RawEvent, file_type: Option<io::Result<fs::FileType>>) -> Option<EventInfo> {
    let path = ev.path.clone()?;
    let op = match ev.op.as_ref() {
        Ok(op) => op,
        Err(_) => {
            let file_type = file_type
                .unwrap_or_else(|| fs::metadata(&path).map(|x| x.file_type()))
                .ok();
            return Some(EventInfo {
                path,
                action: Action::Other,
                file_type,
            });
        }
    };
    if op.contains(op::REMOVE) {
        Some(EventInfo {
            path,
            action: Action::Remove,
            file_type: None,
        })
    } else {
        let file_type = file_type
            .unwrap_or_else(|| fs::metadata(&path).map(|x| x.file_type()))
            .ok();
        if op.contains(op::CREATE) {
            Some(EventInfo {
                path,
                action: Action::Add,
                file_type,
            })
        } else if op.contains(op::RENAME) {
            let action = if file_type.is_some() {
                Action::RenameTo
            } else {
                Action::RenameFrom
            };
            Some(EventInfo {
                path,
                action,
                file_type,
            })
        } else {
            None
        }
    }
}

fn new_create_event(path: impl Into<PathBuf>) -> RawEvent {
    RawEvent {
        path: Some(path.into()),
        op: Ok(op::CREATE),
        cookie: None,
    }
}

pub(crate) trait WatcherInternal {
    /// If it returns `Ok(false)` it means it's not natively supported
    fn add_recursive_watch(&mut self, dir: &Path) -> Result<bool>;
    fn add_non_recursive_watch(&mut self, dir: &Path, is_root: bool) -> Result<()>;
    fn remove_non_recursive_watch(&mut self, dir: &Path) -> Result<()>;
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
        is_root: bool,
        watcher: &mut impl WatcherInternal,
        mut event_tx: Option<&mut EventTx>,
    ) -> Result<()> {
        match &self.mode {
            RecursiveMode::Filtered(filter) => {
                watcher.add_non_recursive_watch(dir, is_root)?;
                self.paths.insert(dir.to_path_buf());
                for e in WalkDir::new(dir)
                    .min_depth(1)
                    .follow_links(filter.follow_links)
                    .into_iter()
                    .filter_entry(|e| (filter.filter)(e.into()))
                    .filter_map(|e| e.ok())
                {
                    watcher.add_non_recursive_watch(e.path(), false)?;
                    self.paths.insert(e.path().to_path_buf());
                    if let Some(event_tx) = event_tx.as_mut() {
                        event_tx.send(new_create_event(e.path()))
                    }
                }
            }
            RecursiveMode::Recursive => {
                if !self.is_native_recursive {
                    // simulate it
                    watcher.add_non_recursive_watch(dir, is_root)?;
                    self.paths.insert(dir.to_path_buf());
                    for e in WalkDir::new(dir)
                        .min_depth(1)
                        .into_iter()
                        .filter_entry(|e| e.file_type().is_dir())
                        .filter_map(|e| e.ok())
                    {
                        watcher.add_non_recursive_watch(e.path(), false)?;
                        self.paths.insert(e.path().to_path_buf());
                        if let Some(event_tx) = event_tx.as_mut() {
                            event_tx.send(new_create_event(e.path()))
                        }
                    }
                }
            }
            RecursiveMode::NonRecursive => {
                if is_root {
                    self.paths.insert(dir.to_path_buf());
                    watcher.add_non_recursive_watch(dir, is_root)?;
                }
            }
        }
        Ok(())
    }

    fn remove_watch(&mut self, dir: &Path, watcher: &mut impl WatcherInternal) -> Result<()> {
        let mut remove_list = Vec::new();
        for path in &self.paths {
            if path.starts_with(dir) {
                remove_list.push(path.to_path_buf());
            }
        }
        for path in remove_list {
            watcher.remove_non_recursive_watch(&path)?;
            self.paths.remove(&path);
        }
        Ok(())
    }

    fn remove_all(self, watcher: &mut impl WatcherInternal) -> Result<()> {
        for path in &self.paths {
            watcher.remove_non_recursive_watch(&path)?;
        }
        Ok(())
    }
}

pub(crate) struct RecursionAdapter {
    roots: HashMap<PathBuf, RootWatch>,
}

impl RecursionAdapter {
    pub fn new() -> RecursionAdapter {
        RecursionAdapter {
            roots: HashMap::new(),
        }
    }

    pub fn handle_event(
        &mut self,
        ev: RawEvent,
        watcher: &mut impl WatcherInternal,
        event_tx: &mut EventTx,
    ) {
        self.handle_event_inner(ev, None, watcher, event_tx)
    }

    pub fn handle_event_with_file_type(
        &mut self,
        ev: RawEvent,
        file_type: io::Result<fs::FileType>,
        watcher: &mut impl WatcherInternal,
        event_tx: &mut EventTx,
    ) {
        self.handle_event_inner(ev, Some(file_type), watcher, event_tx)
    }

    fn handle_event_inner(
        &mut self,
        ev: RawEvent,
        file_type: Option<io::Result<fs::FileType>>,
        watcher: &mut impl WatcherInternal,
        event_tx: &mut EventTx,
    ) {
        let info = match event_info(&ev, file_type) {
            Some(info) => info,
            None => {
                event_tx.send(ev);
                return;
            }
        };
        // special case for root
        if self.roots.contains_key(&info.path) {
            event_tx.send(ev);
            // if it's a rename we still want to watch it
            if info.action == Action::Remove {
                // ok to unwrap because of `contains_key` check
                let root_watch = self.roots.remove(&info.path).unwrap();
                if let Err(_e) = root_watch.remove_all(watcher) {
                    // TODO log error?
                }
            }
        } else {
            // find containing root
            if let Some(root_watch) = self.find_root(&info.path) {
                // check if we should ignore the event itself
                if let RecursiveMode::Filtered(filter) = &root_watch.mode {
                    if !(filter.filter)(FilterItem {
                        path: &info.path,
                        file_type: info.file_type,
                    }) {
                        return;
                    }
                }
                // send before, since `add_watch` could also send events for nested paths
                event_tx.send(ev);
                let is_dir = info.file_type.map_or(false, |f| f.is_dir());
                match info.action {
                    Action::Add if is_dir => {
                        // it it's an add we can be sure that we haven't emited events
                        // for the contents, so pass the event_tx
                        if let Err(_e) =
                            root_watch.add_watch(&info.path, false, watcher, Some(event_tx))
                        {
                            // TODO log error?
                        }
                    }
                    Action::RenameTo if is_dir => {
                        // if it's a rename, we cannot know if we have already emited events
                        // for the contents, do not pass the event_tx
                        if let Err(_e) = root_watch.add_watch(&info.path, false, watcher, None) {
                            // TODO log error?
                        }
                    }
                    Action::Remove | Action::RenameFrom => {
                        // If it's `Action::RenameFrom` remove watch
                        // because it could have moved outside the root or not match the filter anymore
                        // If we should still be watching it we will receive another event with `Action::Add`
                        // or `Action::RenameTo`
                        if let Err(_e) = root_watch.remove_watch(&info.path, watcher) {
                            // TODO log error?
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn add_root(
        &mut self,
        path: PathBuf,
        mode: RecursiveMode,
        watcher: &mut impl WatcherInternal,
    ) -> Result<()> {
        self.add_root_inner(path, mode, watcher, None)
    }

    fn add_root_inner(
        &mut self,
        path: PathBuf,
        mode: RecursiveMode,
        watcher: &mut impl WatcherInternal,
        event_tx: Option<&mut EventTx>,
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

        if is_recursive && watcher.add_recursive_watch(&path)? {
            root_watch.paths.insert(path.clone());
            root_watch.is_native_recursive = true;
        } else {
            root_watch.add_watch(&path, true, watcher, event_tx)?;
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
                root_watch.remove_all(watcher)?;
                Ok(())
            }
        } else {
            Err(Error::WatchNotFound)
        }
    }

    pub fn remove_all(&mut self, watcher: &mut impl WatcherInternal) -> Result<()> {
        for (_p, r) in self.roots.drain() {
            r.remove_all(watcher)?;
        }
        Ok(())
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
