//! Generic in-memory log store with fixed capacity.
//!
//! Shared across modules (recipe, git, sandbox) for execution history.
//! Backed by a `Mutex<VecDeque<T>>` ring buffer — oldest entries are
//! evicted when capacity is reached.

use std::collections::VecDeque;
use std::sync::Mutex;

pub trait HasId {
    fn id(&self) -> &str;
}

#[derive(Debug)]
pub struct LogStore<T> {
    entries: Mutex<VecDeque<T>>,
    capacity: usize,
}

impl<T: HasId + Clone> LogStore<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub fn push(&self, entry: T) {
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    pub fn get(&self, id: &str) -> Option<T> {
        let entries = self.entries.lock().unwrap();
        entries.iter().find(|e| e.id() == id).cloned()
    }

    pub fn recent(&self, n: usize) -> Vec<T> {
        let entries = self.entries.lock().unwrap();
        entries.iter().rev().take(n).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone)]
    struct Entry {
        id: String,
        value: i32,
    }

    impl HasId for Entry {
        fn id(&self) -> &str {
            &self.id
        }
    }

    #[test]
    fn push_and_get() {
        let store = LogStore::new(10);
        store.push(Entry { id: "a".into(), value: 1 });
        store.push(Entry { id: "b".into(), value: 2 });
        assert_eq!(store.get("a").unwrap().value, 1);
        assert_eq!(store.get("b").unwrap().value, 2);
    }

    #[test]
    fn eviction_at_capacity() {
        let store = LogStore::new(2);
        store.push(Entry { id: "a".into(), value: 1 });
        store.push(Entry { id: "b".into(), value: 2 });
        store.push(Entry { id: "c".into(), value: 3 });
        assert!(store.get("a").is_none());
        assert_eq!(store.get("b").unwrap().value, 2);
        assert_eq!(store.get("c").unwrap().value, 3);
    }

    #[test]
    fn recent_newest_first() {
        let store = LogStore::new(10);
        store.push(Entry { id: "a".into(), value: 1 });
        store.push(Entry { id: "b".into(), value: 2 });
        store.push(Entry { id: "c".into(), value: 3 });
        let recent = store.recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, "c");
        assert_eq!(recent[1].id, "b");
    }

    #[test]
    fn get_missing_returns_none() {
        let store: LogStore<Entry> = LogStore::new(10);
        assert!(store.get("nonexistent").is_none());
    }
}
