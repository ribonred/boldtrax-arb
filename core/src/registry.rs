use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, RwLock};

use crate::types::{Exchange, Instrument, InstrumentKey, InstrumentType, Pairs};

#[derive(Debug, Clone, Default)]
pub struct InstrumentRegistry {
    inner: Arc<RwLock<InstrumentRegistryInner>>,
}

#[derive(Debug, Default)]
struct InstrumentRegistryInner {
    instruments: HashMap<InstrumentKey, Instrument>,
    by_exchange: HashMap<Exchange, Vec<InstrumentKey>>,
    by_pair: HashMap<Pairs, Vec<InstrumentKey>>,
    /// (exchange, exchange_symbol, instrument_type) → InstrumentKey for O(log n) lookup
    by_exchange_symbol: BTreeMap<(Exchange, String, InstrumentType), InstrumentKey>,
}

impl InstrumentRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(InstrumentRegistryInner::default())),
        }
    }

    pub fn insert(&self, instrument: Instrument) {
        let key = instrument.key;
        let mut inner = self.inner.write().expect("lock poisoned");

        // If it already exists, we don't need to update the indices
        if let Entry::Occupied(mut e) = inner.instruments.entry(key) {
            e.insert(instrument);
            return;
        }

        // Update by_exchange index
        inner.by_exchange.entry(key.exchange).or_default().push(key);

        // Update by_pair index
        inner.by_pair.entry(key.pair).or_default().push(key);

        // Update by_exchange_symbol index
        inner.by_exchange_symbol.insert(
            (
                key.exchange,
                instrument.exchange_symbol.clone(),
                key.instrument_type,
            ),
            key,
        );

        // Insert instrument
        inner.instruments.insert(key, instrument);
    }

    pub fn insert_batch(&self, instruments: Vec<Instrument>) {
        let mut inner = self.inner.write().expect("lock poisoned");

        for instrument in instruments {
            let key = instrument.key;

            // If it already exists, we don't need to update the indices
            if let Entry::Occupied(mut e) = inner.instruments.entry(key) {
                e.insert(instrument);
                continue;
            }

            // Update by_exchange index
            inner.by_exchange.entry(key.exchange).or_default().push(key);

            // Update by_pair index
            inner.by_pair.entry(key.pair).or_default().push(key);

            // Update by_exchange_symbol index
            inner.by_exchange_symbol.insert(
                (
                    key.exchange,
                    instrument.exchange_symbol.clone(),
                    key.instrument_type,
                ),
                key,
            );

            // Insert instrument
            inner.instruments.insert(key, instrument);
        }
    }

    pub fn get(&self, key: &InstrumentKey) -> Option<Instrument> {
        let inner = self.inner.read().expect("lock poisoned");
        inner.instruments.get(key).cloned()
    }

    pub fn get_by_parts(
        &self,
        exchange: Exchange,
        pair: Pairs,
        instrument_type: InstrumentType,
    ) -> Option<Instrument> {
        let key = InstrumentKey {
            exchange,
            pair,
            instrument_type,
        };
        self.get(&key)
    }

    pub fn get_by_pair(&self, pair: Pairs) -> Vec<Instrument> {
        let inner = self.inner.read().expect("lock poisoned");
        inner
            .by_pair
            .get(&pair)
            .map(|keys| {
                keys.iter()
                    .filter_map(|k| inner.instruments.get(k).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn get_by_exchange(&self, exchange: Exchange) -> Vec<Instrument> {
        let inner = self.inner.read().expect("lock poisoned");
        inner
            .by_exchange
            .get(&exchange)
            .map(|keys| {
                keys.iter()
                    .filter_map(|k| inner.instruments.get(k).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn get_all(&self) -> Vec<Instrument> {
        let inner = self.inner.read().expect("lock poisoned");
        inner.instruments.values().cloned().collect()
    }

    pub fn contains(&self, key: &InstrumentKey) -> bool {
        let inner = self.inner.read().expect("lock poisoned");
        inner.instruments.contains_key(key)
    }

    pub fn len(&self) -> usize {
        let inner = self.inner.read().expect("lock poisoned");
        inner.instruments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get_by_exchange_symbol(
        &self,
        exchange: Exchange,
        symbol: &str,
        instrument_type: InstrumentType,
    ) -> Option<Instrument> {
        let inner = self.inner.read().expect("lock poisoned");
        let key = inner
            .by_exchange_symbol
            .get(&(exchange, symbol.to_string(), instrument_type))?;
        inner.instruments.get(key).cloned()
    }

    pub fn clear(&self) {
        let mut inner = self.inner.write().expect("lock poisoned");
        inner.instruments.clear();
        inner.by_exchange.clear();
        inner.by_pair.clear();
        inner.by_exchange_symbol.clear();
    }
}
