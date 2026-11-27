//! An interned string table kept in one buffer: every string's bytes back to back,
//! plus where each one ends.
//!
//! The build state file stores a table the same way, so loading one is a single
//! copy of its bytes and a single UTF-8 check, rather than an allocation and a copy
//! per string. That matters most on static musl, whose `memcpy` is slow for small
//! copies, but it saves the allocations everywhere. Lookups hash the string and
//! compare it once against the table; nothing is copied.

use std::hash::BuildHasher;

use hashbrown::HashTable;
use rustc_hash::FxBuildHasher;

/// Strings by id, ids by string.
#[derive(Debug, Default, Clone)]
pub struct StrTable {
    text: String,
    /// Where each string ends in `text`; string `i` starts where `i - 1` ends.
    ends: Vec<u32>,
    /// Each string's id, by the string's hash.
    index: HashTable<u32>,
}

impl StrTable {
    /// A table from its parts, as the state file stores them. `None` unless every
    /// end is in order and on a character boundary, or if two strings are the same.
    pub(super) fn from_parts(text: String, ends: Vec<u32>) -> Option<Self> {
        let mut table = Self {
            text,
            ends,
            index: HashTable::new(),
        };
        let mut start = 0;
        for &end in &table.ends {
            let end = end as usize;
            if end < start || !table.text.is_char_boundary(end) {
                return None;
            }
            start = end;
        }
        if start != table.text.len() {
            return None;
        }
        // Room for every id up front, so inserting never needs to rehash.
        table.index.reserve(table.ends.len(), |_| 0);
        for id in 0..table.len() {
            let text = table.get(id);
            let hash = hash(text);
            if table.find_hashed(text, hash).is_some() {
                return None;
            }
            table.index.insert_unique(hash, id, |_| 0);
        }
        Some(table)
    }

    /// The bytes of every string, back to back, and where each ends.
    pub(super) fn parts(&self) -> (&str, &[u32]) {
        (&self.text, &self.ends)
    }

    /// How many strings there are; ids are `0..len()`.
    #[must_use]
    pub fn len(&self) -> u32 {
        u32::try_from(self.ends.len()).unwrap_or(u32::MAX)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// String `id`.
    ///
    /// # Panics
    ///
    /// If `id` isn't in the table.
    #[must_use]
    pub fn get(&self, id: u32) -> &str {
        slice(&self.text, &self.ends, id)
    }

    /// The id of `text`, if it's in the table.
    #[must_use]
    pub fn find(&self, text: &str) -> Option<u32> {
        self.find_hashed(text, hash(text))
    }

    /// The id of `text`, adding it if it's new. Returns whether it was added too.
    pub fn intern(&mut self, text: &str) -> (u32, bool) {
        let hash = hash(text);
        if let Some(id) = self.find_hashed(text, hash) {
            return (id, false);
        }
        let id = self.len();
        self.text.push_str(text);
        self.ends
            .push(u32::try_from(self.text.len()).unwrap_or(u32::MAX));
        let Self {
            text: all,
            ends,
            index,
        } = self;
        index.insert_unique(hash, id, |&id| self::hash(slice(all, ends, id)));
        (id, true)
    }

    fn find_hashed(&self, text: &str, hash: u64) -> Option<u32> {
        self.index.find(hash, |&id| self.get(id) == text).copied()
    }
}

/// String `id` of a table's parts, for rehashing while the table is borrowed.
fn slice<'a>(text: &'a str, ends: &[u32], id: u32) -> &'a str {
    let i = id as usize;
    let start = if i == 0 { 0 } else { ends[i - 1] as usize };
    &text[start..ends[i] as usize]
}

fn hash(text: &str) -> u64 {
    FxBuildHasher.hash_one(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_finds_and_round_trips() {
        let mut table = StrTable::default();
        assert_eq!(table.intern("src/a.h"), (0, true));
        assert_eq!(table.intern("src/é.h"), (1, true));
        assert_eq!(table.intern("src/a.h"), (0, false));
        assert_eq!(table.find("src/é.h"), Some(1));
        assert_eq!(table.find("src/b.h"), None);
        // Enough strings to make the index grow and rehash.
        for i in 0..1000 {
            table.intern(&format!("src/{i}.h"));
        }
        assert_eq!(table.find("src/999.h"), Some(1001));

        let (text, ends) = table.parts();
        let copy = StrTable::from_parts(text.to_string(), ends.to_vec()).unwrap();
        assert_eq!(copy.len(), table.len());
        assert_eq!(copy.get(1), "src/é.h");
        assert_eq!(copy.find("src/500.h"), Some(502));
    }

    #[test]
    fn rejects_bad_parts() {
        let text = || "aéb".to_string();
        assert!(StrTable::from_parts(text(), vec![1, 3, 4]).is_some());
        // Inside `é`, out of order, past the end, short of the end, a duplicate.
        assert!(StrTable::from_parts(text(), vec![2, 4]).is_none());
        assert!(StrTable::from_parts(text(), vec![3, 1, 4]).is_none());
        assert!(StrTable::from_parts(text(), vec![1, 5]).is_none());
        assert!(StrTable::from_parts(text(), vec![1, 3]).is_none());
        assert!(StrTable::from_parts("aa".into(), vec![1, 2]).is_none());
    }
}
