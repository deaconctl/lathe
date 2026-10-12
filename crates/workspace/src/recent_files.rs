use std::{
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use db::{
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use fs::Fs;
use gpui::App;

/// How many files the list keeps. Older entries are dropped as new ones are recorded.
const MAX_RECENT_FILES: i64 = 100;

/// Files opened in any local project, most recent first, shared by every window.
/// Kept separate from recent projects so a single file can be reopened on its own.
pub struct RecentFilesDb(ThreadSafeConnection);

impl Domain for RecentFilesDb {
    const NAME: &str = stringify!(RecentFilesDb);

    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE recent_files (
            path TEXT PRIMARY KEY,
            last_opened INTEGER NOT NULL
        ) STRICT;
    )];
}

db::static_connection!(RecentFilesDb, []);

impl RecentFilesDb {
    query! {
        async fn upsert_recent_file(path: String, last_opened: i64) -> Result<()> {
            INSERT OR REPLACE INTO recent_files(path, last_opened) VALUES (?, ?)
        }
    }

    query! {
        async fn prune_recent_files(keep: i64) -> Result<()> {
            DELETE FROM recent_files WHERE path NOT IN (
                SELECT path FROM recent_files ORDER BY last_opened DESC, rowid DESC LIMIT ?
            )
        }
    }

    query! {
        fn recent_file_paths() -> Result<Vec<String>> {
            SELECT path FROM recent_files ORDER BY last_opened DESC, rowid DESC
        }
    }

    query! {
        pub async fn remove_recent_file(path: String) -> Result<()> {
            DELETE FROM recent_files WHERE path = ?
        }
    }

    pub async fn record_recent_file(&self, path: PathBuf) -> anyhow::Result<()> {
        let last_opened = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or_default();
        self.upsert_recent_file(path.to_string_lossy().into_owned(), last_opened)
            .await?;
        self.prune_recent_files(MAX_RECENT_FILES).await
    }

    /// Recent files that still exist on disk, most recent first.
    pub async fn recent_files(&self, fs: Arc<dyn Fs>) -> anyhow::Result<Vec<PathBuf>> {
        let mut existing = Vec::new();
        for path in self.recent_file_paths()? {
            let path = PathBuf::from(path);
            if fs.is_file(&path).await {
                existing.push(path);
            }
        }
        Ok(existing)
    }
}

/// Remembers `abs_path` as the most recently opened file.
pub fn record_recent_file(abs_path: PathBuf, cx: &App) {
    let db = RecentFilesDb::global(cx);
    db::write_and_log(
        cx,
        move || async move { db.record_recent_file(abs_path).await },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;

    #[gpui::test]
    async fn test_recent_files_are_most_recent_first_and_skip_missing(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({ "a.rs": "", "b.rs": "" }))
            .await;
        let db = RecentFilesDb::open_test_db("test_recent_files_ordering").await;

        db.record_recent_file("/root/a.rs".into()).await.unwrap();
        db.record_recent_file("/root/b.rs".into()).await.unwrap();
        db.record_recent_file("/root/deleted.rs".into())
            .await
            .unwrap();
        db.record_recent_file("/root/a.rs".into()).await.unwrap();

        assert_eq!(
            db.recent_files(fs.clone()).await.unwrap(),
            vec![PathBuf::from("/root/a.rs"), PathBuf::from("/root/b.rs")],
            "reopening a file moves it to the top, and files gone from disk are hidden"
        );

        db.remove_recent_file("/root/a.rs".into()).await.unwrap();
        assert_eq!(
            db.recent_files(fs).await.unwrap(),
            vec![PathBuf::from("/root/b.rs")]
        );
    }
}
