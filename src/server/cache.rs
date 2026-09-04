use lru::LruCache;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub struct ImageLruCache {
    max_bytes: usize,
    current_bytes: usize,
    cache: LruCache<String, Arc<[u8]>>,
}

impl ImageLruCache {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            current_bytes: 0,
            cache: LruCache::unbounded(),
        }
    }

    pub fn get(&mut self, key: &str) -> Option<Arc<[u8]>> {
        self.cache.get(key).cloned()
    }

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

pub struct ServerCache {
    pub manga_dirs: Mutex<HashMap<String, (String, PathBuf)>>,
    pub chapter_pages: Mutex<HashMap<String, Vec<String>>>,
    pub image_cache: Mutex<ImageLruCache>,
}

impl ServerCache {
    pub fn new(max_image_bytes: usize) -> Self {
        Self {
            manga_dirs: Mutex::new(HashMap::new()),
            chapter_pages: Mutex::new(HashMap::new()),
            image_cache: Mutex::new(ImageLruCache::new(max_image_bytes)),
        }
    }
}
