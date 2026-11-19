//! The time a turn's memory work may take.
//!
//! - [`HookBudget`] is made at the start of preparing a turn: it bounds that turn's SessionBridge
//!   fetch, the automatic recall (request and body read) and the composing. Its deadline is the
//!   automatic tier's budget plus a transport margin, and always inside `hook_timeout_ms` (a turn
//!   goes on without memory rather than wait longer). The recall is sent what is left as `budget_ms`:
//!   the server only ever shortens its own budget to it.
//! - [`RecallBudgets`] holds the server's configured budgets (automatic tier, active retrieval), read
//!   from `/v1/memory/health` at start-up and from the responses' `execution`, and updated only when the
//!   configuration fingerprint changes; a response's effective budget is never written back. Until they
//!   are known, or when the server runs none (an older server: no `execution`), start-up values derived
//!   from `hook_timeout_ms` are used.
use super::{log::Logger, util::now_ms};
use serde_json::Value;
use std::sync::{Arc, Mutex};

const TAG: &str = "[helicon-memory]";
/// Network, JSON and composing: what the extension needs beyond the server's own budget.
pub(crate) const TRANSPORT_MARGIN_MS: u64 = 800;
/// A turn's memory work ends this much before the hook timeout.
pub(crate) const HOST_SAFETY_MS: u64 = 300;
/// The SessionBridge is waited for until this much before the turn's deadline (then omitted).
pub(crate) const COMPOSE_MARGIN_MS: u64 = 100;
/// Less than this left: the recall is not sent (it could not end in time).
pub(crate) const MIN_RECALL_BUDGET_MS: i64 = 200;
/// The active retrieval's start-up value is at most this (less the transport margin).
const ACTIVE_START_CAP_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BudgetSource {
    Health,
    Response,
    Startup,
}

impl BudgetSource {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Health => "health",
            Self::Response => "response",
            Self::Startup => "startup",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Budgets {
    /// The automatic tier's budget, ms.
    pub auto_ms: u64,
    /// The active retrieval's budget, ms.
    pub active_ms: u64,
    pub source: BudgetSource,
    pub fingerprint: Option<String>,
}

/// The start-up values: from the hook timeout alone.
pub(crate) fn startup_budgets(hook_timeout_ms: u64) -> Budgets {
    let auto_ms = hook_timeout_ms.saturating_sub(TRANSPORT_MARGIN_MS + HOST_SAFETY_MS);
    Budgets {
        auto_ms,
        active_ms: (3 * auto_ms).min(ACTIVE_START_CAP_MS - TRANSPORT_MARGIN_MS),
        source: BudgetSource::Startup,
        fingerprint: None,
    }
}

/// A server budget is believed up to this long: a figure beyond it is a mistake, not a budget.
const MAX_BUDGET_MS: f64 = 600_000.0;

fn positive(v: Option<&Value>) -> Option<u64> {
    let n = v?.as_f64()?;
    (n.is_finite() && n > 0.0).then(|| n.min(MAX_BUDGET_MS).floor() as u64)
}

#[derive(Default)]
struct State {
    cached: Option<Budgets>,
    /// Why the start-up values are in use (said once per change).
    startup_why: Option<String>,
}

/// The server's configured budgets for one backend, else the start-up values.
#[derive(Clone)]
pub(crate) struct RecallBudgets {
    base_url: String,
    hook_timeout_ms: u64,
    logger: Logger,
    state: Arc<Mutex<State>>,
}

impl RecallBudgets {
    pub(crate) fn new(base_url: &str, hook_timeout_ms: u64, logger: Logger) -> Self {
        Self {
            base_url: base_url.to_owned(),
            hook_timeout_ms,
            logger,
            state: Arc::default(),
        }
    }

    pub(crate) fn current(&self) -> Budgets {
        self.state
            .lock()
            .unwrap()
            .cached
            .clone()
            .unwrap_or_else(|| startup_budgets(self.hook_timeout_ms))
    }

    /// Active retrieval: the deadline of a search started now (the server's budget plus transport).
    pub(crate) fn active_deadline_ms(&self) -> u64 {
        now_ms() + self.current().active_ms + TRANSPORT_MARGIN_MS
    }

    /// `/v1/memory/health`'s answer (`None`: the call failed).
    pub(crate) fn learn_from_health(&self, health: Option<&Value>) {
        let Some(h) = health else {
            return self.use_startup("health not answered");
        };
        let Some(r) = h.get("recall").filter(|r| r.is_object()) else {
            return self.use_startup("no recall budgets in health (older server)");
        };
        let invalid = match r.get("error").and_then(Value::as_str) {
            Some(e) => format!("health recall error: {e}"),
            None => "health without budgets".to_owned(),
        };
        self.take(
            r.get("auto_tier_budget_ms"),
            r.get("active_budget_ms"),
            r.get("config_fingerprint"),
            BudgetSource::Health,
            &invalid,
        );
    }

    /// A recall response's `execution` (`None`: the server ran no budgets).
    pub(crate) fn learn_from_response(&self, execution: Option<&Value>) {
        let Some(exec) = execution.filter(|e| e.is_object()) else {
            return self
                .use_startup("a response without execution (older server or benchmark instance)");
        };
        // (effective_budget_ms is this recall's own, shortened by what was sent: never written back)
        self.take(
            exec.get("auto_tier_budget_ms"),
            exec.get("active_budget_ms"),
            exec.get("config_fingerprint"),
            BudgetSource::Response,
            "execution without budgets",
        );
    }

    fn take(
        &self,
        auto: Option<&Value>,
        active: Option<&Value>,
        fingerprint: Option<&Value>,
        source: BudgetSource,
        invalid_why: &str,
    ) {
        let fingerprint = fingerprint
            .and_then(Value::as_str)
            .filter(|f| !f.is_empty())
            .map(str::to_owned);
        let (Some(auto_ms), Some(active_ms), Some(fingerprint)) =
            (positive(auto), positive(active), fingerprint)
        else {
            return self.use_startup(invalid_why);
        };
        let mut state = self.state.lock().unwrap();
        if state
            .cached
            .as_ref()
            .is_some_and(|c| c.fingerprint.as_deref() == Some(fingerprint.as_str()))
        {
            return; // the same configuration: nothing to update
        }
        let before = state.cached.take();
        self.logger.info(&format!(
            "{TAG} recall budgets for {}: automatic {auto_ms}ms, active {active_ms}ms (fingerprint {fingerprint}, from {}{})",
            self.base_url,
            source.name(),
            before
                .map(|b| format!("; was {}/{}ms", b.auto_ms, b.active_ms))
                .unwrap_or_default()
        ));
        state.cached = Some(Budgets {
            auto_ms,
            active_ms,
            source,
            fingerprint: Some(fingerprint),
        });
        state.startup_why = None;
    }

    fn use_startup(&self, why: &str) {
        let mut state = self.state.lock().unwrap();
        let had = state.cached.take().is_some();
        if had || state.startup_why.as_deref() != Some(why) {
            let s = startup_budgets(self.hook_timeout_ms);
            self.logger.warn(&format!(
                "{TAG} recall budgets for {}: start-up values (automatic {}ms, active {}ms) - {why}{}",
                self.base_url,
                s.auto_ms,
                s.active_ms,
                if had { "; the cached budgets were dropped" } else { "" }
            ));
        }
        state.startup_why = Some(why.to_owned());
    }
}

/// One turn's time (made at its start; a configuration read later does not reset it).
#[derive(Clone, Debug)]
pub(crate) struct HookBudget {
    pub hook_timeout_ms: u64,
    pub budgets: Budgets,
    pub started_at: u64,
    pub deadline_ms: u64,
}

impl HookBudget {
    pub(crate) fn new(hook_timeout_ms: u64, budgets: Budgets) -> Self {
        Self::at(hook_timeout_ms, budgets, now_ms())
    }

    pub(crate) fn at(hook_timeout_ms: u64, budgets: Budgets, now: u64) -> Self {
        let span = (budgets.auto_ms + TRANSPORT_MARGIN_MS)
            .min(hook_timeout_ms.saturating_sub(HOST_SAFETY_MS));
        Self {
            hook_timeout_ms,
            budgets,
            started_at: now,
            deadline_ms: now + span,
        }
    }

    /// What the recall sent now may take on the server: the deadline less the transport margin.
    pub(crate) fn recall_budget_ms(&self) -> i64 {
        self.deadline_ms as i64 - now_ms() as i64 - TRANSPORT_MARGIN_MS as i64
    }

    /// The SessionBridge is waited for until this time, then omitted.
    pub(crate) fn compose_by_ms(&self) -> u64 {
        self.deadline_ms.saturating_sub(COMPOSE_MARGIN_MS)
    }

    pub(crate) fn elapsed_ms(&self) -> u64 {
        now_ms().saturating_sub(self.started_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn logger() -> Logger {
        Logger::new(std::env::temp_dir().join("helicon-budget-test.log"), false)
    }

    #[test]
    fn start_up_values_follow_the_hook_timeout() {
        let b = startup_budgets(15_000);
        assert_eq!(b.auto_ms, 13_900);
        assert_eq!(b.active_ms, 41_700);
        assert_eq!(startup_budgets(100).auto_ms, 0);
        assert_eq!(startup_budgets(60_000).active_ms, 59_200);
    }

    #[test]
    fn the_server_budgets_replace_them_until_its_fingerprint_changes() {
        let budgets = RecallBudgets::new("http://h", 15_000, logger());
        assert_eq!(budgets.current().source, BudgetSource::Startup);
        budgets.learn_from_response(Some(&json!({
            "auto_tier_budget_ms": 2000, "active_budget_ms": 9000, "config_fingerprint": "abc"
        })));
        let c = budgets.current();
        assert_eq!(
            (c.auto_ms, c.active_ms, c.source),
            (2000, 9000, BudgetSource::Response)
        );
        // the same fingerprint does not update; another does
        budgets.learn_from_response(Some(&json!({
            "auto_tier_budget_ms": 5, "active_budget_ms": 6, "config_fingerprint": "abc"
        })));
        assert_eq!(budgets.current().auto_ms, 2000);
        budgets.learn_from_health(Some(&json!({"recall": {
            "auto_tier_budget_ms": 3000, "active_budget_ms": 10000, "config_fingerprint": "def"
        }})));
        assert_eq!(budgets.current().auto_ms, 3000);
        // a response without execution drops them
        budgets.learn_from_response(None);
        assert_eq!(budgets.current().source, BudgetSource::Startup);
    }

    #[test]
    fn an_absurd_server_budget_is_clamped() {
        let budgets = RecallBudgets::new("http://h", 15_000, logger());
        budgets.learn_from_response(Some(&json!({
            "auto_tier_budget_ms": 1.0e30, "active_budget_ms": 1.0e30, "config_fingerprint": "x"
        })));
        let c = budgets.current();
        assert_eq!((c.auto_ms, c.active_ms), (600_000, 600_000));
        assert!(budgets.active_deadline_ms() > now_ms());
    }

    #[test]
    fn the_hook_budget_stays_inside_the_hook_timeout() {
        let b = HookBudget::at(15_000, startup_budgets(15_000), 1_000_000);
        // auto 13_900 + margin 800 = 14_700; timeout 15_000 - 300 = 14_700
        assert_eq!(b.deadline_ms, 1_014_700);
        let tight = HookBudget::at(2_000, startup_budgets(2_000), 0);
        assert_eq!(tight.deadline_ms, 1_700);
        assert_eq!(tight.compose_by_ms(), 1_600);
    }
}
