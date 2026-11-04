use crate::models::{Chapter, Manga};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Cached upstream scrape result for a manga and its chapter list.
#[derive(Clone)]
struct CachedRemoteManga {
    manga: Manga,
    chapters: Vec<Chapter>,
    fetched_at: Instant,
}

/// Time-to-live (TTL) cache for scraped manga metadata and chapter listings.
pub struct MangaTtlCache {
    ttl: Duration,
    entries: Mutex<HashMap<String, CachedRemoteManga>>,
}

impl MangaTtlCache {
    /// Creates a new `MangaTtlCache` with the given TTL duration.
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Retrieves cached upstream `(Manga, Vec<Chapter>)` if not expired.
    pub fn get(&self, id: &str) -> Option<(Manga, Vec<Chapter>)> {
        let mut map = crate::server::helpers::lock_mutex(&self.entries);
        if let Some(entry) = map.get(id) {
            if entry.fetched_at.elapsed() <= self.ttl {
                return Some((entry.manga.clone(), entry.chapters.clone()));
            }
            map.remove(id);
        }
        None
    }

    /// Stores upstream `(Manga, Vec<Chapter>)` in the TTL cache.
    pub fn insert(&self, id: String, manga: Manga, chapters: Vec<Chapter>) {
        let mut map = crate::server::helpers::lock_mutex(&self.entries);
        if map.len() > 256 {
            let ttl = self.ttl;
            map.retain(|_, v| v.fetched_at.elapsed() <= ttl);
        }
        map.insert(
            id,
            CachedRemoteManga {
                manga,
                chapters,
                fetched_at: Instant::now(),
            },
        );
    }
}
