extern crate walkdir;

use self::walkdir::{DirEntry, WalkDir};
use crate::{op, DebouncedEvent, RawEvent, Watcher};
use std::collections::{HashMap, HashSet};
use std::fs::FileType;
use std::iter::Iterator;
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

trait Event {
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

trait WatcherInternal<E: Event> {
    fn add_single_watch(&mut self, path: &Path);
    fn remove_single_watch(&mut self, path: &Path);
    fn send(&self, event: E);
}

struct NestedWatches {
    mode: RecursiveMode,
    paths: HashSet<PathBuf>,
}

struct RecursionAdapter {
    roots: HashMap<PathBuf, NestedWatches>,
}

impl RecursionAdapter {
    pub fn handle_event<E: Event>(&mut self, ev: E, watcher: &mut impl WatcherInternal<E>) {
        let dir = match ev.is_dir_create() {
            None => return,
            Some(dir) => dir,
        };
        let nested = match self.find_root(&dir) {
            None => {
                // error?
                return;
            }
            Some(x) => x,
        };
        match &nested.mode {
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
                        nested.paths.insert(e.path().to_path_buf());
                        watcher.add_single_watch(e.path());
                    }

                    watcher.send(E::new_create(e.into_path()));
                }
            }
            RecursiveMode::Recursive => {
                // TODO do nothing if native recursion is supported
                for e in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
                    if e.file_type().is_dir() {
                        nested.paths.insert(e.path().to_path_buf());
                        watcher.add_single_watch(e.path());
                    }

                    watcher.send(E::new_create(e.into_path()));
                }
            }
            RecursiveMode::NonRecursive => {}
        }
    }

    pub fn add_root(&mut self, path: PathBuf, mode: RecursiveMode) {
        // TODO what if `path` already exists with a different `RecursiveMode`
        self.roots.entry(path).or_insert_with(|| NestedWatches {
            mode,
            paths: HashSet::new(),
        });
    }

    pub fn remove_root(&mut self, path: &Path) -> HashSet<PathBuf> {
        self.roots.remove(path).map(|x| x.paths).unwrap_or_default()
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
