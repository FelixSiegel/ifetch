use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub struct ImageLruCache {
    max_bytes: usize,
    current_bytes: usize,
    entries: HashMap<String, Arc<[u8]>>,
    order: VecDeque<(String, usize)>,
}

impl ImageLruCache {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            current_bytes: 0,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<Arc<[u8]>> {
        self.entries.get(key).cloned()
    }

    pub fn insert(&mut self, key: String, data: Arc<[u8]>) {
        let size = data.len();
        if size > self.max_bytes {
            return;
        }

        if let Some(existing) = self.entries.remove(&key) {
            self.current_bytes = self.current_bytes.saturating_sub(existing.len());
            self.order.retain(|(k, _)| k != &key);
        }

        while self.current_bytes + size > self.max_bytes {
            if let Some((old_key, old_size)) = self.order.pop_front() {
                self.entries.remove(&old_key);
                self.current_bytes = self.current_bytes.saturating_sub(old_size);
            } else {
                break;
            }
        }

        self.current_bytes += size;
        self.entries.insert(key.clone(), data);
        self.order.push_back((key, size));
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
