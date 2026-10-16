//! Checks the context design's promise on the bill: a request that only extends
//! the previous one should read most of it from the provider's cache.
//!
//! Only consecutive requests of one turn are compared, and only for a model that
//! has read from the cache before in this session — a provider that never
//! caches, or a gap long enough for any provider's cache to expire, is not a
//! silent invalidator and must not raise an alarm.

use std::collections::HashSet;

#[derive(Default)]
pub struct CacheWatch {
    proven: HashSet<(String, String)>,
}

impl CacheWatch {
    /// `warm` says the previous request of this turn went to the same model and
    /// nothing sanctioned (compaction) reset the prefix since.
    pub fn observe(
        &mut self,
        provider: &str,
        model: &str,
        warm: bool,
        input: u64,
        cached: u64,
    ) -> Option<String> {
        let key = (provider.to_string(), model.to_string());
        if cached > 0 {
            self.proven.insert(key);
            return None;
        }
        (warm && input >= MIN_CACHEABLE && self.proven.contains(&key)).then(|| {
            format!(
                "Prompt cache missed: {input} input tokens on {model} were billed without a cache read, although this request only extended the previous one."
            )
        })
    }
}

/// Below this, providers do not cache a prefix at all, so a miss means nothing.
/// Rune's own threshold for raising the alarm, not a vendor's figure.
const MIN_CACHEABLE: u64 = 4096;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_miss_is_reported_only_where_a_hit_was_due() {
        let mut watch = CacheWatch::default();
        assert!(
            watch.observe("p", "m", true, 50_000, 0).is_none(),
            "never proven to cache"
        );
        assert!(watch.observe("p", "m", true, 50_000, 40_000).is_none());
        assert!(
            watch.observe("p", "m", false, 50_000, 0).is_none(),
            "a cold start"
        );
        assert!(
            watch.observe("p", "m", true, 100, 0).is_none(),
            "too small to cache"
        );
        assert!(watch.observe("p", "other", true, 50_000, 0).is_none());
        assert!(watch.observe("p", "m", true, 50_000, 0).is_some());
    }
}
