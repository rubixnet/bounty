//! The hard monthly Jev spend limit (spec S4).
//!
//! Money is counted in integer pico-dollars (1e-12 USD), so $0.042 per
//! million tokens is exactly 42,000 per token. Before a submission is sent,
//! its upper-bound estimate is **reserved**; a reservation that would take
//! `spent + reserved` over the limit is refused and nothing is sent. Each
//! reply then settles its request: the reservation is released and the
//! billed `usage.input_tokens` is added to the month's spend, which is
//! written to a small JSON file (write-then-rename) so a restart cannot
//! reset it. A new UTC month starts from zero.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::partition::Month;

const PICO_PER_USD: f64 = 1e12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpendError {
    /// The reservation would cross the monthly limit.
    LimitReached,
    /// The ledger file could not be written; spending stops.
    Ledger,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerFile {
    /// `YYYY-MM`, UTC.
    month: String,
    spent_pico_usd: u64,
}

#[derive(Debug)]
struct State {
    month: String,
    spent: u64,
    reserved: u64,
}

#[derive(Debug)]
pub struct SpendLedger {
    path: PathBuf,
    limit: u64,
    price_per_token: u64,
    state: Mutex<State>,
}

fn month_key(now_ms: u64) -> String {
    let m = Month::of_ms(now_ms);
    format!("{:04}-{:02}", m.year, m.month)
}

impl SpendLedger {
    /// Opens (or starts) the ledger. An unreadable or corrupt file is an
    /// error: the server refuses to start rather than forget its spend.
    pub fn open(
        path: &Path,
        limit_usd: f64,
        price_usd_per_million_tokens: f64,
        now_ms: u64,
    ) -> Result<Self, String> {
        let (month, spent) = match std::fs::read(path) {
            Ok(bytes) => {
                let file: LedgerFile = serde_json::from_slice(&bytes)
                    .map_err(|_| format!("the Jev spend ledger {} is corrupt", path.display()))?;
                (file.month, file.spent_pico_usd)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (month_key(now_ms), 0),
            Err(e) => return Err(format!("cannot read the Jev spend ledger: {e}")),
        };
        Ok(Self {
            path: path.to_owned(),
            limit: (limit_usd * PICO_PER_USD).floor() as u64,
            price_per_token: (price_usd_per_million_tokens * 1e6).round() as u64,
            state: Mutex::new(State {
                month,
                spent,
                reserved: 0,
            }),
        })
    }

    /// The cost of `tokens` input tokens, in pico-dollars.
    pub fn cost(&self, tokens: u64) -> u64 {
        tokens.saturating_mul(self.price_per_token)
    }

    fn lock(&self, now_ms: u64) -> std::sync::MutexGuard<'_, State> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let month = month_key(now_ms);
        if state.month != month {
            state.month = month;
            state.spent = 0;
        }
        state
    }

    /// Reserves `pico` for requests about to be sent.
    pub fn reserve(&self, now_ms: u64, pico: u64) -> Result<(), SpendError> {
        let mut state = self.lock(now_ms);
        let after = state
            .spent
            .saturating_add(state.reserved)
            .saturating_add(pico);
        if after > self.limit {
            return Err(SpendError::LimitReached);
        }
        state.reserved += pico;
        Ok(())
    }

    /// Releases an unused reservation.
    pub fn release(&self, now_ms: u64, pico: u64) {
        let mut state = self.lock(now_ms);
        state.reserved = state.reserved.saturating_sub(pico);
    }

    /// Settles one request: releases its reservation and books what it cost.
    pub fn settle(&self, now_ms: u64, reserved: u64, billed: u64) -> Result<(), SpendError> {
        let mut state = self.lock(now_ms);
        state.reserved = state.reserved.saturating_sub(reserved);
        state.spent = state.spent.saturating_add(billed);
        let file = LedgerFile {
            month: state.month.clone(),
            spent_pico_usd: state.spent,
        };
        let bytes = serde_json::to_vec(&file).map_err(|_| SpendError::Ledger)?;
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, bytes).map_err(|_| SpendError::Ledger)?;
        std::fs::rename(&tmp, &self.path).map_err(|_| SpendError::Ledger)
    }

    /// The current month's spend in USD.
    pub fn spent_usd(&self, now_ms: u64) -> f64 {
        self.lock(now_ms).spent as f64 / PICO_PER_USD
    }
}
