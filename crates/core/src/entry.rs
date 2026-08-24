//! Normalized usage record and the mutable accumulator used by the aggregations.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::fmt;

/// One usage record, normalized across providers. `total == input + output + cache_write + cache_read`.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Stable identity used for cross-file dedup (provider-specific scheme).
    pub id: String,
    /// Original event instant (UTC).
    pub date: DateTime<Utc>,
    /// Local calendar day (`yyyy-MM-dd`) the record is charged to.
    pub local_day: String,
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub cache_write: i64,
    pub cache_read: i64,
    /// Exact charge persisted by the source, when present — preferred over table pricing.
    pub explicit_cost: Option<f64>,
}

impl Entry {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_write + self.cache_read
    }
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}({}) in={} out={} cw={} cr={}",
            self.id, self.local_day, self.input, self.output, self.cache_write, self.cache_read
        )
    }
}

/// Running total with cost, mirroring the Swift `Bucket`.
#[derive(Debug, Default)]
pub struct Bucket {
    pub input: i64,
    pub output: i64,
    pub cache_write: i64,
    pub cache_read: i64,
    pub cost: f64,
}

impl Bucket {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_write + self.cache_read
    }

    /// Add an entry. A positive `explicit_cost` wins over table pricing (source of truth);
    /// a non-positive one is ignored and the model table is used instead.
    pub fn add(&mut self, e: &Entry) {
        self.input += e.input;
        self.output += e.output;
        self.cache_write += e.cache_write;
        self.cache_read += e.cache_read;
        let cost = match e.explicit_cost {
            Some(c) if c > 0.0 => c,
            _ => crate::cost::ModelPricing::cost(
                &e.model,
                e.input,
                e.output,
                e.cache_write,
                e.cache_read,
            ),
        };
        self.cost += cost;
    }
}

/// Global dedup by `id`, keeping the entry with the largest `total` (the completed form of a
/// re-logged message). Preserves first-seen order for stable output.
pub fn dedup_keep_max(entries: Vec<Entry>) -> Vec<Entry> {
    let mut by_id: HashMap<String, Entry> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for e in entries {
        let id = e.id.clone();
        let keep = match by_id.get(&id) {
            Some(ex) => e.total() > ex.total(),
            None => {
                order.push(id.clone());
                true
            }
        };
        if keep {
            by_id.insert(id, e);
        }
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
}
