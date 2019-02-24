extern crate notify;
extern crate tempdir;

mod utils;

use notify::*;
use std::env;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tempdir::TempDir;
use utils::*;

fn filtered_setup<E, W: Watcher, F: FnOnce(mpsc::Sender<E>) -> Result<W>>(
    create_watcher: F,
    io_delay_ms: u64,
) -> (TempDir, W, mpsc::Receiver<E>) {
    let tdir = TempDir::new("temp_dir").expect("failed to create temporary directory");

    let (tx, rx) = mpsc::channel();
    let mut watcher = create_watcher(tx).expect("failed to create watcher");

    let filter = RecursionFilter {
        follow_links: false,
        filter: Box::new(|d| {
            d.path
                .file_name()
                .and_then(|n| n.to_str())
                .map_or(true, |x| !x.starts_with("ignored_"))
        }),
    };
    watcher
        .watch(tdir.mkpath("."), RecursiveMode::Filtered(filter))
        .expect("failed to watch directory");

    let files = vec![
        "dir1",
        "dir1/file1",
        "dir1/ignored_dir",
        "dir1/ignored_dir/file1",
        "dir1/ignored_dir/subdir1",
        "dir1/ignored_dir/subdir1/file1",
        "dir1/subdir1",
        "dir1/subdir1/file1",
        "dir1/subdir1/file2",
        "dir1/subdir2",
        "dir1/subdir2/subdir1",
        "dir1/subdir2/subdir1/file1",
        "dir1/subdir2/subdir1/ignored_file",
        "dir1/subdir2/ignored_dir",
        "dir1/subdir2/ignored_dir/file1",
    ];

    for f in &files {
        tdir.create(f);
        sleep(io_delay_ms);
    }

    tdir.rename("dir1/ignored_dir", "dir1/non_ignored_dir");
    sleep(io_delay_ms);
    tdir.rename("dir1/subdir2", "dir1/ignored_subdir1");
    sleep(io_delay_ms);

    (tdir, watcher, rx)
}

#[test]
fn recommended_watcher_filtered() {
    let (_tdir, _watcher, rx) = filtered_setup(RecommendedWatcher::new_raw, 10);
    let evs = recv_events(&rx);

    println!("{:#?}", evs);
}

#[test]
fn poll_watcher_filtered() {
    let (_tdir, _watcher, rx) = filtered_setup(|s| PollWatcher::with_delay_ms(s, 100), 250);
    let evs = recv_events(&rx);

    println!("{:#?}", evs);
}

#[test]
fn recommended_watcher_filtered_debounced() {
    let (_tdir, _watcher, rx) = filtered_setup(
        |s| RecommendedWatcher::new(s, Duration::from_millis(250)),
        10,
    );
    let evs = utils::recv_events_debounced(&rx, Duration::from_millis(500));

    println!("{:#?}", evs);
}
