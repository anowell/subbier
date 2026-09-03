//! Time series from `state.db`, shaped for a chart. Allowance samples and
//! proxied traffic are two types so they cannot be plotted on one axis. Absence
//! is not zero: an allowance bucket with no sample means nobody polled (a short
//! gap holds the last value), while an empty throughput bucket genuinely is 0.

use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp};

use crate::model::{SubKey, WindowKind};
use crate::proxy::pool_from_path;
use crate::store::db::{AllowanceRow, ProxiedBucket};

/// How far a resampled allowance series carries the last known value across a
/// gap before reporting `None`. Generous next to the poll cadence so a missed
/// poll does not punch a hole in the line.
pub const MAX_HOLD: SignedDuration = SignedDuration::from_mins(30);

/// A proxy endpoint: the bare proxy, or one pool.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Endpoint {
    Default,
    Pool(String),
}

impl Endpoint {
    /// Which endpoint served a stored `route`.
    #[must_use]
    pub fn of_route(route: &str) -> Self {
        pool_from_path(route).map_or(Endpoint::Default, |p| Endpoint::Pool(p.to_owned()))
    }

    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Endpoint::Default => "default",
            Endpoint::Pool(name) => name,
        }
    }
}

/// One irregular series, oldest first: the timestamps are whenever a poll landed
/// or a request finished, so they are neither evenly spaced nor aligned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Series {
    pub points: Vec<(Timestamp, f64)>,
}

impl Series {
    /// `buckets` evenly spaced samples over `since..until`, each the mean of the
    /// points falling in it, or `None` where none did. The mean, not the last
    /// value, so the line's shape does not depend on the pane's width.
    fn resample(&self, since: Timestamp, until: Timestamp, buckets: usize) -> Vec<Option<f64>> {
        let mut sums = vec![(0.0f64, 0usize); buckets];
        if buckets == 0 {
            return Vec::new();
        }
        let span = (until.as_millisecond() - since.as_millisecond()).max(1);
        for (ts, value) in &self.points {
            let offset = ts.as_millisecond() - since.as_millisecond();
            if offset < 0 || *ts > until {
                continue;
            }
            // A point landing exactly on `until` belongs in the last bucket.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let idx = ((offset as i128 * buckets as i128) / span as i128) as usize;
            let slot = &mut sums[idx.min(buckets - 1)];
            slot.0 += value;
            slot.1 += 1;
        }
        sums.into_iter()
            .map(|(sum, n)| (n > 0).then(|| sum / n as f64))
            .collect()
    }

    /// Resampled, with each empty bucket filled from the last known value for up
    /// to `hold`. A longer gap stays `None` so the caller can break the line.
    #[must_use]
    pub fn resample_held(
        &self,
        since: Timestamp,
        until: Timestamp,
        buckets: usize,
        hold: SignedDuration,
    ) -> Vec<Option<f64>> {
        let mut out = self.resample(since, until, buckets);
        if buckets == 0 {
            return out;
        }
        let span = (until.as_millisecond() - since.as_millisecond()).max(1);
        #[allow(clippy::cast_possible_truncation)]
        let bucket_ms = span / buckets as i64;
        let max_run = if bucket_ms > 0 {
            usize::try_from(hold.as_millis() / i128::from(bucket_ms)).unwrap_or(usize::MAX)
        } else {
            usize::MAX
        };

        let mut last: Option<f64> = None;
        let mut run = 0usize;
        for slot in &mut out {
            match *slot {
                Some(v) => {
                    last = Some(v);
                    run = 0;
                }
                None => {
                    run += 1;
                    if run <= max_run {
                        *slot = last;
                    } else {
                        last = None;
                    }
                }
            }
        }
        out
    }
}

/// Allowance percentages over time, one series per (sub, window), on the same
/// `0..=100` scale as [`crate::snapshot::WindowView::pct`].
#[derive(Debug, Clone, Default)]
pub struct AllowanceHistory {
    series: BTreeMap<(SubKey, WindowKind), Series>,
}

impl AllowanceHistory {
    #[must_use]
    pub fn from_rows(rows: Vec<AllowanceRow>) -> Self {
        let mut series: BTreeMap<(SubKey, WindowKind), Series> = BTreeMap::new();
        for row in rows {
            series
                .entry((row.sub, row.window))
                .or_default()
                .points
                .push((row.ts, f64::from(row.pct)));
        }
        Self { series }
    }

    #[must_use]
    pub fn get(&self, sub: &SubKey, window: &WindowKind) -> Option<&Series> {
        self.series
            .iter()
            .find(|((s, w), _)| s == sub && w == window)
            .map(|(_, series)| series)
    }
}

/// Proxy-observed traffic over time, counted at the endpoint that served it and
/// never summed over its members, who serve other endpoints too.
#[derive(Debug, Clone, Default)]
pub struct Throughput {
    tokens: BTreeMap<Endpoint, Series>,
    requests: BTreeMap<Endpoint, Series>,
    /// The bucket width the rows were summed over; turns a total into a rate.
    bucket: SignedDuration,
}

impl Throughput {
    /// `bucket` must be the same width [`crate::store::db::Db::proxied_since`]
    /// was given.
    #[must_use]
    pub fn from_buckets(buckets: Vec<ProxiedBucket>, bucket: SignedDuration) -> Self {
        let mut tokens: BTreeMap<Endpoint, Vec<(Timestamp, f64)>> = BTreeMap::new();
        let mut requests: BTreeMap<Endpoint, Vec<(Timestamp, f64)>> = BTreeMap::new();
        // Several routes share one endpoint, so a bucket can appear twice.
        let mut token_acc: BTreeMap<(Endpoint, Timestamp), f64> = BTreeMap::new();
        let mut request_acc: BTreeMap<(Endpoint, Timestamp), f64> = BTreeMap::new();
        for row in buckets {
            let endpoint = Endpoint::of_route(&row.route);
            #[allow(clippy::cast_precision_loss)]
            {
                *token_acc.entry((endpoint.clone(), row.ts)).or_default() += row.tokens as f64;
                *request_acc.entry((endpoint, row.ts)).or_default() += f64::from(row.requests);
            }
        }
        for ((endpoint, ts), v) in token_acc {
            tokens.entry(endpoint).or_default().push((ts, v));
        }
        for ((endpoint, ts), v) in request_acc {
            requests.entry(endpoint).or_default().push((ts, v));
        }
        Self {
            tokens: tokens
                .into_iter()
                .map(|(k, points)| (k, Series { points }))
                .collect(),
            requests: requests
                .into_iter()
                .map(|(k, points)| (k, Series { points }))
                .collect(),
            bucket,
        }
    }

    fn get(&self, endpoint: &Endpoint, metric: Metric) -> Option<&Series> {
        match metric {
            Metric::Tokens => self.tokens.get(endpoint),
            Metric::Requests => self.requests.get(endpoint),
        }
    }

    /// A resampled series as a rate, empty buckets filled with `0.0`. A rate
    /// rather than a per-bucket total so widening the range does not multiply
    /// the numbers on the axis.
    #[must_use]
    pub fn rate(
        &self,
        endpoint: &Endpoint,
        metric: Metric,
        since: Timestamp,
        until: Timestamp,
        buckets: usize,
        rate: Rate,
    ) -> Vec<f64> {
        let Some(series) = self.get(endpoint, metric) else {
            return vec![0.0; buckets];
        };
        let per = (self.bucket.as_secs_f64() / rate.per().as_secs_f64()).max(f64::MIN_POSITIVE);
        series
            .resample(since, until, buckets)
            .into_iter()
            .map(|v| v.unwrap_or(0.0) / per)
            .collect()
    }
}

/// The denominator a throughput rate is quoted in. Which one is a property of
/// the span, not a preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rate {
    PerMinute,
    PerHour,
}

impl Rate {
    fn per(self) -> SignedDuration {
        match self {
            Rate::PerMinute => SignedDuration::from_mins(1),
            Rate::PerHour => SignedDuration::from_hours(1),
        }
    }
}

/// Which proxy-observed quantity a throughput chart is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Tokens,
    Requests,
}

impl Metric {
    /// `"tok/min"`, `"req/hr"`.
    #[must_use]
    pub fn unit(self, rate: Rate) -> &'static str {
        match (self, rate) {
            (Metric::Tokens, Rate::PerMinute) => "tok/min",
            (Metric::Tokens, Rate::PerHour) => "tok/hr",
            (Metric::Requests, Rate::PerMinute) => "req/min",
            (Metric::Requests, Rate::PerHour) => "req/hr",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Provider;

    fn t(secs: i64) -> Timestamp {
        Timestamp::from_second(1_700_000_000 + secs).unwrap()
    }

    fn key(n: u8) -> SubKey {
        SubKey::new(Provider::Codex, format!("acct-{n}"))
    }

    fn series(points: &[(i64, f64)]) -> Series {
        Series {
            points: points.iter().map(|(s, v)| (t(*s), *v)).collect(),
        }
    }

    #[test]
    fn an_endpoint_comes_from_the_stored_route_and_nowhere_else() {
        assert_eq!(Endpoint::of_route("/v1/messages"), Endpoint::Default);
        assert_eq!(
            Endpoint::of_route("/pool/moonshot/v1/messages"),
            Endpoint::Pool("moonshot".to_owned())
        );
        assert_eq!(Endpoint::Default.name(), "default");
    }

    #[test]
    fn resampling_averages_each_bucket_and_ignores_what_is_outside() {
        let s = series(&[(0, 10.0), (10, 20.0), (25, 50.0), (95, 90.0)]);
        assert_eq!(
            s.resample(t(0), t(100), 5),
            vec![Some(15.0), Some(50.0), None, None, Some(90.0)]
        );

        let edges = series(&[(-50, 1.0), (0, 2.0), (100, 3.0), (150, 4.0)]);
        assert_eq!(edges.resample(t(0), t(100), 2), vec![Some(2.0), Some(3.0)]);
        assert!(edges.resample(t(0), t(100), 0).is_empty());
        assert_eq!(series(&[]).resample(t(0), t(100), 3), vec![None; 3]);
    }

    #[test]
    fn a_held_resample_carries_a_short_gap_and_gives_up_on_a_long_one() {
        let s = series(&[(0, 42.0), (595, 44.0)]);
        assert_eq!(
            s.resample_held(t(0), t(600), 10, SignedDuration::from_mins(3)),
            vec![
                Some(42.0),
                Some(42.0),
                Some(42.0),
                Some(42.0),
                None,
                None,
                None,
                None,
                None,
                Some(44.0),
            ]
        );

        let dense = series(&[(0, 1.0), (30, 2.0), (60, 3.0)]);
        assert_eq!(
            dense.resample_held(t(0), t(90), 3, MAX_HOLD),
            dense.resample(t(0), t(90), 3),
            "with no gaps it is just a resample"
        );
    }

    #[test]
    fn allowance_history_groups_by_sub_and_window() {
        let row = |ts, sub, window, pct| AllowanceRow {
            ts: t(ts),
            sub,
            window,
            pct,
        };
        let history = AllowanceHistory::from_rows(vec![
            row(0, key(1), WindowKind::Weekly, 10.0),
            row(60, key(1), WindowKind::Weekly, 12.0),
            row(0, key(1), WindowKind::Session, 50.0),
            row(0, key(2), WindowKind::Weekly, 90.0),
        ]);
        assert_eq!(
            history.get(&key(1), &WindowKind::Weekly).unwrap().points,
            vec![(t(0), 10.0), (t(60), 12.0)]
        );
        assert!(history.get(&key(2), &WindowKind::Session).is_none());
    }

    fn bucket(ts: i64, route: &str, requests: u32, tokens: u64) -> ProxiedBucket {
        ProxiedBucket {
            ts: t(ts),
            route: route.to_owned(),
            requests,
            tokens,
        }
    }

    #[test]
    fn throughput_folds_every_route_of_an_endpoint_together() {
        let tp = Throughput::from_buckets(
            vec![
                bucket(0, "/pool/moonshot/v1/messages", 2, 1_000),
                bucket(0, "/pool/moonshot/v1/responses", 3, 2_000),
                bucket(0, "/v1/messages", 1, 500),
            ],
            SignedDuration::from_mins(1),
        );
        let pool = Endpoint::Pool("moonshot".to_owned());
        assert_eq!(
            tp.get(&pool, Metric::Tokens).unwrap().points,
            vec![(t(0), 3_000.0)]
        );
        assert_eq!(
            tp.get(&pool, Metric::Requests).unwrap().points,
            vec![(t(0), 5.0)]
        );
        assert_eq!(
            tp.get(&Endpoint::Default, Metric::Tokens).unwrap().points,
            vec![(t(0), 500.0)]
        );
    }

    #[test]
    fn a_throughput_gap_is_zero_and_the_rate_is_normalised() {
        let rate_of = |tp: &Throughput, until, buckets, rate| {
            tp.rate(
                &Endpoint::Default,
                Metric::Tokens,
                t(0),
                until,
                buckets,
                rate,
            )
        };
        let minutely = Throughput::from_buckets(
            vec![
                bucket(0, "/v1/messages", 4, 1_200),
                bucket(120, "/v1/messages", 2, 600),
            ],
            SignedDuration::from_mins(1),
        );
        assert_eq!(
            rate_of(&minutely, t(180), 3, Rate::PerMinute),
            vec![1_200.0, 0.0, 600.0]
        );
        assert_eq!(
            rate_of(&minutely, t(180), 3, Rate::PerHour),
            vec![72_000.0, 0.0, 36_000.0],
            "sixty times the number, same shape"
        );

        let five_minutely = Throughput::from_buckets(
            vec![bucket(0, "/v1/messages", 10, 6_000)],
            SignedDuration::from_mins(5),
        );
        assert_eq!(
            rate_of(&five_minutely, t(300), 1, Rate::PerMinute),
            vec![1_200.0]
        );

        assert_eq!(
            rate_of(&Throughput::default(), t(60), 4, Rate::PerMinute),
            vec![0.0; 4],
            "an endpoint with no traffic rates as zero rather than nothing"
        );
    }
}
