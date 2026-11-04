pub mod image;
pub mod ttl;

pub use image::ImageLruCache;
pub use ttl::MangaTtlCache;

use crate::models::{Chapter, Manga};
use std::{collections::HashMap, path::PathBuf, sync::Mutex, time::Duration};

/// Central in-memory cache shared across HTTP worker threads.
pub struct ServerCache {
    /// Maps manga ID to its resolved title and disk directory path.
    pub manga_dirs: Mutex<HashMap<String, (String, PathBuf)>>,
    /// Maps chapter ID (`manga_id::chapter_num`) to its sorted list of page image filenames.
    pub chapter_pages: Mutex<HashMap<String, Vec<String>>>,
    /// Maps chapter ID (`manga_id::chapter_num`) to its resolved `.cbz` archive path on disk.
    pub chapter_cbz_paths: Mutex<HashMap<String, PathBuf>>,
    /// Time-to-live cache for upstream scraped manga metadata.
    pub remote_manga: MangaTtlCache,
    /// Byte-bounded LRU cache for decompressed or fetched chapter page images.
    pub image_cache: Mutex<ImageLruCache>,
}

impl ServerCache {
    /// Creates a new `ServerCache` instance with the specified image memory cap and metadata TTL.
    pub fn new(max_image_bytes: usize, ttl: Duration) -> Self {
        Self {
            manga_dirs: Mutex::new(HashMap::new()),
            chapter_pages: Mutex::new(HashMap::new()),
            chapter_cbz_paths: Mutex::new(HashMap::new()),
            remote_manga: MangaTtlCache::new(ttl),
            image_cache: Mutex::new(ImageLruCache::new(max_image_bytes)),
        }
    }

    /// Retrieves cached upstream `(Manga, Vec<Chapter>)` if not expired.
    pub fn get_remote_manga(&self, id: &str) -> Option<(Manga, Vec<Chapter>)> {
        self.remote_manga.get(id)
    }

    /// Stores upstream `(Manga, Vec<Chapter>)` in the TTL cache.
    pub fn put_remote_manga(&self, id: String, manga: Manga, chapters: Vec<Chapter>) {
        self.remote_manga.insert(id, manga, chapters);
    }
}
