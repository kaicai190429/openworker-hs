//! Small helpers the extension shares: clocks, UTF-8 cutting and a bounded map.
use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `2025-06-01T10:30:00.123Z` for a Unix time in milliseconds.
pub(crate) fn iso_from_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        ms % 1000
    )
}

pub(crate) fn iso_now() -> String {
    iso_from_ms(now_ms())
}

/// Cuts `text` to at most `max_bytes` of UTF-8 without splitting a character.
pub(crate) fn cut_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A map that forgets its oldest entry past a size, for per-turn bookkeeping.
pub(crate) struct BoundedMap<K, V> {
    max: usize,
    order: VecDeque<K>,
    map: HashMap<K, V>,
}

impl<K: Clone + Eq + Hash, V> BoundedMap<K, V> {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            max,
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }
    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }
    pub(crate) fn insert(&mut self, key: K, value: V) {
        if !self.map.contains_key(&key) {
            if self.map.len() >= self.max {
                if let Some(oldest) = self.order.pop_front() {
                    self.map.remove(&oldest);
                }
            }
            self.order.push_back(key.clone());
        }
        self.map.insert(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_time_is_rfc3339_utc() {
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_ms(1_748_773_800_123), "2025-06-01T10:30:00.123Z");
        assert_eq!(iso_from_ms(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn cutting_never_splits_a_character() {
        assert_eq!(cut_bytes("héllo", 2), "h");
        assert_eq!(cut_bytes("héllo", 3), "hé");
        assert_eq!(cut_bytes("日本語", 4), "日");
        assert_eq!(cut_bytes("abc", 10), "abc");
        assert_eq!(cut_bytes("abc", 0), "");
    }

    #[test]
    fn the_bounded_map_forgets_its_oldest_entry() {
        let mut m = BoundedMap::new(2);
        m.insert("a", 1);
        m.insert("b", 2);
        m.insert("c", 3);
        assert!(m.get(&"a").is_none() && m.get(&"b") == Some(&2) && m.get(&"c") == Some(&3));
        m.insert("b", 20);
        assert_eq!(m.get(&"b"), Some(&20));
    }
}
