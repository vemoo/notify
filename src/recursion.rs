extern crate walkdir;

use self::walkdir::{DirEntry, WalkDir};
use crate::{op, DebouncedEvent, RawEvent, Watcher};
use std::collections::{HashMap, HashSet};
use std::fs::FileType;
use std::iter::Iterator;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

pub enum RecursiveMode {
    Recursive,
    NonRecursive,
    Filtered(RecursionFilter),
}

struct RecursionFilter {
    filter_dir: Box<Fn(&Path) -> bool>,
    follow_links: bool,
}

pub trait Event {
    fn new_create(path: impl Into<PathBuf>) -> Self;
    fn is_dir_create(&self) -> Option<&Path>;
}

impl Event for DebouncedEvent {
    fn new_create(path: impl Into<PathBuf>) -> Self {
        DebouncedEvent::Create(path.into())
    }
    fn is_dir_create(&self) -> Option<&Path> {
        match self {
            DebouncedEvent::Create(path) if path.is_dir() => Some(path),
            _ => None,
        }
    }
}

impl Event for RawEvent {
    fn new_create(path: impl Into<PathBuf>) -> Self {
        RawEvent {
            path: Some(path.into()),
            op: Ok(op::CREATE),
            cookie: None,
        }
    }
    fn is_dir_create(&self) -> Option<&Path> {
        match self {
            RawEvent {
                op: Ok(op),
                path: Some(path),
                ..
            } if op.contains(op::CREATE) && path.is_dir() => Some(path),
            _ => None,
        }
    }
}

pub trait WatcherInternal<E: Event> {
    /// If it returns `None` it means it's not supported (natively)
    fn add_recursive_watch(&mut self, dir: &Path) -> Option<()>;
    fn add_non_recursive_watch(&mut self, dir: &Path);
    fn remove_non_recursive_watch(&mut self, dir: &Path);
    fn send(&self, event: E);
}

struct NestedWatches {
    mode: RecursiveMode,
    paths: HashSet<PathBuf>,
}

impl NestedWatches {
    fn add_watch<E: Event>(
        &mut self,
        dir: &Path,
        watcher: &mut impl WatcherInternal<E>,
        emit_for_contents: bool,
    ) {
        match &self.mode {
            RecursiveMode::Filtered(filter) => {
                for e in WalkDir::new(dir)
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
                        watcher.add_non_recursive_watch(e.path());
                    }
                    if emit_for_contents {
                        watcher.send(E::new_create(e.into_path()));
                    }
                }
            }
            RecursiveMode::Recursive => {
                if watcher.add_recursive_watch(dir).is_some() {
                    // is supported
                } else {
                    // simulate it
                    for e in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
                        if e.file_type().is_dir() {
                            self.paths.insert(e.path().to_path_buf());
                            watcher.add_non_recursive_watch(e.path());
                        }
                        if emit_for_contents {
                            watcher.send(E::new_create(e.into_path()));
                        }
                    }
                }
            }
            RecursiveMode::NonRecursive => {}
        }
    }
}

struct RecursionAdapter<E: Event> {
    roots: HashMap<PathBuf, NestedWatches>,
    _event_type: PhantomData<E>,
}

impl<E: Event> RecursionAdapter<E> {
    pub fn handle_event(&mut self, ev: E, watcher: &mut impl WatcherInternal<E>) {
        if let Some(dir) = ev.is_dir_create() {
            if let Some(nested) = self.find_root(dir) {
                nested.add_watch(dir, watcher, true);
            }
        }
    }

    pub fn add_root(
        &mut self,
        path: PathBuf,
        mode: RecursiveMode,
        watcher: &mut impl WatcherInternal<E>,
    ) {
        // TODO what if `path` already exists with a different `RecursiveMode`
        if self.roots.contains_key(&path) {
            return;
        }

        let mut nested = NestedWatches {
            mode,
            paths: HashSet::new(),
        };
        nested.add_watch(&path, watcher, false);

        self.roots.insert(path, nested);
    }

    pub fn remove_root(&mut self, path: &Path, watcher: &mut impl WatcherInternal<E>) {
        if let Some(nested) = self.roots.remove(path) {
            for path in nested.paths.iter() {
                watcher.remove_non_recursive_watch(path);
            }
        }
    }

    fn find_root(&mut self, path: &Path) -> Option<&mut NestedWatches> {
        let mut parent = Some(path);

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
