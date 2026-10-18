//! Rate limits and token budgets on policies.
//!
//! A policy's [`Limits`] cap requests and tokens per minute and per day: of
//! every LLM request, and of the requests to one provider or one account.
//! The windows slide: a minute is the last 60 s (one-second buckets), a day
//! the last 24 h (one-minute buckets). Consumption is counted per client key;
//! with `per = "policy"` the limits apply to the sum over every key of the
//! policy instead.
//!
//! A request is counted when it is admitted. Its tokens are counted when the
//! response finishes, so token limits are checked before a request and the
//! request that crosses a token limit still completes. Rejected requests are
//! not counted. Counters live in memory and are rebuilt from the request log
//! at startup.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, OnceLock};

use opentelemetry::KeyValue;
use opentelemetry::metrics::ObservableGauge;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::db::{Db, RequestCount};
use crate::policy::Policy;
use crate::telemetry::metrics;

const MINUTE: i64 = 60;
const DAY: i64 = 86_400;

/// Whose consumption a policy's limits apply to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Counting {
    /// Each key has its own allowance.
    #[default]
    Key,
    /// All keys of the policy share one allowance.
    Policy,
}

impl Counting {
    fn is_key(&self) -> bool {
        *self == Counting::Key
    }
}

/// Request and token limits over the sliding windows. A missing field is unlimited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allowance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_minute: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_day: Option<u64>,
    /// Input plus output tokens, as the upstream reports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_minute: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_day: Option<u64>,
}

impl Allowance {
    pub fn is_empty(&self) -> bool {
        self.checks().is_empty()
    }

    fn validate(&self, prefix: &str) -> anyhow::Result<()> {
        for (name, limit, ..) in self.checks() {
            anyhow::ensure!(limit > 0, "{prefix}{name} must be at least 1 (disable the key to block it)");
        }
        Ok(())
    }

    /// Each configured limit: name, value, unit and window.
    fn checks(&self) -> Vec<(&'static str, u64, Unit, i64)> {
        [
            ("requests_per_minute", self.requests_per_minute, Unit::Requests, MINUTE),
            ("requests_per_day", self.requests_per_day, Unit::Requests, DAY),
            ("tokens_per_minute", self.tokens_per_minute, Unit::Tokens, MINUTE),
            ("tokens_per_day", self.tokens_per_day, Unit::Tokens, DAY),
        ]
        .into_iter()
        .filter_map(|(name, limit, unit, window)| limit.map(|l| (name, l, unit, window)))
        .collect()
    }
}

/// Limits of a policy: of every LLM request, and of the requests to one
/// provider or one subscription account. A missing field is unlimited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Counting::is_key")]
    pub per: Counting,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_minute: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_day: Option<u64>,
    /// Input plus output tokens, as the upstream reports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_minute: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_day: Option<u64>,
    /// Provider name -> limits of the requests sent to it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, Allowance>,
    /// Account id -> limits of the requests sent to it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub accounts: BTreeMap<String, Allowance>,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        self.total().is_empty() && self.providers.is_empty() && self.accounts.is_empty() && self.per.is_key()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.total().validate("limits.")?;
        for (p, a) in &self.providers {
            a.validate(&format!("limits.providers.{p}."))?;
        }
        for (id, a) in &self.accounts {
            a.validate(&format!("limits.accounts.{id}."))?;
        }
        Ok(())
    }

    /// The limits of every LLM request.
    pub fn total(&self) -> Allowance {
        Allowance {
            requests_per_minute: self.requests_per_minute,
            requests_per_day: self.requests_per_day,
            tokens_per_minute: self.tokens_per_minute,
            tokens_per_day: self.tokens_per_day,
        }
    }

    /// The limits a meter is checked against.
    fn allowance(&self, meter: &Meter) -> Option<Allowance> {
        match meter {
            Meter::Total => Some(self.total()),
            Meter::Provider(p) => self.providers.get(*p).cloned(),
            Meter::Account(id) => self.accounts.get(*id).cloned(),
        }
    }
}

/// What a count is of: every LLM request of a key, or those sent to one
/// provider or one account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Meter<'a> {
    Total,
    Provider(&'a str),
    Account(&'a str),
}

/// The meters a request still has to count on. A pool counts them together
/// with the account that serves the request, so a request no account
/// admits counts on none of them.
#[derive(Debug, Default)]
pub struct Charge<'a> {
    meters: Vec<Meter<'a>>,
    settled: bool,
}

impl<'a> Charge<'a> {
    pub fn new(meters: &[Meter<'a>]) -> Self {
        Charge { meters: meters.to_vec(), settled: false }
    }

    /// The meters to count with the first account the request reaches.
    pub fn pending(&self) -> &[Meter<'a>] {
        if self.settled { &[] } else { &self.meters }
    }

    /// The request was counted on the meters.
    pub fn settle(&mut self) {
        self.settled = true;
    }

    pub fn is_settled(&self) -> bool {
        self.settled
    }
}

impl Meter<'_> {
    fn describe(&self) -> Option<String> {
        match self {
            Meter::Total => None,
            Meter::Provider(p) => Some(format!("provider {p:?}")),
            Meter::Account(id) => Some(format!("account {id:?}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Requests,
    Tokens,
}

/// Consumption in the current windows, in the shape of [`Limits`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consumption {
    pub requests_per_minute: u64,
    pub requests_per_day: u64,
    pub tokens_per_minute: u64,
    pub tokens_per_day: u64,
}

/// A request refused by a limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exceeded {
    /// The limit's field name, e.g. `requests_per_minute`.
    pub limit: &'static str,
    /// The provider or account the limit is on; `None` for every request.
    pub on: Option<String>,
    pub value: u64,
    pub per: Counting,
    /// Seconds until enough of the window has passed to admit a request.
    pub retry_after: i64,
}

impl Exceeded {
    pub fn message(&self, policy: &str) -> String {
        let whose = match self.per {
            Counting::Key => "this key".to_string(),
            Counting::Policy => format!("policy {policy:?}"),
        };
        let on = self.on.as_ref().map(|o| format!(" on {o}")).unwrap_or_default();
        format!("rate limit exceeded: {} of {}{on} for {whose}; retry after {} s", self.limit, self.value, self.retry_after)
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Start of the bucket, in Unix seconds.
    at: i64,
    requests: u64,
    tokens: u64,
}

impl Bucket {
    fn get(&self, unit: Unit) -> u64 {
        match unit {
            Unit::Requests => self.requests,
            Unit::Tokens => self.tokens,
        }
    }
}

/// One key's consumption: one-second buckets for the minute window and
/// one-minute buckets for the day window, oldest first.
#[derive(Debug, Default)]
struct Usage {
    seconds: VecDeque<Bucket>,
    minutes: VecDeque<Bucket>,
}

impl Usage {
    fn add(&mut self, at: i64, requests: u64, tokens: u64) {
        add(&mut self.seconds, at, requests, tokens);
        add(&mut self.minutes, at - at.rem_euclid(MINUTE), requests, tokens);
    }

    fn prune(&mut self, now: i64) {
        for (buckets, window) in [(&mut self.seconds, MINUTE), (&mut self.minutes, DAY)] {
            while buckets.front().is_some_and(|b| b.at <= now - window) {
                buckets.pop_front();
            }
        }
    }

    fn buckets(&self, window: i64) -> &VecDeque<Bucket> {
        if window == MINUTE { &self.seconds } else { &self.minutes }
    }

    fn is_empty(&self) -> bool {
        self.minutes.is_empty()
    }
}

/// Add to the bucket starting at `at`; buckets stay sorted (a late write to
/// an older bucket is inserted in place).
fn add(buckets: &mut VecDeque<Bucket>, at: i64, requests: u64, tokens: u64) {
    match buckets.iter().rposition(|b| b.at <= at) {
        Some(i) if buckets[i].at == at => {
            buckets[i].requests += requests;
            buckets[i].tokens += tokens;
        }
        Some(i) => buckets.insert(i + 1, Bucket { at, requests, tokens }),
        None => buckets.push_front(Bucket { at, requests, tokens }),
    }
}

/// One key's consumption: of every request, and per provider and account.
#[derive(Debug, Default)]
struct ClientUsage {
    total: Usage,
    providers: HashMap<String, Usage>,
    accounts: HashMap<String, Usage>,
}

impl ClientUsage {
    fn get(&mut self, meter: &Meter) -> Option<&mut Usage> {
        match meter {
            Meter::Total => Some(&mut self.total),
            Meter::Provider(p) => self.providers.get_mut(*p),
            Meter::Account(id) => self.accounts.get_mut(*id),
        }
    }

    fn entry(&mut self, meter: &Meter) -> &mut Usage {
        match meter {
            Meter::Total => &mut self.total,
            Meter::Provider(p) => self.providers.entry(p.to_string()).or_default(),
            Meter::Account(id) => self.accounts.entry(id.to_string()).or_default(),
        }
    }

    fn add(&mut self, provider: &str, account: Option<&str>, at: i64, requests: u64, tokens: u64) {
        self.total.add(at, requests, tokens);
        self.entry(&Meter::Provider(provider)).add(at, requests, tokens);
        if let Some(id) = account {
            self.entry(&Meter::Account(id)).add(at, requests, tokens);
        }
    }

    fn prune(&mut self, now: i64) {
        self.total.prune(now);
        for m in [&mut self.providers, &mut self.accounts] {
            m.retain(|_, u| {
                u.prune(now);
                !u.is_empty()
            });
        }
    }
}

/// In-memory consumption of every client key.
#[derive(Default)]
pub struct Limiter {
    clients: Mutex<HashMap<String, ClientUsage>>,
    gauge: OnceLock<ObservableGauge<u64>>,
}

/// Client ids are `<policy>/<name>`.
fn in_policy(client_id: &str, policy: &str) -> bool {
    client_id.strip_prefix(policy).is_some_and(|rest| rest.starts_with('/'))
}

impl Limiter {
    /// Rebuild the counters from the request log of the last day. Each
    /// logged request counts on the provider and the account that answered it.
    pub async fn seed(&self, db: &Db, now: i64) -> anyhow::Result<usize> {
        let rows = db.request_counts_since(now - DAY).await?;
        let mut clients = self.clients.lock();
        for RequestCount { client_id, provider, account_id, ts, requests, tokens } in &rows {
            clients.entry(client_id.clone()).or_default().add(
                provider,
                account_id.as_deref(),
                *ts,
                *requests as u64,
                (*tokens).max(0) as u64,
            );
        }
        Ok(rows.len())
    }

    /// Admit one request of `client_id` under `policy` on every meter,
    /// counting it on each, or refuse it when a limit of one is already
    /// reached. Nothing is counted when the request is refused.
    pub fn admit(&self, client_id: &str, policy: &Policy, meters: &[Meter], now: i64) -> Result<(), Exceeded> {
        let mut clients = self.clients.lock();
        for meter in meters {
            if let Err(e) = check(&mut clients, client_id, policy, meter, now) {
                metrics().limit_rejections.add(
                    1,
                    &[
                        KeyValue::new("policy", policy.name.clone()),
                        KeyValue::new("client", client_id.to_string()),
                        KeyValue::new("limit", e.limit),
                        KeyValue::new("on", e.on.clone().unwrap_or_else(|| "all".into())),
                    ],
                );
                return Err(e);
            }
        }
        let usage = clients.entry(client_id.to_string()).or_default();
        for meter in meters {
            usage.entry(meter).add(now, 1, 0);
        }
        Ok(())
    }

    /// Whether a request on every meter would be admitted, without counting it.
    pub fn check(&self, client_id: &str, policy: &Policy, meters: &[Meter], now: i64) -> Result<(), Exceeded> {
        let mut clients = self.clients.lock();
        meters.iter().try_for_each(|meter| check(&mut clients, client_id, policy, meter, now))
    }

    /// Count the tokens of a finished request, on its provider and account.
    pub fn record_tokens(&self, client_id: &str, provider: &str, account: Option<&str>, tokens: u64, now: i64) {
        if tokens > 0 {
            self.clients.lock().entry(client_id.to_string()).or_default().add(provider, account, now, 0, tokens);
        }
    }

    /// One key's consumption of every request.
    pub fn consumption(&self, client_id: &str, now: i64) -> Consumption {
        self.sum(now, Meter::Total, |id| id == client_id)
    }

    /// The consumption of every key of a policy together, on a meter.
    pub fn policy_consumption(&self, policy: &str, meter: Meter, now: i64) -> Consumption {
        self.sum(now, meter, |id| in_policy(id, policy))
    }

    /// The consumption a policy's limits of every request are checked against for a key.
    pub fn counted(&self, client_id: &str, policy: &Policy, now: i64) -> Consumption {
        match policy.limits.per {
            Counting::Key => self.consumption(client_id, now),
            Counting::Policy => self.policy_consumption(&policy.name, Meter::Total, now),
        }
    }

    fn sum(&self, now: i64, meter: Meter, select: impl Fn(&str) -> bool) -> Consumption {
        let mut clients = self.clients.lock();
        let mut c = Consumption::default();
        for (_, client) in clients.iter_mut().filter(|(id, _)| select(id)) {
            client.prune(now);
            let Some(usage) = client.get(&meter) else { continue };
            let total = |window: i64, unit: Unit| usage.buckets(window).iter().map(|b| b.get(unit)).sum::<u64>();
            c.requests_per_minute += total(MINUTE, Unit::Requests);
            c.requests_per_day += total(DAY, Unit::Requests);
            c.tokens_per_minute += total(MINUTE, Unit::Tokens);
            c.tokens_per_day += total(DAY, Unit::Tokens);
        }
        c
    }

    /// Drop keys with nothing left in the day window.
    pub fn prune(&self, now: i64) {
        self.clients.lock().retain(|_, client| {
            client.prune(now);
            !client.total.is_empty()
        });
    }

    /// Every key with consumption in the day window.
    fn snapshot(&self, now: i64) -> Vec<(String, Consumption)> {
        let ids: Vec<String> = self.clients.lock().keys().cloned().collect();
        ids.into_iter()
            .map(|id| {
                let c = self.consumption(&id, now);
                (id, c)
            })
            .collect()
    }

    /// Export consumption as the `xolotl.limit.consumption` gauge.
    pub fn register_metrics(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        let gauge = opentelemetry::global::meter("xolotl")
            .u64_observable_gauge("xolotl.limit.consumption")
            .with_description("Requests and tokens per client key in the sliding minute and day windows")
            .with_callback(move |o| {
                let Some(me) = me.upgrade() else { return };
                for (client, c) in me.snapshot(crate::db::now()) {
                    for (unit, window, n) in [
                        ("requests", "minute", c.requests_per_minute),
                        ("requests", "day", c.requests_per_day),
                        ("tokens", "minute", c.tokens_per_minute),
                        ("tokens", "day", c.tokens_per_day),
                    ] {
                        o.observe(
                            n,
                            &[KeyValue::new("client", client.clone()), KeyValue::new("unit", unit), KeyValue::new("window", window)],
                        );
                    }
                }
            })
            .build();
        let _ = self.gauge.set(gauge);
    }
}

/// Check the policy's limits on `meter` for a key, or for all keys of the
/// policy with `per = "policy"`.
fn check(clients: &mut HashMap<String, ClientUsage>, client_id: &str, policy: &Policy, meter: &Meter, now: i64) -> Result<(), Exceeded> {
    let limits = &policy.limits;
    let Some(allowance) = limits.allowance(meter) else { return Ok(()) };
    for (limit, value, unit, window) in allowance.checks() {
        let mut buckets: Vec<Bucket> = Vec::new();
        for (id, client) in clients.iter_mut() {
            let counted = match limits.per {
                Counting::Key => id == client_id,
                Counting::Policy => in_policy(id, &policy.name),
            };
            if counted {
                client.prune(now);
                if let Some(usage) = client.get(meter) {
                    buckets.extend(usage.buckets(window));
                }
            }
        }
        if let Some(retry_after) = retry_after(buckets, unit, window, value, now) {
            return Err(Exceeded { limit, on: meter.describe(), value, per: limits.per, retry_after });
        }
    }
    Ok(())
}

/// `None` when consumption in the window is below `limit`; otherwise the
/// seconds until enough buckets age out of the window to drop below it.
fn retry_after(mut buckets: Vec<Bucket>, unit: Unit, window: i64, limit: u64, now: i64) -> Option<i64> {
    let mut used: u64 = buckets.iter().map(|b| b.get(unit)).sum();
    if used < limit {
        return None;
    }
    buckets.sort_by_key(|b| b.at);
    for b in &buckets {
        used -= b.get(unit);
        if used < limit {
            return Some((b.at + window - now).max(1));
        }
    }
    Some(window)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_700_000_000;

    fn policy(name: &str, limits: Limits) -> Policy {
        Policy { name: name.into(), providers: vec!["*".into()], limits, ..Default::default() }
    }

    #[test]
    fn requests_per_minute_slides() {
        let l = Limiter::default();
        let p = policy("p", Limits { requests_per_minute: Some(2), ..Default::default() });
        l.admit("p/a", &p, &[Meter::Total], T0).unwrap();
        l.admit("p/a", &p, &[Meter::Total], T0 + 10).unwrap();
        let e = l.admit("p/a", &p, &[Meter::Total], T0 + 20).unwrap_err();
        assert_eq!(e.limit, "requests_per_minute");
        // The first request leaves the window at T0 + 60.
        assert_eq!(e.retry_after, 40);
        // Rejected requests are not counted.
        assert_eq!(l.consumption("p/a", T0 + 20).requests_per_minute, 2);
        l.admit("p/a", &p, &[Meter::Total], T0 + 60).unwrap();
        assert!(l.admit("p/a", &p, &[Meter::Total], T0 + 61).is_err());
    }

    #[test]
    fn counting_per_key_and_per_policy() {
        let l = Limiter::default();
        let per_key = policy("p", Limits { requests_per_day: Some(1), ..Default::default() });
        l.admit("p/a", &per_key, &[Meter::Total], T0).unwrap();
        l.admit("p/b", &per_key, &[Meter::Total], T0).unwrap();
        // Another policy whose name is a prefix doesn't share the count.
        let shared = policy("p", Limits { per: Counting::Policy, requests_per_day: Some(3), ..Default::default() });
        l.admit("pp/x", &shared, &[Meter::Total], T0).unwrap();
        l.admit("p/c", &shared, &[Meter::Total], T0).unwrap();
        let e = l.admit("p/d", &shared, &[Meter::Total], T0).unwrap_err();
        assert_eq!((e.limit, e.per), ("requests_per_day", Counting::Policy));
        assert!(e.message("p").contains("policy \"p\""));
        assert_eq!(l.policy_consumption("p", Meter::Total, T0).requests_per_day, 3);
        assert_eq!(l.counted("p/d", &shared, T0).requests_per_day, 3);
        assert_eq!(l.counted("p/d", &per_key, T0).requests_per_day, 0);
    }

    #[test]
    fn provider_and_account_limits() {
        let l = Limiter::default();
        let one = Allowance { requests_per_day: Some(1), ..Default::default() };
        let mut limits = Limits { per: Counting::Policy, ..Default::default() };
        limits.providers.insert("codex".into(), one.clone());
        limits.accounts.insert("codex-a1".into(), Allowance { tokens_per_day: Some(10), ..Default::default() });
        let p = policy("p", limits);
        l.admit("p/a", &p, &[Meter::Total, Meter::Provider("codex")], T0).unwrap();
        // Another key of the policy shares the allowance; nothing is counted when refused.
        let e = l.admit("p/b", &p, &[Meter::Total, Meter::Provider("codex")], T0).unwrap_err();
        assert_eq!((e.limit, e.on.as_deref()), ("requests_per_day", Some("provider \"codex\"")));
        assert!(e.message("p").contains("requests_per_day of 1 on provider \"codex\" for policy \"p\""), "{}", e.message("p"));
        assert_eq!(l.policy_consumption("p", Meter::Total, T0).requests_per_day, 1);
        l.admit("p/b", &p, &[Meter::Total, Meter::Provider("llama")], T0).unwrap();

        // Tokens count on the provider and the account that served them.
        l.admit("p/a", &p, &[Meter::Account("codex-a1")], T0).unwrap();
        l.record_tokens("p/a", "codex", Some("codex-a1"), 10, T0);
        assert!(l.check("p/b", &p, &[Meter::Account("codex-a1")], T0).is_err());
        assert!(l.check("p/b", &p, &[Meter::Account("codex-b2")], T0).is_ok());
        assert_eq!(l.policy_consumption("p", Meter::Provider("codex"), T0).tokens_per_day, 10);
        assert_eq!(l.policy_consumption("p", Meter::Account("codex-a1"), T0).requests_per_day, 1);
        assert_eq!(l.policy_consumption("p", Meter::Total, T0).tokens_per_day, 10);
    }

    #[test]
    fn token_limits_check_finished_requests() {
        let l = Limiter::default();
        let p = policy("p", Limits { tokens_per_minute: Some(100), tokens_per_day: Some(150), ..Default::default() });
        l.admit("p/a", &p, &[Meter::Total], T0).unwrap();
        l.record_tokens("p/a", "codex", None, 99, T0);
        // Below the limit: admitted, even though this request may cross it.
        l.admit("p/a", &p, &[Meter::Total], T0 + 1).unwrap();
        l.record_tokens("p/a", "codex", None, 60, T0 + 1);
        let e = l.admit("p/a", &p, &[Meter::Total], T0 + 2).unwrap_err();
        assert_eq!((e.limit, e.retry_after), ("tokens_per_minute", 58));
        // Next minute: the minute window is clear, the day window is not.
        let e = l.admit("p/a", &p, &[Meter::Total], T0 + 61).unwrap_err();
        assert_eq!(e.limit, "tokens_per_day");
        assert!(e.retry_after > 86_000);
        let c = l.consumption("p/a", T0 + 61);
        assert_eq!(c, Consumption { requests_per_minute: 0, requests_per_day: 2, tokens_per_minute: 0, tokens_per_day: 159 });
    }

    #[test]
    fn retry_after_counts_enough_buckets() {
        let b = |at, requests| Bucket { at, requests, tokens: 0 };
        // Two requests must age out before a third fits under a limit of 2.
        let buckets = vec![b(T0 + 5, 1), b(T0, 1), b(T0 + 9, 1)];
        assert_eq!(retry_after(buckets, Unit::Requests, MINUTE, 2, T0 + 30), Some(35));
        assert_eq!(retry_after(vec![b(T0, 1)], Unit::Requests, MINUTE, 2, T0 + 30), None);
    }

    #[test]
    fn unlimited_policies_still_count() {
        let l = Limiter::default();
        let p = policy("p", Limits::default());
        for _ in 0..5 {
            l.admit("p/a", &p, &[Meter::Total], T0).unwrap();
        }
        assert_eq!(l.consumption("p/a", T0).requests_per_minute, 5);
        l.prune(T0 + DAY);
        assert!(l.clients.lock().is_empty());
    }

    #[test]
    fn late_writes_keep_buckets_sorted() {
        let mut u = Usage::default();
        u.add(T0 + 10, 1, 0);
        u.add(T0, 0, 5);
        u.add(T0 + 10, 0, 1);
        let at: Vec<i64> = u.seconds.iter().map(|b| b.at).collect();
        assert_eq!(at, vec![T0, T0 + 10]);
        assert_eq!(u.seconds[1].tokens, 1);
    }

    #[test]
    fn validation_and_serde() {
        assert!(Limits { requests_per_minute: Some(0), ..Default::default() }.validate().is_err());
        assert!(Limits::default().is_empty());
        assert!(!Limits { per: Counting::Policy, ..Default::default() }.is_empty());
        let l: Limits = toml::from_str("per = \"policy\"\ntokens_per_day = 5").unwrap();
        assert_eq!(l, Limits { per: Counting::Policy, tokens_per_day: Some(5), ..Default::default() });
        assert!(toml::from_str::<Limits>("spend = 5").is_err());
        let l: Limits = toml::from_str("[providers.codex]\nrequests_per_day = 5\n[accounts.codex-a1]\ntokens_per_day = 0").unwrap();
        assert_eq!(l.providers["codex"].requests_per_day, Some(5));
        assert!(l.validate().unwrap_err().to_string().contains("limits.accounts.codex-a1.tokens_per_day"));
        assert!(toml::from_str::<Limits>("[providers.codex]\nspend = 5").is_err());
    }

    #[tokio::test]
    async fn seeds_from_the_request_log() {
        let db = Db::in_memory().await.unwrap();
        let now = crate::db::now();
        for (client, input) in [(Some("p/a"), Some(10)), (Some("p/a"), None), (None, Some(7))] {
            db.log_request(&crate::db::RequestRecord {
                client_id: client.map(str::to_string),
                provider: "codex".into(),
                endpoint: "/v1/responses".into(),
                status: 200,
                input_tokens: input,
                output_tokens: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let l = Limiter::default();
        l.seed(&db, now).await.unwrap();
        let c = l.consumption("p/a", now);
        assert_eq!((c.requests_per_day, c.tokens_per_day), (2, 14));
        // Requests of the bootstrap admin token (no client) are not counted.
        assert_eq!(l.clients.lock().keys().collect::<Vec<_>>(), vec!["p/a"]);
        // Rows older than a day are not read.
        let later = Limiter::default();
        assert_eq!(later.seed(&db, now + DAY + MINUTE).await.unwrap(), 0);
        assert_eq!(later.consumption("p/a", now + DAY + MINUTE), Consumption::default());
    }

    #[tokio::test]
    async fn replay_matches_live_counting_across_a_window_boundary() {
        let db = Db::in_memory().await.unwrap();
        let now = crate::db::now();
        let p = policy("p", Limits { requests_per_minute: Some(1), ..Default::default() });
        // Admitted 61 s ago, finished now.
        let live = Arc::new(Limiter::default());
        live.admit("p/a", &p, &[Meter::Total], now - 61).unwrap();
        let mut acc = crate::proxy::Accounting::new(db.clone(), "codex", "/v1/responses", Some("p/a"), None).with_limiter(&live);
        // Accounting starts with the request, so it records the admission time.
        assert!(acc.record.started_at.is_some_and(|t| (now..=now + 1).contains(&t)));
        acc.record.started_at = Some(now - 61);
        acc.finish(200, crate::proxy::TokenUsage { input: Some(5), output: Some(3), cached: None });
        for _ in 0..50 {
            if !db.request_counts_since(0).await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let replayed = Limiter::default();
        replayed.seed(&db, now).await.unwrap();
        let expected = Consumption { requests_per_minute: 0, requests_per_day: 1, tokens_per_minute: 8, tokens_per_day: 8 };
        assert_eq!(live.consumption("p/a", now), expected);
        assert_eq!(replayed.consumption("p/a", now), expected);
        assert!(replayed.admit("p/a", &p, &[Meter::Total], now).is_ok(), "the request left the minute window before the restart");
    }
}
