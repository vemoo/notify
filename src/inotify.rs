//! Watcher implementation for the inotify Linux API
//!
//! The inotify API provides a mechanism for monitoring filesystem events.  Inotify can be used to
//! monitor individual files, or to monitor directories.  When a directory is monitored, inotify
//! will return events for the directory itself, and for files inside the directory.

extern crate inotify as inotify_sys;
extern crate libc;
extern crate walkdir;

use self::inotify_sys::{EventMask, Inotify, WatchDescriptor, WatchMask};
use super::debounce::{Debounce, EventTx};
use super::recursion::{RecursionAdapter, WatcherInternal};
use super::{op, DebouncedEvent, Error, Op, RawEvent, RecursiveMode, Result, Watcher};
use mio;
use mio_extras;
use std::collections::HashMap;
use std::env;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

const INOTIFY: mio::Token = mio::Token(0);
const MESSAGE: mio::Token = mio::Token(1);

// The EventLoop will set up a mio::Poll and use it to wait for the following:
//
// -  messages telling it what to do
//
// -  events telling it that something has happened on one of the watched files.
struct EventLoop {
    running: bool,
    poll: mio::Poll,
    event_loop_tx: mio_extras::channel::Sender<EventLoopMsg>,
    event_loop_rx: mio_extras::channel::Receiver<EventLoopMsg>,
    inotify: Option<Inotify>,
    event_tx: EventTx,
    watches: HashMap<PathBuf, (WatchDescriptor, WatchMask, bool)>,
    paths: HashMap<WatchDescriptor, PathBuf>,
    rename_event: Option<RawEvent>,
}

impl WatcherInternal for EventLoop {
    fn add_recursive_watch(&mut self, _dir: &Path) -> bool {
        // not supported
        false
    }
    fn add_non_recursive_watch(&mut self, dir: &Path, is_root: bool) {
        let _ = self.add_single_watch(dir.to_path_buf(), false, is_root);
    }
    fn remove_non_recursive_watch(&mut self, dir: &Path) {
        let _ = self.remove_watch(dir.to_path_buf(), false);
    }
    fn send(&mut self, event: RawEvent) {
        self.event_tx.send(event)
    }
}

/// Watcher implementation based on inotify
pub struct INotifyWatcher(Mutex<mio_extras::channel::Sender<EventLoopMsg>>);

enum EventLoopMsg {
    AddWatch(PathBuf, RecursiveMode, Sender<Result<()>>),
    RemoveWatch(PathBuf, Sender<Result<()>>),
    Shutdown,
    RenameTimeout(u32),
}

#[inline]
fn check_pending_rename_event(rename_event: &mut Option<RawEvent>) -> Option<RawEvent> {
    mem::replace(rename_event, None).map(|e| RawEvent {
        path: e.path,
        op: Ok(op::Op::REMOVE),
        cookie: None,
    })
}

impl EventLoop {
    pub fn new(inotify: Inotify, event_tx: EventTx) -> Result<EventLoop> {
        let (event_loop_tx, event_loop_rx) = mio_extras::channel::channel::<EventLoopMsg>();
        let poll = mio::Poll::new()?;
        poll.register(
            &event_loop_rx,
            MESSAGE,
            mio::Ready::readable(),
            mio::PollOpt::edge(),
        )?;

        let inotify_fd = inotify.as_raw_fd();
        let evented_inotify = mio::unix::EventedFd(&inotify_fd);
        poll.register(
            &evented_inotify,
            INOTIFY,
            mio::Ready::readable(),
            mio::PollOpt::edge(),
        )?;

        let event_loop = EventLoop {
            running: true,
            poll,
            event_loop_tx,
            event_loop_rx,
            inotify: Some(inotify),
            event_tx,
            watches: HashMap::new(),
            paths: HashMap::new(),
            rename_event: None,
        };
        Ok(event_loop)
    }

    fn channel(&self) -> mio_extras::channel::Sender<EventLoopMsg> {
        self.event_loop_tx.clone()
    }

    fn add_single_watch(
        &mut self,
        path: PathBuf,
        is_recursive: bool,
        watch_self: bool,
    ) -> Result<()> {
        let mut watchmask = WatchMask::ATTRIB
            | WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::CLOSE_WRITE
            | WatchMask::MODIFY
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO;

        if watch_self {
            watchmask.insert(WatchMask::DELETE_SELF);
            watchmask.insert(WatchMask::MOVE_SELF);
        }

        if let Some(&(_, old_watchmask, _)) = self.watches.get(&path) {
            watchmask.insert(old_watchmask);
            watchmask.insert(WatchMask::MASK_ADD);
        }

        if let Some(ref mut inotify) = self.inotify {
            match inotify.add_watch(&path, watchmask) {
                Err(e) => Err(Error::Io(e)),
                Ok(w) => {
                    watchmask.remove(WatchMask::MASK_ADD);
                    self.watches
                        .insert(path.clone(), (w.clone(), watchmask, is_recursive));
                    self.paths.insert(w, path);
                    Ok(())
                }
            }
        } else {
            Ok(())
        }
    }

    fn remove_watch(&mut self, path: PathBuf, remove_recursive: bool) -> Result<()> {
        match self.watches.remove(&path) {
            None => return Err(Error::WatchNotFound),
            Some((w, _, is_recursive)) => {
                if let Some(ref mut inotify) = self.inotify {
                    try!(inotify.rm_watch(w.clone()).map_err(Error::Io));
                    self.paths.remove(&w);

                    if is_recursive || remove_recursive {
                        let mut remove_list = Vec::new();
                        for (w, p) in &self.paths {
                            if p.starts_with(&path) {
                                try!(inotify.rm_watch(w.clone()).map_err(Error::Io));
                                self.watches.remove(p);
                                remove_list.push(w.clone());
                            }
                        }
                        for w in remove_list {
                            self.paths.remove(&w);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

struct EventLoopWrapper {
    event_loop: EventLoop,
    recursion_adapter: RecursionAdapter,
}

impl EventLoopWrapper {
    pub fn new(inotify: Inotify, event_tx: EventTx) -> Result<EventLoopWrapper> {
        let event_loop = EventLoop::new(inotify, event_tx)?;
        Ok(EventLoopWrapper {
            event_loop,
            recursion_adapter: RecursionAdapter::new(),
        })
    }

    // Run the event loop.
    pub fn run(self) {
        thread::spawn(|| self.event_loop_thread());
    }

    fn event_loop_thread(mut self) {
        let mut events = mio::Events::with_capacity(16);
        loop {
            // Wait for something to happen.
            self.event_loop
                .poll
                .poll(&mut events, None)
                .expect("poll failed");

            // Process whatever happened.
            for event in &events {
                self.handle_event(&event);
            }

            // Stop, if we're done.
            if !self.event_loop.running {
                break;
            }
        }
    }

    // Handle a single event.
    fn handle_event(&mut self, event: &mio::Event) {
        match event.token() {
            MESSAGE => {
                // The channel is readable - handle messages.
                self.handle_messages()
            }
            INOTIFY => {
                // inotify has something to tell us.
                self.handle_inotify()
            }
            _ => unreachable!(),
        }
    }

    fn handle_messages(&mut self) {
        while let Ok(msg) = self.event_loop.event_loop_rx.try_recv() {
            match msg {
                EventLoopMsg::AddWatch(path, recursive_mode, tx) => {
                    let _ = tx.send(self.recursion_adapter.add_root(
                        path,
                        recursive_mode,
                        &mut self.event_loop,
                    ));
                }
                EventLoopMsg::RemoveWatch(path, tx) => {
                    let _ = tx.send(
                        self.recursion_adapter
                            .remove_root(&path, &mut self.event_loop),
                    );
                }
                EventLoopMsg::Shutdown => {
                    self.recursion_adapter.remove_all(&mut self.event_loop);
                    if let Some(inotify) = self.event_loop.inotify.take() {
                        let _ = inotify.close();
                    }
                    self.event_loop.running = false;
                    break;
                }
                EventLoopMsg::RenameTimeout(cookie) => {
                    let current_cookie =
                        self.event_loop.rename_event.as_ref().and_then(|e| e.cookie);
                    // send pending rename event only if the rename event for which the timer has been created hasn't been handled already; otherwise ignore this timeout
                    if current_cookie == Some(cookie) {
                        if let Some(ev) =
                            check_pending_rename_event(&mut self.event_loop.rename_event)
                        {
                            self.recursion_adapter
                                .handle_event(ev, &mut self.event_loop);
                        }
                    }
                }
            }
        }
    }

    fn handle_inotify(&mut self) {
        let mut acc_events = Vec::new();

        if let Some(ref mut inotify) = self.event_loop.inotify {
            let mut buffer = [0; 1024];
            match inotify.read_events(&mut buffer) {
                Ok(events) => {
                    for event in events {
                        if event.mask.contains(EventMask::Q_OVERFLOW) {
                            acc_events.push(RawEvent {
                                path: None,
                                op: Ok(op::Op::RESCAN),
                                cookie: None,
                            });
                        }

                        let path = match event.name {
                            Some(name) => self
                                .event_loop
                                .paths
                                .get(&event.wd)
                                .map(|root| root.join(&name)),
                            None => self.event_loop.paths.get(&event.wd).cloned(),
                        };

                        if event.mask.contains(EventMask::MOVED_FROM) {
                            if let Some(ev) =
                                check_pending_rename_event(&mut self.event_loop.rename_event)
                            {
                                acc_events.push(ev);
                            }
                            self.event_loop.rename_event = Some(RawEvent {
                                path: path,
                                op: Ok(op::Op::RENAME),
                                cookie: Some(event.cookie),
                            });
                        } else {
                            let mut o = Op::empty();
                            let mut c = None;
                            if event.mask.contains(EventMask::MOVED_TO) {
                                let rename_event =
                                    mem::replace(&mut self.event_loop.rename_event, None);
                                if let Some(e) = rename_event {
                                    if e.cookie == Some(event.cookie) {
                                        acc_events.push(e);
                                        o.insert(op::Op::RENAME);
                                        c = Some(event.cookie);
                                    } else {
                                        o.insert(op::Op::CREATE);
                                    }
                                } else {
                                    o.insert(op::Op::CREATE);
                                }
                            }
                            if event.mask.contains(EventMask::MOVE_SELF) {
                                o.insert(op::Op::RENAME);
                            }
                            if event.mask.contains(EventMask::CREATE) {
                                o.insert(op::Op::CREATE);
                            }
                            if event.mask.contains(EventMask::DELETE_SELF)
                                || event.mask.contains(EventMask::DELETE)
                            {
                                o.insert(op::Op::REMOVE);
                            }
                            if event.mask.contains(EventMask::MODIFY) {
                                o.insert(op::Op::WRITE);
                            }
                            if event.mask.contains(EventMask::CLOSE_WRITE) {
                                o.insert(op::Op::CLOSE_WRITE);
                            }
                            if event.mask.contains(EventMask::ATTRIB) {
                                o.insert(op::Op::CHMOD);
                            }

                            if !o.is_empty() {
                                if let Some(event) =
                                    check_pending_rename_event(&mut self.event_loop.rename_event)
                                {
                                    acc_events.push(event);
                                }

                                acc_events.push(RawEvent {
                                    path: path,
                                    op: Ok(o),
                                    cookie: c,
                                });
                            }
                        }
                    }

                    // When receiving only the first part of a move event (IN_MOVED_FROM) it is unclear
                    // whether the second part (IN_MOVED_TO) will arrive because the file or directory
                    // could just have been moved out of the watched directory. So it's necessary to wait
                    // for possible subsequent events in case it's a complete move event but also to make sure
                    // that the first part of the event is handled in a timely manner in case no subsequent events arrive.
                    if let Some(ref rename_event) = self.event_loop.rename_event {
                        let event_loop_tx = self.event_loop.event_loop_tx.clone();
                        let cookie = rename_event.cookie.unwrap(); // unwrap is safe because rename_event is always set with some cookie
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(10)); // wait up to 10 ms for a subsequent event
                            event_loop_tx
                                .send(EventLoopMsg::RenameTimeout(cookie))
                                .unwrap();
                        });
                    }
                }
                Err(e) => {
                    acc_events.push(RawEvent {
                        path: None,
                        op: Err(Error::Io(e)),
                        cookie: None,
                    });
                }
            }
        }

        for ev in acc_events {
            self.recursion_adapter
                .handle_event(ev, &mut self.event_loop);
        }
    }
}

impl Watcher for INotifyWatcher {
    fn new_raw(tx: Sender<RawEvent>) -> Result<INotifyWatcher> {
        let inotify = Inotify::init()?;
        let event_tx = EventTx::Raw { tx };
        let event_loop = EventLoopWrapper::new(inotify, event_tx)?;
        let channel = event_loop.event_loop.channel();
        event_loop.run();
        Ok(INotifyWatcher(Mutex::new(channel)))
    }

    fn new(tx: Sender<DebouncedEvent>, delay: Duration) -> Result<INotifyWatcher> {
        let inotify = Inotify::init()?;
        let event_tx = EventTx::Debounced {
            tx: tx.clone(),
            debounce: Debounce::new(delay, tx),
        };
        let event_loop = EventLoopWrapper::new(inotify, event_tx)?;
        let channel = event_loop.event_loop.channel();
        event_loop.run();
        Ok(INotifyWatcher(Mutex::new(channel)))
    }

    fn watch<P: AsRef<Path>>(&mut self, path: P, recursive_mode: RecursiveMode) -> Result<()> {
        let pb = if path.as_ref().is_absolute() {
            path.as_ref().to_owned()
        } else {
            let p = try!(env::current_dir().map_err(Error::Io));
            p.join(path)
        };
        let (tx, rx) = mpsc::channel();
        let msg = EventLoopMsg::AddWatch(pb, recursive_mode, tx);

        // we expect the event loop to live and reply => unwraps must not panic
        self.0.lock().unwrap().send(msg).unwrap();
        rx.recv().unwrap()
    }

    fn unwatch<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let pb = if path.as_ref().is_absolute() {
            path.as_ref().to_owned()
        } else {
            let p = try!(env::current_dir().map_err(Error::Io));
            p.join(path)
        };
        let (tx, rx) = mpsc::channel();
        let msg = EventLoopMsg::RemoveWatch(pb, tx);

        // we expect the event loop to live and reply => unwraps must not panic
        self.0.lock().unwrap().send(msg).unwrap();
        rx.recv().unwrap()
    }
}

impl Drop for INotifyWatcher {
    fn drop(&mut self) {
        // we expect the event loop to live => unwrap must not panic
        self.0.lock().unwrap().send(EventLoopMsg::Shutdown).unwrap();
    }
}
