use std::cell::Cell;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type Version = (u64, u64, u64, i64, i64, i64, i64);

// Cover filesystems with two-second timestamp granularity.
const RACY_MARGIN: Duration = Duration::from_secs(2);

fn timestamp_is_racy(version: Option<Version>, now: SystemTime) -> bool {
    let Some((_, _, _, seconds, nanos, _, _)) = version else {
        return false;
    };
    let modified = UNIX_EPOCH.checked_add(Duration::new(seconds.max(0) as u64, nanos as u32));
    modified
        .and_then(|modified| now.duration_since(modified).ok())
        .is_none_or(|age| age <= RACY_MARGIN)
}

pub struct Bookmarks {
    path: PathBuf,
    version: Option<Version>,
    contents: Option<String>,
    racy: Cell<bool>,
    entries: Vec<String>,
    #[cfg(test)]
    reloads: usize,
    #[cfg(test)]
    reads: Cell<usize>,
}

impl Bookmarks {
    pub fn new(path: PathBuf) -> Self {
        let mut bookmarks = Self {
            path,
            version: None,
            contents: None,
            racy: Cell::new(false),
            entries: Vec::new(),
            #[cfg(test)]
            reloads: 0,
            #[cfg(test)]
            reads: Cell::new(0),
        };
        bookmarks.reload();
        bookmarks
    }

    fn version(&self) -> Option<Version> {
        std::fs::metadata(&self.path).ok().map(|m| {
            (
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        })
    }

    fn reload(&mut self) {
        let now = SystemTime::now();
        self.version = self.version();
        self.racy.set(timestamp_is_racy(self.version, now));
        #[cfg(test)]
        {
            self.reloads += 1;
        }
        self.contents = self.read_contents();
        self.entries = self
            .contents
            .as_ref()
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
    }

    pub fn has_changed(&self) -> bool {
        let now = SystemTime::now();
        if self.version != self.version() {
            return true;
        }
        if !self.racy.get() {
            return false;
        }
        if self.contents != self.read_contents() || self.version != self.version() {
            return true;
        }
        self.racy.set(timestamp_is_racy(self.version, now));
        false
    }

    fn read_contents(&self) -> Option<String> {
        #[cfg(test)]
        self.reads.set(self.reads.get() + 1);
        std::fs::read_to_string(&self.path).ok()
    }

    fn refresh(&mut self) {
        if self.has_changed() {
            self.reload();
        }
    }

    pub fn contains(&mut self, url: &str) -> bool {
        self.refresh();
        self.entries.iter().any(|entry| entry == url)
    }

    pub fn toggle(&mut self, url: &str) {
        self.refresh();
        if let Some(i) = self.entries.iter().position(|entry| entry == url) {
            self.entries.remove(i);
        } else {
            self.entries.push(url.to_string());
        }
        let _ = std::fs::write(&self.path, self.entries.join("\n") + "\n");
        self.reload();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rowser-bookmarks-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("bookmarks.txt")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn change_detection_does_not_reload() {
        let fixture = Fixture::new();
        let mut bookmarks = Bookmarks::new(fixture.file());
        assert!(!bookmarks.has_changed());
        std::fs::write(fixture.file(), "one\n").unwrap();
        assert!(bookmarks.has_changed());
        assert!(bookmarks.has_changed());
        assert_eq!(bookmarks.reloads, 1);
        assert!(bookmarks.entries.is_empty());
        assert!(bookmarks.contains("one"));
        assert!(!bookmarks.has_changed());

        std::fs::write(fixture.file(), "longer-url\n").unwrap();
        assert!(bookmarks.has_changed());
        assert_eq!(bookmarks.reloads, 2);
        assert!(bookmarks.contains("longer-url"));
        assert!(!bookmarks.has_changed());

        std::fs::remove_file(fixture.file()).unwrap();
        assert!(bookmarks.has_changed());
        assert_eq!(bookmarks.reloads, 3);
        assert!(!bookmarks.contains("longer-url"));
        assert!(!bookmarks.has_changed());
    }

    #[test]
    fn unchanged_non_racy_bookmarks_are_read_only_once() {
        let fixture = Fixture::new();
        std::fs::write(fixture.file(), "https://example.org\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(fixture.file())
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(10))
            .unwrap();
        let mut bookmarks = Bookmarks::new(fixture.file());
        assert!(!bookmarks.racy.get());
        for _ in 0..100 {
            assert!(!bookmarks.has_changed());
            assert!(bookmarks.contains("https://example.org"));
            assert!(!bookmarks.contains("https://other.org"));
        }
        assert_eq!(bookmarks.reloads, 1);
        assert_eq!(bookmarks.reads.get(), 1);
    }

    #[test]
    fn matching_contents_become_trusted_after_racy_window() {
        let fixture = Fixture::new();
        std::fs::write(fixture.file(), "one\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(fixture.file())
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(10))
            .unwrap();
        let mut bookmarks = Bookmarks::new(fixture.file());
        bookmarks.racy.set(true);
        assert!(!bookmarks.has_changed());
        assert!(!bookmarks.racy.get());
        assert_eq!(bookmarks.reads.get(), 2);
        for _ in 0..100 {
            assert!(!bookmarks.has_changed());
            assert!(bookmarks.contains("one"));
        }
        assert_eq!(bookmarks.reads.get(), 2);
    }

    #[test]
    fn timestamp_window_is_conservative() {
        let version = Some((0, 0, 4, 100, 0, 100, 0));
        assert!(timestamp_is_racy(
            version,
            UNIX_EPOCH + Duration::from_secs(99)
        ));
        assert!(timestamp_is_racy(
            version,
            UNIX_EPOCH + Duration::from_secs(102)
        ));
        assert!(!timestamp_is_racy(
            version,
            UNIX_EPOCH + Duration::from_secs(103)
        ));
    }

    #[test]
    fn external_edit_with_matching_metadata_is_detected() {
        let fixture = Fixture::new();
        std::fs::write(fixture.file(), "one\n").unwrap();
        let mut bookmarks = Bookmarks::new(fixture.file());
        assert!(bookmarks.racy.get());
        std::fs::write(fixture.file(), "two\n").unwrap();
        // Model a timestamp collision regardless of filesystem granularity.
        bookmarks.version = bookmarks.version();
        assert!(bookmarks.has_changed());
        assert!(!bookmarks.contains("one"));
        assert!(bookmarks.contains("two"));
        assert!(!bookmarks.has_changed());
    }

    #[test]
    fn external_changes_and_toggle_are_reflected() {
        let fixture = Fixture::new();
        let mut bookmarks = Bookmarks::new(fixture.file());
        assert!(!bookmarks.contains("one"));
        assert_eq!(bookmarks.reloads, 1);
        bookmarks.toggle("one");
        assert!(bookmarks.contains("one"));
        assert_eq!(std::fs::read_to_string(fixture.file()).unwrap(), "one\n");
        std::fs::write(fixture.file(), "two\n").unwrap();
        assert!(!bookmarks.contains("one"));
        assert!(bookmarks.contains("two"));
        bookmarks.toggle("two");
        assert!(!bookmarks.contains("two"));
        assert_eq!(std::fs::read_to_string(fixture.file()).unwrap(), "\n");
        std::fs::remove_file(fixture.file()).unwrap();
        assert!(!bookmarks.contains("two"));
    }

    #[test]
    fn failed_write_does_not_leave_a_phantom_bookmark() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.file()).unwrap();
        let mut bookmarks = Bookmarks::new(fixture.file());
        bookmarks.toggle("one");
        assert!(!bookmarks.contains("one"));
        for _ in 0..100 {
            assert!(!bookmarks.contains("one"));
        }
        assert_eq!(bookmarks.reloads, 2);
    }
}
