//! Generic Watcher implementation based on polling
//!
//! Checks the `watch`ed paths periodically to detect changes. This implementation only uses
//! Rust stdlib APIs and should work on all of the platforms it supports.

use self::walkdir::WalkDir;
use super::debounce::{Debounce, EventTx};
use super::{op, DebouncedEvent, Error, RawEvent, RecursiveMode, Result, Watcher};
use filetime::FileTime;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

extern crate walkdir;

struct PathData {
    mtime: i64,
    last_check: Instant,
}

struct WatchData {
    recursive_mode: RecursiveMode,
    paths: HashMap<PathBuf, PathData>,
}

/// Polling based `Watcher` implementation
pub struct PollWatcher {
    event_tx: EventTx,
    watches: Arc<Mutex<HashMap<PathBuf, WatchData>>>,
    open: Arc<RwLock<bool>>,
}

fn walkdir<'a>(
    dir: &Path,
    recursive_mode: &'a RecursiveMode,
) -> impl Iterator<Item = walkdir::DirEntry> + 'a {
    let max_depth;
    let follow_links;
    match recursive_mode {
        RecursiveMode::Filtered(filter) => {
            max_depth = usize::max_value();
            follow_links = filter.follow_links;
        }
        RecursiveMode::Recursive => {
            max_depth = usize::max_value();
            follow_links = true;
        }
        RecursiveMode::NonRecursive => {
            max_depth = 1;
            follow_links = true;
        }
    }

    WalkDir::new(dir)
        .follow_links(follow_links)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(move |e| match recursive_mode {
            RecursiveMode::Filtered(filter) => (filter.filter)(e.into()),
            RecursiveMode::Recursive | RecursiveMode::NonRecursive => true,
        })
        .filter_map(|e| e.ok())
}

impl PollWatcher {
    /// Create a PollWatcher which polls every `delay` milliseconds
    pub fn with_delay_ms(tx: Sender<RawEvent>, delay: u32) -> Result<PollWatcher> {
        let mut p = PollWatcher {
            event_tx: EventTx::Raw { tx: tx.clone() },
            watches: Arc::new(Mutex::new(HashMap::new())),
            open: Arc::new(RwLock::new(true)),
        };
        let event_tx = EventTx::Raw { tx: tx };
        p.run(Duration::from_millis(delay as u64), event_tx);
        Ok(p)
    }

    fn run(&mut self, delay: Duration, mut event_tx: EventTx) {
        let watches = self.watches.clone();
        let open = self.open.clone();

        thread::spawn(move || {
            // In order of priority:
            // TODO: handle chmod events
            // TODO: handle renames
            // TODO: DRY it up

            loop {
                if !(*open.read().unwrap()) {
                    break;
                }

                if let Ok(mut watches) = watches.lock() {
                    let current_time = Instant::now();

                    for (
                        watch,
                        &mut WatchData {
                            ref recursive_mode,
                            ref mut paths,
                        },
                    ) in watches.iter_mut()
                    {
                        match fs::metadata(watch) {
                            Err(e) => {
                                event_tx.send(RawEvent {
                                    path: Some(watch.clone()),
                                    op: Err(Error::Io(e)),
                                    cookie: None,
                                });
                                continue;
                            }
                            Ok(metadata) => {
                                if !metadata.is_dir() {
                                    let mtime =
                                        FileTime::from_last_modification_time(&metadata).seconds();
                                    match paths.insert(
                                        watch.clone(),
                                        PathData {
                                            mtime: mtime,
                                            last_check: current_time,
                                        },
                                    ) {
                                        None => {
                                            unreachable!();
                                        }
                                        Some(PathData {
                                            mtime: old_mtime, ..
                                        }) => {
                                            if mtime > old_mtime {
                                                event_tx.send(RawEvent {
                                                    path: Some(watch.clone()),
                                                    op: Ok(op::Op::WRITE),
                                                    cookie: None,
                                                });
                                            }
                                        }
                                    }
                                } else {
                                    for entry in walkdir(watch, recursive_mode) {
                                        let path = entry.path();

                                        match entry.metadata() {
                                            Err(e) => {
                                                event_tx.send(RawEvent {
                                                    path: Some(path.to_path_buf()),
                                                    op: Err(Error::Io(e.into())),
                                                    cookie: None,
                                                });
                                            }
                                            Ok(m) => {
                                                let mtime =
                                                    FileTime::from_last_modification_time(&m)
                                                        .seconds();
                                                match paths.insert(
                                                    path.to_path_buf(),
                                                    PathData {
                                                        mtime: mtime,
                                                        last_check: current_time,
                                                    },
                                                ) {
                                                    None => {
                                                        event_tx.send(RawEvent {
                                                            path: Some(path.to_path_buf()),
                                                            op: Ok(op::Op::CREATE),
                                                            cookie: None,
                                                        });
                                                    }
                                                    Some(PathData {
                                                        mtime: old_mtime, ..
                                                    }) => {
                                                        if mtime > old_mtime {
                                                            event_tx.send(RawEvent {
                                                                path: Some(path.to_path_buf()),
                                                                op: Ok(op::Op::WRITE),
                                                                cookie: None,
                                                            });
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    for (_, &mut WatchData { ref mut paths, .. }) in watches.iter_mut() {
                        let mut removed = Vec::new();
                        for (path, &PathData { last_check, .. }) in paths.iter() {
                            if last_check < current_time {
                                event_tx.send(RawEvent {
                                    path: Some(path.clone()),
                                    op: Ok(op::Op::REMOVE),
                                    cookie: None,
                                });
                                removed.push(path.clone());
                            }
                        }
                        for path in removed {
                            (*paths).remove(&path);
                        }
                    }
                }

                thread::sleep(delay);
            }
        });
    }
}

impl Watcher for PollWatcher {
    fn new_raw(tx: Sender<RawEvent>) -> Result<PollWatcher> {
        PollWatcher::with_delay_ms(tx, 30_000)
    }

    fn new(tx: Sender<DebouncedEvent>, delay: Duration) -> Result<PollWatcher> {
        let mut p = PollWatcher {
            event_tx: EventTx::DebouncedTx { tx: tx.clone() },
            watches: Arc::new(Mutex::new(HashMap::new())),
            open: Arc::new(RwLock::new(true)),
        };
        let event_tx = EventTx::Debounced {
            tx: tx.clone(),
            debounce: Debounce::new(delay, tx),
        };
        p.run(delay, event_tx);
        Ok(p)
    }

    fn watch<P: AsRef<Path>>(&mut self, path: P, recursive_mode: RecursiveMode) -> Result<()> {
        if let Ok(mut watches) = self.watches.lock() {
            let current_time = Instant::now();

            let watch = path.as_ref().to_owned();

            match fs::metadata(path) {
                Err(e) => {
                    self.event_tx.send(RawEvent {
                        path: Some(watch.clone()),
                        op: Err(Error::Io(e)),
                        cookie: None,
                    });
                }
                Ok(metadata) => {
                    if !metadata.is_dir() {
                        let mut paths = HashMap::new();
                        let mtime = FileTime::from_last_modification_time(&metadata).seconds();
                        paths.insert(
                            watch.clone(),
                            PathData {
                                mtime: mtime,
                                last_check: current_time,
                            },
                        );
                        watches.insert(
                            watch,
                            WatchData {
                                recursive_mode,
                                paths: paths,
                            },
                        );
                    } else {
                        let mut paths = HashMap::new();
                        for entry in walkdir(&watch, &recursive_mode) {
                            let path = entry.path();

                            match entry.metadata() {
                                Err(e) => {
                                    self.event_tx.send(RawEvent {
                                        path: Some(path.to_path_buf()),
                                        op: Err(Error::Io(e.into())),
                                        cookie: None,
                                    });
                                }
                                Ok(m) => {
                                    let mtime = FileTime::from_last_modification_time(&m).seconds();
                                    paths.insert(
                                        path.to_path_buf(),
                                        PathData {
                                            mtime: mtime,
                                            last_check: current_time,
                                        },
                                    );
                                }
                            }
                        }
                        watches.insert(
                            watch,
                            WatchData {
                                recursive_mode,
                                paths: paths,
                            },
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn unwatch<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        if (*self.watches)
            .lock()
            .unwrap()
            .remove(path.as_ref())
            .is_some()
        {
            Ok(())
        } else {
            Err(Error::WatchNotFound)
        }
    }
}

impl Drop for PollWatcher {
    fn drop(&mut self) {
        {
            let mut open = (*self.open).write().unwrap();
            (*open) = false;
        }
    }
}
