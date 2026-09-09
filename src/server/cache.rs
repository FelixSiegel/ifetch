use lru::LruCache;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Memory-bounded LRU cache for image bytes.
///
/// Tracks total stored bytes and evicts least recently used entries
/// whenever `current_bytes` exceeds `max_bytes`.
pub struct ImageLruCache {
    max_bytes: usize,
    current_bytes: usize,
    cache: LruCache<String, Arc<[u8]>>,
}

impl ImageLruCache {
    /// Creates a new `ImageLruCache` bounded to `max_bytes`.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            current_bytes: 0,
            cache: LruCache::unbounded(),
        }
    }

    /// Retrieves cached image bytes if present.
    pub fn get(&mut self, key: &str) -> Option<Arc<[u8]>> {
        self.cache.get(key).cloned()
    }

    /// Inserts an image into the cache, evicting older items if the total byte limit is reached.
    pub fn insert(&mut self, key: String, data: Arc<[u8]>) {
        let size = data.len();
        if size > self.max_bytes {
            return;
        }

        if let Some(old) = self.cache.put(key, data) {
            self.current_bytes = self.current_bytes.saturating_sub(old.len());
        }

        self.current_bytes += size;

        while self.current_bytes > self.max_bytes {
            if let Some((_k, old)) = self.cache.pop_lru() {
                self.current_bytes = self.current_bytes.saturating_sub(old.len());
            } else {
                break;
            }
        }
    }
}

/// Central in-memory cache shared across HTTP worker threads.
pub struct ServerCache {
    /// Maps manga ID to its resolved title and disk directory path.
    pub manga_dirs: Mutex<HashMap<String, (String, PathBuf)>>,
    /// Maps chapter ID (`manga_id::chapter_num`) to its sorted list of page image filenames.
    pub chapter_pages: Mutex<HashMap<String, Vec<String>>>,
    /// Byte-bounded LRU cache for decompressed or fetched chapter page images.
    pub image_cache: Mutex<ImageLruCache>,
}

impl ServerCache {
    /// Creates a new `ServerCache` instance with the specified image memory cap.
    pub fn new(max_image_bytes: usize) -> Self {
        Self {
            manga_dirs: Mutex::new(HashMap::new()),
            chapter_pages: Mutex::new(HashMap::new()),
            image_cache: Mutex::new(ImageLruCache::new(max_image_bytes)),
        }
    }
}
