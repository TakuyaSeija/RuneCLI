//! Token usage tracking and persistence across sessions and time periods.
//!
//! Stores every usage update (tok_in, tok_out, cache_in, context_used, cost)
//! with timestamp and session reference, allowing aggregation by:
//! - session (realtime)
//! - hour
//! - day
//! - week
//! - month
//! - year
//! - all-time

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{now_ms, Db};
use crate::error::Result;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionUsage {
    pub unpriced_calls: u64,
    pub used: u64,
    pub tok_in: u64,
    pub tok_out: u64,
    pub cache_in: u64,
    pub spend: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Hour,
    Day,
    Week,
    Month,
    Year,
    AllTime,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AggregateUsage {
    pub tok_in: u64,
    pub tok_out: u64,
    pub cache_in: u64,
    pub spend: f64,
}

impl Db {
    /// Record one measured request for every role, using the engine's price result.
    ///
    /// `estimated_in` is what Rune's own estimate said this request would cost. It
    /// is stored beside the provider's real `input` so the two can be compared
    /// later: the provider owns the true token count, and the only honest way to
    /// know how far Rune's guess drifts is to keep both numbers. `0` means the
    /// request carried no estimate and must not be used as a calibration sample.
    #[allow(clippy::too_many_arguments)]
    pub fn record_usage_role(
        &self,
        session: i64,
        turn: i64,
        input: u64,
        output: u64,
        cached: u64,
        cost: Option<f64>,
        provider: &str,
        model: &str,
        role: &str,
        details: &crate::provider::UsageDetails,
        estimated_in: u64,
    ) -> Result<()> {
        if [input, output, cached]
            .into_iter()
            .any(|n| n > i64::MAX as u64)
            || cached > input
            || cost.is_some_and(|n| !n.is_finite() || n < 0.0)
        {
            return Err(crate::Error::invalid("invalid usage accounting values"));
        }
        crate::store::while_busy(|| {
            self.conn.execute("INSERT INTO token_usage (session_id,turn,timestamp,tok_in,tok_out,cache_in,context_used,cost,provider,model,role,details,cost_known,estimated_in) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![session,turn,now_ms(),input as i64,output as i64,cached as i64,if role=="agent"{input as i64}else{0},cost.unwrap_or(0.0),provider,model,role,serde_json::to_string(details)?,cost.is_some(),estimated_in.min(i64::MAX as u64) as i64])?;
            Ok(())
        })
    }

    /// How far Rune's own estimate drifts from the provider's real input, per model.
    ///
    /// Returns `real / estimated` summed over every recorded request that carried
    /// both a real count and an estimate — the provider's own numbers, never a
    /// ratio anyone wrote down. `None` when no comparable sample exists, so the
    /// caller keeps its own estimate rather than inventing a correction.
    pub fn estimate_correction(&self, provider: &str, model: &str) -> Result<Option<f64>> {
        let row = self.conn.query_row(
            "SELECT SUM(tok_in), SUM(estimated_in) FROM token_usage \
             WHERE provider=?1 AND model=?2 AND role='agent' AND estimated_in > 0 AND tok_in > 0 \
               AND (details IS NULL OR json_extract(details,'$.input_known')=1)",
            params![provider, model],
            |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
        )?;
        Ok(match row {
            (Some(real), Some(estimated)) if real > 0 && estimated > 0 => {
                Some(real as f64 / estimated as f64)
            }
            _ => None,
        })
    }

    /// Retrieve cumulative and latest token usage for a session.
    pub fn session_usage(&self, session_id: i64) -> Result<SessionUsage> {
        self.session_usage_before(session_id, None)
    }

    /// [`Db::session_usage`] as it stood before turn `before`: the rows of
    /// earlier turns only. A window that joins a running turn adds that turn's
    /// own requests from the events it replays, so the rows it has already
    /// written must not be counted a second time.
    pub fn session_usage_before(
        &self,
        session_id: i64,
        before: Option<i64>,
    ) -> Result<SessionUsage> {
        let mut usage = SessionUsage::default();
        let last_row = self.conn.query_row(
            "SELECT context_used FROM token_usage WHERE session_id = ?1 AND (?2 IS NULL OR turn < ?2) AND role = 'agent' AND (details IS NULL OR json_extract(details,'$.input_known')=1) ORDER BY id DESC LIMIT 1",
            params![session_id, before],
            |r| Ok(r.get::<_, i64>(0)? as u64),
        ).optional()?;
        if let Some(context_used) = last_row {
            usage.used = context_used;
        }
        let agg_row = self.conn.query_row(
            "SELECT COALESCE(SUM(tok_in), 0), COALESCE(SUM(tok_out), 0), COALESCE(SUM(cache_in), 0), COALESCE(SUM(cost), 0.0) FROM token_usage WHERE session_id = ?1 AND (?2 IS NULL OR turn < ?2)",
            params![session_id, before],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64, r.get::<_, f64>(3)?)),
        ).optional()?;
        if let Some((tok_in, tok_out, cache_in, spend)) = agg_row {
            usage.tok_in = tok_in;
            usage.tok_out = tok_out;
            usage.cache_in = cache_in;
            usage.spend = spend;
        }
        // What the sidebar calls "not priced" is a claim about Rune's own
        // accounting, so it must be about accounting: a request the provider
        // priced, or at least measured, that Rune could not put a price on.
        //
        // A request that never produced usage is a different fact — it failed,
        // was cancelled, or died mid-stream. Nothing was billed for it, so
        // counting it here would report a pricing gap where there was no
        // spending to price, and it would do so forever: the row is written
        // once and no later turn ever revises it, so a single early failure
        // would sit in the number for the life of the session.
        usage.unpriced_calls = self.conn.query_row(
            "SELECT COUNT(*) FROM token_usage WHERE session_id=?1 AND (?2 IS NULL OR turn < ?2) AND cost_known=0 \
               AND (tok_in > 0 OR tok_out > 0 \
                    OR json_extract(details,'$.input_known')=1 \
                    OR json_extract(details,'$.output_known')=1)",
            params![session_id, before],
            |r| r.get(0),
        )?;
        Ok(usage)
    }

    /// Aggregate token usage over a given time period (hour, day, week, month, year, all-time).
    pub fn aggregate_usage(&self, period: Period) -> Result<AggregateUsage> {
        let now = now_ms();
        let since = match period {
            Period::Hour => now.saturating_sub(3_600_000),
            Period::Day => now.saturating_sub(86_400_000),
            Period::Week => now.saturating_sub(7 * 86_400_000),
            Period::Month => now.saturating_sub(30 * 86_400_000),
            Period::Year => now.saturating_sub(365 * 86_400_000),
            Period::AllTime => 0,
        };
        let row = self.conn.query_row(
            "SELECT COALESCE(SUM(tok_in), 0), COALESCE(SUM(tok_out), 0), COALESCE(SUM(cache_in), 0), COALESCE(SUM(cost), 0.0)
             FROM token_usage WHERE timestamp >= ?1",
            params![since],
            |r| Ok(AggregateUsage {
                tok_in: r.get::<_, i64>(0)? as u64,
                tok_out: r.get::<_, i64>(1)? as u64,
                cache_in: r.get::<_, i64>(2)? as u64,
                spend: r.get::<_, f64>(3)?,
            }),
        )?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_query_session_usage() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/test").unwrap();
        db.record_usage_role(
            s.id,
            1,
            1000,
            150,
            200,
            Some(0.002),
            "p1",
            "m1",
            "agent",
            &crate::provider::UsageDetails {
                input_known: true,
                ..Default::default()
            },
            0,
        )
        .unwrap();
        db.record_usage_role(
            s.id,
            2,
            1200,
            250,
            400,
            Some(0.003),
            "p1",
            "m1",
            "agent",
            &crate::provider::UsageDetails {
                input_known: true,
                ..Default::default()
            },
            0,
        )
        .unwrap();

        let u = db.session_usage(s.id).unwrap();
        assert_eq!(u.tok_in, 2200);
        assert_eq!(u.cache_in, 600);
        assert_eq!(u.used, 1200);
        assert_eq!(u.tok_out, 400);
        assert!((u.spend - 0.005).abs() < 1e-6);

        let hour_agg = db.aggregate_usage(Period::Hour).unwrap();
        assert_eq!(hour_agg.tok_in, 2200);
        assert_eq!(hour_agg.tok_out, 400);
        assert_eq!(hour_agg.cache_in, 600);
        assert!((hour_agg.spend - 0.005).abs() < 1e-6);

        let all_agg = db.aggregate_usage(Period::AllTime).unwrap();
        assert_eq!(all_agg.tok_in, 2200);
    }

    #[test]
    fn no_sample_means_no_correction_rather_than_a_guess() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/test").unwrap();
        // A request with no estimate recorded is not a calibration sample.
        db.record_usage_role(
            s.id,
            1,
            1000,
            10,
            0,
            None,
            "p1",
            "m1",
            "agent",
            &crate::provider::UsageDetails {
                input_known: true,
                ..Default::default()
            },
            0,
        )
        .unwrap();
        assert_eq!(db.estimate_correction("p1", "m1").unwrap(), None);
        assert_eq!(db.estimate_correction("p1", "never-seen").unwrap(), None);
    }

    #[test]
    fn the_correction_is_the_providers_own_ratio_not_a_constant() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/test").unwrap();
        // Rune guessed 1,000; the provider measured 1,500.
        db.record_usage_role(
            s.id,
            1,
            1500,
            10,
            0,
            None,
            "p1",
            "m1",
            "agent",
            &crate::provider::UsageDetails {
                input_known: true,
                ..Default::default()
            },
            1000,
        )
        .unwrap();
        let r = db.estimate_correction("p1", "m1").unwrap().unwrap();
        assert!((r - 1.5).abs() < 1e-9, "expected 1.5, got {r}");

        // A different model with the opposite drift is scored on its own numbers.
        db.record_usage_role(
            s.id,
            2,
            800,
            10,
            0,
            None,
            "p2",
            "m2",
            "agent",
            &crate::provider::UsageDetails {
                input_known: true,
                ..Default::default()
            },
            1600,
        )
        .unwrap();
        let r2 = db.estimate_correction("p2", "m2").unwrap().unwrap();
        assert!((r2 - 0.5).abs() < 1e-9, "expected 0.5, got {r2}");
    }

    #[test]
    fn an_unknown_input_count_is_not_used_to_calibrate() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/test").unwrap();
        // input_known=false means the provider did not really report this count.
        db.record_usage_role(
            s.id,
            1,
            1500,
            10,
            0,
            None,
            "p1",
            "m1",
            "agent",
            &crate::provider::UsageDetails {
                input_known: false,
                ..Default::default()
            },
            1000,
        )
        .unwrap();
        assert_eq!(db.estimate_correction("p1", "m1").unwrap(), None);
    }

    #[test]
    fn a_non_agent_row_with_an_estimate_is_not_a_calibration_sample() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/test").unwrap();
        // Only the agent route reflects the request Rune is estimating; a
        // summariser or classifier call must never skew the factor.
        db.record_usage_role(
            s.id,
            1,
            9000,
            10,
            0,
            None,
            "p1",
            "m1",
            "compact",
            &crate::provider::UsageDetails {
                input_known: true,
                ..Default::default()
            },
            1000,
        )
        .unwrap();
        assert_eq!(db.estimate_correction("p1", "m1").unwrap(), None);
        // ...while an agent row on the same model does count.
        db.record_usage_role(
            s.id,
            2,
            1500,
            10,
            0,
            None,
            "p1",
            "m1",
            "agent",
            &crate::provider::UsageDetails {
                input_known: true,
                ..Default::default()
            },
            1000,
        )
        .unwrap();
        let r = db.estimate_correction("p1", "m1").unwrap().unwrap();
        assert!((r - 1.5).abs() < 1e-9, "only the agent row counts, got {r}");
    }
}
