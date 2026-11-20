//! A batch of key names in one arena: few allocations, stable memory, and
//! cheap sharing between writer threads.

use std::sync::Arc;

/// Keys stored back to back in `data`; key `i` is `data[starts[i]..starts[i + 1]]`.
/// `order` lists the keys in byte order after [`KeyBatch::normalise`], with
/// duplicates removed.
#[derive(Debug, Default, Clone)]
pub struct KeyBatch {
    data: Vec<u8>,
    starts: Vec<u32>,
    order: Vec<u32>,
}

impl KeyBatch {
    pub fn with_capacity(keys: usize, bytes: usize) -> Self {
        let mut starts = Vec::with_capacity(keys + 1);
        starts.push(0);
        KeyBatch {
            data: Vec::with_capacity(bytes),
            starts,
            order: Vec::new(),
        }
    }

    pub fn push(&mut self, key: &[u8]) {
        self.data.extend_from_slice(key);
        self.starts.push(self.data.len() as u32);
    }

    /// Number of keys pushed, duplicates included.
    pub fn len(&self) -> usize {
        self.starts.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes of key names held.
    pub fn bytes(&self) -> usize {
        self.data.len()
    }

    #[inline]
    pub fn key(&self, i: u32) -> &[u8] {
        let i = i as usize;
        &self.data[self.starts[i] as usize..self.starts[i + 1] as usize]
    }

    /// Sort and deduplicate; afterwards [`KeyBatch::sorted`] yields distinct
    /// keys in byte order.
    pub fn normalise(&mut self) {
        let mut order: Vec<u32> = (0..self.len() as u32).collect();
        order.sort_unstable_by(|&a, &b| self.key(a).cmp(self.key(b)));
        order.dedup_by(|a, b| self.key(*a) == self.key(*b));
        self.order = order;
    }

    /// Indices of distinct keys in byte order (empty before `normalise`).
    pub fn sorted(&self) -> &[u32] {
        &self.order
    }

    pub fn clear(&mut self) {
        self.data.clear();
        self.starts.truncate(1);
        self.order.clear();
    }

    pub fn into_shared(self) -> Arc<KeyBatch> {
        Arc::new(self)
    }
}

impl<'a> FromIterator<&'a [u8]> for KeyBatch {
    fn from_iter<T: IntoIterator<Item = &'a [u8]>>(iter: T) -> Self {
        let mut b = KeyBatch::with_capacity(0, 0);
        for k in iter {
            b.push(k);
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_round_trip_and_normalise() {
        let mut b = KeyBatch::with_capacity(4, 16);
        for k in [&b"b"[..], b"a", b"", b"b", b"ab"] {
            b.push(k);
        }
        assert_eq!(b.len(), 5);
        assert_eq!(b.key(2), b"");
        assert_eq!(b.key(4), b"ab");
        b.normalise();
        let sorted: Vec<&[u8]> = b.sorted().iter().map(|&i| b.key(i)).collect();
        assert_eq!(sorted, vec![&b""[..], b"a", b"ab", b"b"]);
        b.clear();
        assert!(b.is_empty());
        assert_eq!(b.bytes(), 0);
    }
}
