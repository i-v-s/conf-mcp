use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;

use crate::confluence::{Image, MAX_IMAGE_BYTES, image_mime};
use crate::hash::sha256_hex;

const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
pub struct ImageCache {
    directory: Option<PathBuf>,
    locks: Arc<Mutex<HashMap<String, Weak<AsyncMutex<()>>>>>,
}

#[derive(Debug, Serialize)]
pub struct ImageFile {
    pub path: String,
    pub mime_type: String,
    pub size_bytes: usize,
    pub cached: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheEntry {
    content_hash: String,
    mime_type: String,
    size_bytes: usize,
    fetched_at: u64,
}

impl ImageCache {
    pub fn new(directory: Option<PathBuf>) -> Self {
        Self {
            directory,
            locks: Arc::default(),
        }
    }

    pub async fn get_or_fetch<F, Fut>(
        &self,
        key: String,
        refresh: bool,
        fetch: F,
    ) -> Result<ImageFile>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<Image>> + Send,
    {
        let directory = self
            .directory
            .clone()
            .context("Не удалось определить download_dir; задайте его в ~/.config/conf-mcp.toml")?;
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .map_err(|_| anyhow::anyhow!("Не удалось получить блокировку кэша изображений"))?;
            locks.retain(|_, value| value.strong_count() > 0);
            if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(AsyncMutex::new(()));
                locks.insert(key.clone(), Arc::downgrade(&lock));
                lock
            }
        };
        let _guard = lock.lock().await;
        let root = blocking(move || prepare_directory(&directory)).await?;
        if !refresh {
            let root_copy = root.clone();
            let key_copy = key.clone();
            if let Some(image) = blocking(move || Ok(cached_image(&root_copy, &key_copy))).await? {
                return Ok(image);
            }
        }
        let image = fetch().await?;
        blocking(move || save_image(&root, &key, image)).await
    }
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(operation)
        .await
        .context("Не удалось выполнить файловую операцию кэша изображений")?
}

fn prepare_directory(directory: &Path) -> Result<PathBuf> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(directory)
        .context("Не удалось создать download_dir; проверьте путь и права записи")?;
    directory
        .canonicalize()
        .context("Не удалось определить абсолютный путь download_dir")
}

fn index_path(root: &Path, key: &str) -> PathBuf {
    root.join(format!(".index-{key}.json"))
}

fn cached_image(root: &Path, key: &str) -> Option<ImageFile> {
    let bytes = read_limited(&index_path(root, key), 4096).ok()?;
    let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now.checked_sub(entry.fetched_at)? >= CACHE_TTL.as_secs() {
        return None;
    }
    let path = entry_path(root, &entry)?;
    if !valid_file(&path, &entry) {
        return None;
    }
    image_file(&path, &entry, true).ok()
}

fn entry_path(root: &Path, entry: &CacheEntry) -> Option<PathBuf> {
    if entry.content_hash.len() != 64
        || !entry
            .content_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || entry.size_bytes > MAX_IMAGE_BYTES
    {
        return None;
    }
    let ext = extension(&entry.mime_type)?;
    Some(root.join(format!("{}.{}", entry.content_hash, ext)))
}

fn valid_file(path: &Path, entry: &CacheEntry) -> bool {
    let Ok(data) = read_limited(path, MAX_IMAGE_BYTES) else {
        return false;
    };
    data.len() == entry.size_bytes
        && image_mime(&data) == Some(entry.mime_type.as_str())
        && sha256_hex(&[&data]) == entry.content_hash
}

fn save_image(root: &Path, key: &str, image: Image) -> Result<ImageFile> {
    if image.data.len() > MAX_IMAGE_BYTES || image_mime(&image.data) != Some(image.mime_type) {
        bail!("Некорректное изображение для сохранения в download_dir");
    }
    let entry = CacheEntry {
        content_hash: sha256_hex(&[&image.data]),
        mime_type: image.mime_type.into(),
        size_bytes: image.data.len(),
        fetched_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("Некорректное системное время для кэша изображений")?
            .as_secs(),
    };
    let path =
        entry_path(root, &entry).context("Неподдерживаемый формат изображения для сохранения")?;
    if !valid_file(&path, &entry) {
        atomic_write(&path, &image.data)?;
    }
    // Индекс публикуется только после полной записи изображения.
    atomic_write(&index_path(root, key), &serde_json::to_vec(&entry)?)?;
    image_file(&path, &entry, false)
}

fn image_file(path: &Path, entry: &CacheEntry, cached: bool) -> Result<ImageFile> {
    Ok(ImageFile {
        path: path
            .to_str()
            .context("Путь download_dir должен содержать корректный UTF-8")?
            .into(),
        mime_type: entry.mime_type.clone(),
        size_bytes: entry.size_bytes,
        cached,
    })
}

fn read_limited(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(std::io::Error::other("Некорректный размер файла кэша"));
    }
    let mut data = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut data)?;
    if data.len() > limit {
        return Err(std::io::Error::other("Файл кэша превышает лимит"));
    }
    Ok(data)
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let root = path.parent().context("Некорректный путь файла кэша")?;
    // NamedTempFile создаёт файл с правами 0600 на Unix.
    let mut temporary = tempfile::NamedTempFile::new_in(root)
        .context("Не удалось создать файл в download_dir; проверьте права записи")?;
    temporary
        .write_all(data)
        .context("Не удалось записать файл в download_dir")?;
    temporary
        .flush()
        .context("Не удалось завершить запись файла в download_dir")?;
    temporary.persist(path).map_err(|_| {
        anyhow::anyhow!("Не удалось опубликовать файл в download_dir; проверьте права записи")
    })?;
    Ok(())
}

fn extension(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn image(suffix: u8) -> Image {
        let mut data = b"\x89PNG\r\n\x1a\ncache-fixture".to_vec();
        data.push(suffix);
        Image {
            data,
            mime_type: "image/png",
        }
    }

    fn key() -> String {
        sha256_hex(&[b"test-url-and-authorization"])
    }

    #[tokio::test]
    async fn files_are_persistent_private_and_cache_survives_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("nested/images");
        let cache = ImageCache::new(Some(directory.clone()));
        assert!(!directory.exists());
        let saved = cache
            .get_or_fetch(key(), false, || async { Ok(image(1)) })
            .await
            .unwrap();
        assert!(!saved.cached);
        assert!(Path::new(&saved.path).is_absolute());
        assert_eq!(fs::read(&saved.path).unwrap(), image(1).data);
        assert_eq!(saved.size_bytes, image(1).data.len());
        assert_eq!(saved.mime_type, "image/png");
        let index = fs::read_to_string(index_path(&directory, &key())).unwrap();
        assert!(!index.contains("test-url"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            for path in [PathBuf::from(&saved.path), index_path(&directory, &key())] {
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        drop(cache);
        let restarted = ImageCache::new(Some(directory));
        let hit = restarted
            .get_or_fetch(key(), false, || async {
                bail!("HTTP не должен вызываться")
            })
            .await
            .unwrap();
        assert!(hit.cached);
        assert_eq!(hit.path, saved.path);
    }

    #[tokio::test]
    async fn refresh_preserves_old_files_and_failed_refresh_preserves_cache() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(Some(temporary.path().into()));
        let first = cache
            .get_or_fetch(key(), false, || async { Ok(image(1)) })
            .await
            .unwrap();
        let refreshed = cache
            .get_or_fetch(key(), true, || async { Ok(image(2)) })
            .await
            .unwrap();
        assert!(!refreshed.cached);
        assert_ne!(first.path, refreshed.path);
        assert_eq!(fs::read(&first.path).unwrap(), image(1).data);
        assert!(
            cache
                .get_or_fetch(key(), true, || async { bail!("HTTP 503") })
                .await
                .is_err()
        );
        let hit = cache
            .get_or_fetch(key(), false, || async {
                bail!("HTTP не должен вызываться")
            })
            .await
            .unwrap();
        assert!(hit.cached);
        assert_eq!(hit.path, refreshed.path);
    }

    #[tokio::test]
    async fn stale_missing_and_corrupt_entries_are_downloaded_again() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let cache = ImageCache::new(Some(root.into()));
        let count = AtomicUsize::new(0);
        let fetch = || async {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(image(1))
        };
        let first = cache.get_or_fetch(key(), false, fetch).await.unwrap();
        let mut entry: CacheEntry =
            serde_json::from_slice(&fs::read(index_path(root, &key())).unwrap()).unwrap();
        entry.fetched_at -= CACHE_TTL.as_secs();
        fs::write(
            index_path(root, &key()),
            serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();
        assert!(
            !cache
                .get_or_fetch(key(), false, fetch)
                .await
                .unwrap()
                .cached
        );
        fs::write(&first.path, image(2).data).unwrap();
        assert!(
            !cache
                .get_or_fetch(key(), false, fetch)
                .await
                .unwrap()
                .cached
        );
        assert_eq!(fs::read(&first.path).unwrap(), image(1).data);
        fs::remove_file(&first.path).unwrap();
        assert!(
            !cache
                .get_or_fetch(key(), false, fetch)
                .await
                .unwrap()
                .cached
        );
        fs::write(index_path(root, &key()), b"not-json").unwrap();
        assert!(
            !cache
                .get_or_fetch(key(), false, fetch)
                .await
                .unwrap()
                .cached
        );
        let mut entry: CacheEntry =
            serde_json::from_slice(&fs::read(index_path(root, &key())).unwrap()).unwrap();
        entry.content_hash = "../../outside".into();
        fs::write(
            index_path(root, &key()),
            serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();
        assert!(
            !cache
                .get_or_fetch(key(), false, fetch)
                .await
                .unwrap()
                .cached
        );
        assert_eq!(count.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn concurrent_requests_share_download_but_distinct_keys_run_independently() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = ImageCache::new(Some(temporary.path().into()));
        let count = AtomicUsize::new(0);
        let fetch = || async {
            count.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok(image(1))
        };
        let (a, b, c) = tokio::join!(
            cache.get_or_fetch(key(), false, fetch),
            cache.get_or_fetch(key(), false, fetch),
            cache.get_or_fetch(key(), false, fetch),
        );
        let results = [a.unwrap(), b.unwrap(), c.unwrap()];
        assert_eq!(results.iter().filter(|r| !r.cached).count(), 1);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(results.iter().all(|r| r.path == results[0].path));

        let barrier = tokio::sync::Barrier::new(2);
        let fetch = || async {
            barrier.wait().await;
            Ok(image(2))
        };
        let results = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                cache.get_or_fetch("different-a".into(), false, fetch),
                cache.get_or_fetch("different-b".into(), false, fetch),
            )
        })
        .await
        .unwrap();
        assert!(!results.0.unwrap().cached);
        assert!(!results.1.unwrap().cached);
    }

    #[tokio::test]
    async fn bad_directory_and_write_errors_are_local_tool_errors() {
        let missing = ImageCache::new(None);
        assert!(
            missing
                .get_or_fetch(key(), false, || async { Ok(image(1)) })
                .await
                .unwrap_err()
                .to_string()
                .contains("download_dir")
        );
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("not-a-directory");
        fs::write(&file, b"keep").unwrap();
        let cache = ImageCache::new(Some(file.clone()));
        assert!(
            cache
                .get_or_fetch(key(), false, || async { Ok(image(1)) })
                .await
                .is_err()
        );
        assert_eq!(fs::read(file).unwrap(), b"keep");
        let cache = ImageCache::new(Some(temporary.path().into()));
        fs::create_dir(index_path(temporary.path(), &key())).unwrap();
        assert!(
            cache
                .get_or_fetch(key(), false, || async { Ok(image(1)) })
                .await
                .is_err()
        );
        assert!(cached_image(temporary.path(), &key()).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn existing_directory_permissions_are_not_changed() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("existing");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o750)).unwrap();
        let cache = ImageCache::new(Some(directory.clone()));
        cache
            .get_or_fetch(key(), false, || async { Ok(image(1)) })
            .await
            .unwrap();
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }
}
