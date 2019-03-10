#![warn(missing_docs)]
//! Watcher implementation for Windows' directory management APIs
//!
//! For more information see the [ReadDirectoryChangesW reference][ref].
//!
//! [ref]: https://msdn.microsoft.com/en-us/library/windows/desktop/aa363950(v=vs.85).aspx

extern crate kernel32;

use winapi::shared::minwindef::TRUE;
use winapi::shared::winerror::ERROR_OPERATION_ABORTED;
use winapi::um::fileapi;
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::minwinbase::{LPOVERLAPPED, OVERLAPPED};
use winapi::um::winbase::{self, INFINITE, WAIT_OBJECT_0};
use winapi::um::winnt::{self, FILE_NOTIFY_INFORMATION, HANDLE};

use super::debounce::{Debounce, EventTx};
use super::recursion::{RecursionAdapter, RecursionFilter, WatcherInternal};
use super::{op, DebouncedEvent, Error, Op, RawEvent, RecursiveMode, Result, Watcher};
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::mem;
use std::os::raw::c_void;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const BUF_SIZE: u32 = 16384;

static mut COOKIE_COUNTER: u32 = 0;

#[derive(Clone)]
struct ReadData {
    dir: PathBuf, // directory that is being watched
    complete_sem: HANDLE,
    is_recursive: bool,
}

struct ReadDirectoryRequest {
    action_tx: Sender<Action>,
    buffer: [u8; BUF_SIZE as usize],
    handle: HANDLE,
    data: ReadData,
}

enum Action {
    Watch(PathBuf, RecursiveMode),
    Unwatch(PathBuf),
    Event(RawEvent),
    Stop,
}

pub enum MetaEvent {
    SingleWatchComplete,
    WatcherAwakened,
}

struct WatchState {
    dir_handle: HANDLE,
    complete_sem: HANDLE,
}

struct ReadDirectoryChangesServerInner {
    action_tx: Sender<Action>,
    meta_tx: Sender<MetaEvent>,
    file_watches: HashMap<PathBuf, PathBuf>,
    watches: HashMap<PathBuf, WatchState>,
}

impl ReadDirectoryChangesServerInner {
    fn add_watch(&mut self, path: PathBuf, is_recursive: bool) -> Result<()> {
        // path must exist and be a directory
        if !path.is_dir() {
            return Err(Error::Generic(
                "Input watch path is not a directory.".to_owned(),
            ));
        }

        let encoded_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle;
        unsafe {
            handle = kernel32::CreateFileW(
                encoded_path.as_ptr(),
                winnt::FILE_LIST_DIRECTORY,
                winnt::FILE_SHARE_READ | winnt::FILE_SHARE_DELETE | winnt::FILE_SHARE_WRITE,
                ptr::null_mut(),
                fileapi::OPEN_EXISTING,
                winbase::FILE_FLAG_BACKUP_SEMANTICS | winbase::FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            );

            if handle == INVALID_HANDLE_VALUE {
                // TODO: Call GetLastError for better error info?
                return Err(Error::PathNotFound);
            }
        }

        // every watcher gets its own semaphore to signal completion
        let semaphore =
            unsafe { kernel32::CreateSemaphoreW(ptr::null_mut(), 0, 1, ptr::null_mut()) };
        if semaphore == ptr::null_mut() || semaphore == INVALID_HANDLE_VALUE {
            unsafe {
                kernel32::CloseHandle(handle);
            }
            return Err(Error::Generic(
                "Failed to create semaphore for watch.".to_owned(),
            ));
        }
        let rd = ReadData {
            dir: path.to_path_buf(),
            complete_sem: semaphore,
            is_recursive: is_recursive,
        };
        let ws = WatchState {
            dir_handle: handle,
            complete_sem: semaphore,
        };
        self.watches.insert(path, ws);
        start_read(&rd, self.action_tx.clone(), handle);
        Ok(())
    }

    fn remove_watch(&mut self, path: PathBuf) {
        if let Some(ws) = self.watches.remove(&path) {
            stop_watch(&ws, &self.meta_tx);
        }
    }
}

impl WatcherInternal for ReadDirectoryChangesServerInner {
    fn add_recursive_watch(&mut self, dir: &Path) -> Result<bool> {
        self.add_watch(dir.to_path_buf(), true)?;
        Ok(true)
    }
    fn add_non_recursive_watch(&mut self, dir: &Path, _is_root: bool) -> Result<()> {
        self.add_watch(dir.to_path_buf(), false)?;
        Ok(())
    }
    fn remove_non_recursive_watch(&mut self, dir: &Path) -> Result<()> {
        self.remove_watch(dir.to_path_buf());
        Ok(())
    }
}

struct ReadDirectoryChangesServer {
    rx: Receiver<Action>,
    event_tx: EventTx,
    cmd_tx: Sender<Result<PathBuf>>,
    inner: ReadDirectoryChangesServerInner,
    wakeup_sem: HANDLE,
    recursion_adapter: RecursionAdapter,
}

fn add_watch_check_file(
    path: PathBuf,
    recursive_mode: RecursiveMode,
    recursion_adapter: &mut RecursionAdapter,
    inner: &mut ReadDirectoryChangesServerInner,
) -> Result<PathBuf> {
    let file_type = std::fs::metadata(&path).map(|m| m.file_type())?;
    if file_type.is_file() {
        // emulate file watching by watching the parent directory
        // and filtering events
        let new_path = path.parent().unwrap().to_path_buf();

        inner
            .file_watches
            .insert(path.to_path_buf(), new_path.to_path_buf());

        let new_recursive_mode = RecursiveMode::Filtered(RecursionFilter {
            filter: Box::new(move |f| f.path == path),
            follow_links: true,
        });

        let new_path_clone = new_path.to_path_buf();
        recursion_adapter.add_root(new_path, new_recursive_mode, inner)?;
        Ok(new_path_clone)
    } else if file_type.is_dir() {
        let path_clone = path.to_path_buf();
        recursion_adapter.add_root(path, recursive_mode, inner)?;
        Ok(path_clone)
    } else {
        Err(Error::Generic(
            "Input watch path is neither a file nor a directory.".to_owned(),
        ))
    }
}

impl ReadDirectoryChangesServer {
    fn start(
        event_tx: EventTx,
        meta_tx: Sender<MetaEvent>,
        cmd_tx: Sender<Result<PathBuf>>,
        wakeup_sem: HANDLE,
    ) -> Sender<Action> {
        let (action_tx, action_rx) = channel();
        // it is, in fact, ok to send the semaphore across threads
        let sem_temp = wakeup_sem as u64;
        let action_tx_clone = action_tx.clone();
        thread::spawn(move || {
            let wakeup_sem = sem_temp as HANDLE;
            let server = ReadDirectoryChangesServer {
                rx: action_rx,
                event_tx: event_tx,
                cmd_tx: cmd_tx,
                wakeup_sem: wakeup_sem,
                inner: ReadDirectoryChangesServerInner {
                    action_tx: action_tx_clone,
                    meta_tx: meta_tx,
                    watches: HashMap::new(),
                    file_watches: HashMap::new(),
                },
                recursion_adapter: RecursionAdapter::new(),
            };
            server.run();
        });
        action_tx
    }

    fn run(mut self) {
        loop {
            // process all available actions first
            let mut stopped = false;

            while let Ok(action) = self.rx.try_recv() {
                match action {
                    Action::Watch(path, recursive_mode) => {
                        let res = add_watch_check_file(
                            path,
                            recursive_mode,
                            &mut self.recursion_adapter,
                            &mut self.inner,
                        );
                        let _ = self.cmd_tx.send(res);
                    }
                    Action::Unwatch(path) => {
                        // TODO send result back to sender or log it if error
                        let _res = if let Some(watched_path) = self.inner.file_watches.remove(&path)
                        {
                            // we where watching the parent directory
                            self.recursion_adapter
                                .remove_root(&watched_path, &mut self.inner)
                        } else {
                            self.recursion_adapter.remove_root(&path, &mut self.inner)
                        };
                    }
                    Action::Stop => {
                        stopped = true;
                        if let Err(_e) = self.recursion_adapter.remove_all(&mut self.inner) {
                            // TODO log error?
                        }
                        break;
                    }
                    Action::Event(ev) => {
                        self.recursion_adapter.handle_event(
                            ev,
                            &mut self.inner,
                            &mut self.event_tx,
                        );
                    }
                }
            }

            if stopped {
                break;
            }

            unsafe {
                // wait with alertable flag so that the completion routine fires
                let waitres = kernel32::WaitForSingleObjectEx(self.wakeup_sem, 100, TRUE);
                if waitres == WAIT_OBJECT_0 {
                    let _ = self.inner.meta_tx.send(MetaEvent::WatcherAwakened);
                }
            }
        }

        // we have to clean this up, since the watcher may be long gone
        unsafe {
            kernel32::CloseHandle(self.wakeup_sem);
        }
    }
}

fn stop_watch(ws: &WatchState, meta_tx: &Sender<MetaEvent>) {
    unsafe {
        let cio = kernel32::CancelIo(ws.dir_handle);
        let ch = kernel32::CloseHandle(ws.dir_handle);
        // have to wait for it, otherwise we leak the memory allocated for there read request
        if cio != 0 && ch != 0 {
            kernel32::WaitForSingleObjectEx(ws.complete_sem, INFINITE, TRUE);
        }
        kernel32::CloseHandle(ws.complete_sem);
    }
    let _ = meta_tx.send(MetaEvent::SingleWatchComplete);
}

fn start_read(rd: &ReadData, action_tx: Sender<Action>, handle: HANDLE) {
    let mut request = Box::new(ReadDirectoryRequest {
        action_tx: action_tx,
        handle: handle,
        buffer: [0u8; BUF_SIZE as usize],
        data: rd.clone(),
    });

    let flags = winnt::FILE_NOTIFY_CHANGE_FILE_NAME
        | winnt::FILE_NOTIFY_CHANGE_DIR_NAME
        | winnt::FILE_NOTIFY_CHANGE_ATTRIBUTES
        | winnt::FILE_NOTIFY_CHANGE_SIZE
        | winnt::FILE_NOTIFY_CHANGE_LAST_WRITE
        | winnt::FILE_NOTIFY_CHANGE_CREATION
        | winnt::FILE_NOTIFY_CHANGE_SECURITY;

    let monitor_subdir = if request.data.is_recursive { 1 } else { 0 };

    unsafe {
        let mut overlapped: Box<OVERLAPPED> = Box::new(mem::zeroed());
        // When using callback based async requests, we are allowed to use the hEvent member
        // for our own purposes

        let req_buf = request.buffer.as_mut_ptr() as *mut c_void;
        let request_p = Box::into_raw(request) as *mut c_void;
        overlapped.hEvent = request_p;

        // This is using an asynchronous call with a completion routine for receiving notifications
        // An I/O completion port would probably be more performant
        let ret = winbase::ReadDirectoryChangesW(
            handle,
            req_buf,
            BUF_SIZE,
            monitor_subdir,
            flags,
            &mut 0u32 as *mut u32, // not used for async reqs
            &mut *overlapped as *mut OVERLAPPED,
            Some(handle_event),
        );

        if ret == 0 {
            // error reading. retransmute request memory to allow drop.
            // allow overlapped to drop by omitting forget()
            let request: Box<ReadDirectoryRequest> = mem::transmute(request_p);

            kernel32::ReleaseSemaphore(request.data.complete_sem, 1, ptr::null_mut());
        } else {
            // read ok. forget overlapped to let the completion routine handle memory
            mem::forget(overlapped);
        }
    }
}

fn send_pending_rename_event(event: Option<RawEvent>, action_tx: &mut Sender<Action>) {
    if let Some(e) = event {
        action_tx
            .send(Action::Event(RawEvent {
                path: e.path,
                op: Ok(op::Op::REMOVE),
                cookie: None,
            }))
            .unwrap();
    }
}

unsafe extern "system" fn handle_event(
    error_code: u32,
    _bytes_written: u32,
    overlapped: LPOVERLAPPED,
) {
    let overlapped: Box<OVERLAPPED> = Box::from_raw(overlapped);
    let request: Box<ReadDirectoryRequest> = Box::from_raw(overlapped.hEvent as *mut _);

    if error_code == ERROR_OPERATION_ABORTED {
        // received when dir is unwatched or watcher is shutdown; return and let overlapped/request
        // get drop-cleaned
        kernel32::ReleaseSemaphore(request.data.complete_sem, 1, ptr::null_mut());
        return;
    }

    handle_event_inner(request);
}

fn handle_event_inner(mut request: Box<ReadDirectoryRequest>) {
    // Get the next request queued up as soon as possible
    start_read(&request.data, request.action_tx.clone(), request.handle);

    let mut rename_event = None;

    // The FILE_NOTIFY_INFORMATION struct has a variable length due to the variable length
    // string as its last member.  Each struct contains an offset for getting the next entry in
    // the buffer.
    let mut cur_offset: *const u8 = request.buffer.as_ptr();
    let mut cur_entry: &mut FILE_NOTIFY_INFORMATION = unsafe { mem::transmute(cur_offset) };
    loop {
        // filename length is size in bytes, so / 2
        let len = cur_entry.FileNameLength as usize / 2;
        let encoded_path: &[u16] =
            unsafe { slice::from_raw_parts((*cur_entry).FileName.as_ptr(), len) };
        // prepend root to get a full path
        let path = request
            .data
            .dir
            .join(PathBuf::from(OsString::from_wide(encoded_path)));

        if cur_entry.Action == winnt::FILE_ACTION_RENAMED_OLD_NAME {
            send_pending_rename_event(rename_event, &mut request.action_tx);
            unsafe {
                COOKIE_COUNTER = COOKIE_COUNTER.wrapping_add(1);
                rename_event = Some(RawEvent {
                    path: Some(path),
                    op: Ok(op::Op::RENAME),
                    cookie: Some(COOKIE_COUNTER),
                });
            }
        } else {
            let mut o = Op::empty();
            let mut c = None;

            match cur_entry.Action {
                winnt::FILE_ACTION_RENAMED_NEW_NAME => {
                    if let Some(e) = rename_event {
                        if let Some(cookie) = e.cookie {
                            request.action_tx.send(Action::Event(e)).unwrap();
                            o.insert(op::Op::RENAME);
                            c = Some(cookie);
                        } else {
                            o.insert(op::Op::CREATE);
                        }
                    } else {
                        o.insert(op::Op::CREATE);
                    }
                    rename_event = None;
                }
                winnt::FILE_ACTION_ADDED => o.insert(op::Op::CREATE),
                winnt::FILE_ACTION_REMOVED => o.insert(op::Op::REMOVE),
                winnt::FILE_ACTION_MODIFIED => o.insert(op::Op::WRITE),
                _ => (),
            };

            send_pending_rename_event(rename_event, &mut request.action_tx);
            rename_event = None;

            request
                .action_tx
                .send(Action::Event(RawEvent {
                    path: Some(path),
                    op: Ok(o),
                    cookie: c,
                }))
                .unwrap();
        }

        if cur_entry.NextEntryOffset == 0 {
            break;
        }
        cur_offset = unsafe { cur_offset.offset(cur_entry.NextEntryOffset as isize) };
        cur_entry = unsafe { mem::transmute(cur_offset) };
    }

    send_pending_rename_event(rename_event, &mut request.action_tx);
}

pub struct ReadDirectoryChangesWatcher {
    tx: Sender<Action>,
    cmd_rx: Receiver<Result<PathBuf>>,
    wakeup_sem: HANDLE,
}

impl ReadDirectoryChangesWatcher {
    pub fn create(
        tx: Sender<RawEvent>,
        meta_tx: Sender<MetaEvent>,
    ) -> Result<ReadDirectoryChangesWatcher> {
        let (cmd_tx, cmd_rx) = channel();

        let wakeup_sem =
            unsafe { kernel32::CreateSemaphoreW(ptr::null_mut(), 0, 1, ptr::null_mut()) };
        if wakeup_sem == ptr::null_mut() || wakeup_sem == INVALID_HANDLE_VALUE {
            return Err(Error::Generic(
                "Failed to create wakeup semaphore.".to_owned(),
            ));
        }

        let event_tx = EventTx::Raw { tx: tx };

        let action_tx = ReadDirectoryChangesServer::start(event_tx, meta_tx, cmd_tx, wakeup_sem);

        Ok(ReadDirectoryChangesWatcher {
            tx: action_tx,
            cmd_rx: cmd_rx,
            wakeup_sem: wakeup_sem,
        })
    }

    pub fn create_debounced(
        tx: Sender<DebouncedEvent>,
        meta_tx: Sender<MetaEvent>,
        delay: Duration,
    ) -> Result<ReadDirectoryChangesWatcher> {
        let (cmd_tx, cmd_rx) = channel();

        let wakeup_sem =
            unsafe { kernel32::CreateSemaphoreW(ptr::null_mut(), 0, 1, ptr::null_mut()) };
        if wakeup_sem == ptr::null_mut() || wakeup_sem == INVALID_HANDLE_VALUE {
            return Err(Error::Generic(
                "Failed to create wakeup semaphore.".to_owned(),
            ));
        }

        let event_tx = EventTx::Debounced {
            tx: tx.clone(),
            debounce: Debounce::new(delay, tx),
        };

        let action_tx = ReadDirectoryChangesServer::start(event_tx, meta_tx, cmd_tx, wakeup_sem);

        Ok(ReadDirectoryChangesWatcher {
            tx: action_tx,
            cmd_rx: cmd_rx,
            wakeup_sem: wakeup_sem,
        })
    }

    fn wakeup_server(&mut self) {
        // breaks the server out of its wait state.  right now this is really just an optimization,
        // so that if you add a watch you don't block for 100ms in watch() while the
        // server sleeps.
        unsafe {
            kernel32::ReleaseSemaphore(self.wakeup_sem, 1, ptr::null_mut());
        }
    }

    fn send_action_require_ack(&mut self, action: Action, pb: &PathBuf) -> Result<()> {
        match self.tx.send(action) {
            Err(_) => Err(Error::Generic(
                "Error sending to internal channel".to_owned(),
            )),
            Ok(_) => {
                // wake 'em up, we don't want to wait around for the ack
                self.wakeup_server();

                match self.cmd_rx.recv() {
                    Err(_) => Err(Error::Generic(
                        "Error receiving from command channel".to_owned(),
                    )),
                    Ok(ack_res) => match ack_res {
                        Err(e) => Err(Error::Generic(format!("Error in watcher: {:?}", e))),
                        Ok(ack_pb) => {
                            if pb.as_path() != ack_pb.as_path() {
                                Err(Error::Generic(format!(
                                    "Expected ack for {:?} but got \
                                     ack for {:?}",
                                    pb, ack_pb
                                )))
                            } else {
                                Ok(())
                            }
                        }
                    },
                }
            }
        }
    }
}

impl Watcher for ReadDirectoryChangesWatcher {
    fn new_raw(tx: Sender<RawEvent>) -> Result<ReadDirectoryChangesWatcher> {
        // create dummy channel for meta event
        let (meta_tx, _) = channel();
        ReadDirectoryChangesWatcher::create(tx, meta_tx)
    }

    fn new(tx: Sender<DebouncedEvent>, delay: Duration) -> Result<ReadDirectoryChangesWatcher> {
        // create dummy channel for meta event
        let (meta_tx, _) = channel();
        ReadDirectoryChangesWatcher::create_debounced(tx, meta_tx, delay)
    }

    fn watch<P: AsRef<Path>>(&mut self, path: P, recursive_mode: RecursiveMode) -> Result<()> {
        let pb = if path.as_ref().is_absolute() {
            path.as_ref().to_owned()
        } else {
            let p = try!(env::current_dir().map_err(Error::Io));
            p.join(path)
        };
        // path must exist and be either a file or directory
        if !pb.is_dir() && !pb.is_file() {
            return Err(Error::Generic(
                "Input watch path is neither a file nor a directory.".to_owned(),
            ));
        }
        self.send_action_require_ack(Action::Watch(pb.clone(), recursive_mode), &pb)
    }

    fn unwatch<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let pb = if path.as_ref().is_absolute() {
            path.as_ref().to_owned()
        } else {
            let p = try!(env::current_dir().map_err(Error::Io));
            p.join(path)
        };
        let res = self
            .tx
            .send(Action::Unwatch(pb))
            .map_err(|_| Error::Generic("Error sending to internal channel".to_owned()));
        self.wakeup_server();
        res
    }
}

impl Drop for ReadDirectoryChangesWatcher {
    fn drop(&mut self) {
        let _ = self.tx.send(Action::Stop);
        // better wake it up
        self.wakeup_server();
    }
}

// `ReadDirectoryChangesWatcher` is not Send/Sync because of the semaphore Handle.
// As said elsewhere it's perfectly safe to send it across threads.
unsafe impl Send for ReadDirectoryChangesWatcher {}
// Because all public methods are `&mut self` it's also perfectly safe to share references.
unsafe impl Sync for ReadDirectoryChangesWatcher {}
