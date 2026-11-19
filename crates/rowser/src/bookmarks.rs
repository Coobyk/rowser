use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

type Version = (u64, u64, u64, i64, i64, i64, i64);

pub struct Bookmarks {
    path: PathBuf,
    version: Option<Version>,
    entries: Vec<String>,
    #[cfg(test)]
    reads: usize,
}

impl Bookmarks {
    pub fn new(path: PathBuf) -> Self {
        let mut bookmarks = Self {
            path,
            version: None,
            entries: Vec::new(),
            #[cfg(test)]
            reads: 0,
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
        self.version = self.version();
        #[cfg(test)]
        {
            self.reads += 1;
        }
        self.entries = std::fs::read_to_string(&self.path)
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();
    }

    pub fn has_changed(&self) -> bool {
        self.version != self.version()
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
    fn change_detection_does_not_read_or_reload() {
        let fixture = Fixture::new();
        let mut bookmarks = Bookmarks::new(fixture.file());
        assert!(!bookmarks.has_changed());
        std::fs::write(fixture.file(), "one\n").unwrap();
        assert!(bookmarks.has_changed());
        assert!(bookmarks.has_changed());
        assert_eq!(bookmarks.reads, 1);
        assert!(bookmarks.entries.is_empty());
        assert!(bookmarks.contains("one"));
        assert!(!bookmarks.has_changed());

        std::fs::write(fixture.file(), "longer-url\n").unwrap();
        assert!(bookmarks.has_changed());
        assert_eq!(bookmarks.reads, 2);
        assert!(bookmarks.contains("longer-url"));
        assert!(!bookmarks.has_changed());

        std::fs::remove_file(fixture.file()).unwrap();
        assert!(bookmarks.has_changed());
        assert_eq!(bookmarks.reads, 3);
        assert!(!bookmarks.contains("longer-url"));
        assert!(!bookmarks.has_changed());
    }

    #[test]
    fn unchanged_bookmarks_are_read_only_once() {
        let fixture = Fixture::new();
        std::fs::write(fixture.file(), "https://example.org\n").unwrap();
        let mut bookmarks = Bookmarks::new(fixture.file());
        for _ in 0..100 {
            assert!(bookmarks.contains("https://example.org"));
            assert!(!bookmarks.contains("https://other.org"));
        }
        assert_eq!(bookmarks.reads, 1);
    }

    #[test]
    fn external_changes_and_toggle_are_reflected() {
        let fixture = Fixture::new();
        let mut bookmarks = Bookmarks::new(fixture.file());
        assert!(!bookmarks.contains("one"));
        assert_eq!(bookmarks.reads, 1);
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
        assert_eq!(bookmarks.reads, 2);
    }
}
