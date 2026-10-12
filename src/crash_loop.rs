use crate::model::Project;
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct CrashLoopPolicy {
    pub attempts: usize,
    pub window: Duration,
    pub healthy_reset: Duration,
}

impl Default for CrashLoopPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            window: Duration::from_secs(120),
            healthy_reset: Duration::from_secs(30),
        }
    }
}

impl CrashLoopPolicy {
    pub(crate) fn from_project(project: &Project) -> Result<Self> {
        let mut policy = Self::default();
        if let Some(config) = project.metadata.get("crashLoop") {
            ensure!(config.is_object(), "dockstride.crashLoop must be an object");
            for field in ["attempts", "windowSeconds", "healthyResetSeconds"] {
                if let Some(value) = config.get(field) {
                    let value = value.as_u64().filter(|value| *value > 0).with_context(|| {
                        format!("dockstride.crashLoop.{field} must be a positive integer")
                    })?;
                    match field {
                        "attempts" => {
                            policy.attempts = usize::try_from(value)
                                .context("Crash-loop attempts exceeds platform capacity")?
                        }
                        "windowSeconds" => policy.window = Duration::from_secs(value),
                        _ => policy.healthy_reset = Duration::from_secs(value),
                    }
                }
            }
        }
        Ok(policy)
    }
}

struct Observations {
    failures: VecDeque<(String, Instant)>,
    seen: VecDeque<String>,
    healthy_since: Option<Instant>,
    updated: Instant,
}

pub(crate) struct CrashLoopTracker {
    policy: CrashLoopPolicy,
    keys: BTreeMap<String, Observations>,
}

impl CrashLoopTracker {
    pub(crate) fn new(policy: CrashLoopPolicy) -> Self {
        Self {
            policy,
            keys: BTreeMap::new(),
        }
    }

    fn observations(&mut self, key: &str, now: Instant) -> &mut Observations {
        let retention = self.policy.window.max(self.policy.healthy_reset);
        self.keys
            .retain(|_, entry| now.saturating_duration_since(entry.updated) <= retention);
        if !self.keys.contains_key(key) && self.keys.len() >= 4096 {
            if let Some(oldest) = self
                .keys
                .iter()
                .min_by_key(|(_, entry)| entry.updated)
                .map(|(key, _)| key.clone())
            {
                self.keys.remove(&oldest);
            }
        }
        let entry = self
            .keys
            .entry(key.to_owned())
            .or_insert_with(|| Observations {
                failures: VecDeque::new(),
                seen: VecDeque::new(),
                healthy_since: None,
                updated: now,
            });
        entry.updated = now;
        while entry
            .failures
            .front()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) >= self.policy.window)
        {
            entry.failures.pop_front();
        }
        entry
    }

    pub(crate) fn observe_failure(&mut self, key: &str, event: &str, now: Instant) -> bool {
        let policy = self.policy;
        let entry = self.observations(key, now);
        if entry
            .healthy_since
            .is_some_and(|since| now.saturating_duration_since(since) >= policy.healthy_reset)
        {
            entry.failures.clear();
        }
        entry.healthy_since = None;
        if !entry.seen.iter().any(|seen| seen == event) {
            entry.seen.push_back(event.to_owned());
            while entry.seen.len() > policy.attempts.max(64) {
                entry.seen.pop_front();
            }
            entry.failures.push_back((event.to_owned(), now));
            while entry.failures.len() > policy.attempts {
                entry.failures.pop_front();
            }
        }
        entry.failures.len() >= policy.attempts
    }

    pub(crate) fn observe_healthy(&mut self, key: &str, now: Instant) {
        let reset = self.policy.healthy_reset;
        let entry = self.observations(key, now);
        let since = *entry.healthy_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= reset {
            entry.failures.clear();
        }
    }

    pub(crate) fn observe_pending(&mut self, key: &str) {
        if let Some(entry) = self.keys.get_mut(key) {
            entry.healthy_since = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_events_expire_and_keys_are_isolated() {
        let now = Instant::now();
        let mut tracker = CrashLoopTracker::new(CrashLoopPolicy::default());
        assert!(!tracker.observe_failure("api/rev/1", "a", now));
        assert!(!tracker.observe_failure("api/rev/1", "a", now));
        assert!(!tracker.observe_failure("api/rev/1", "b", now));
        assert!(!tracker.observe_failure("api/rev/2", "c", now));
        assert!(tracker.observe_failure("api/rev/1", "c", now));
        let later = now + Duration::from_secs(120);
        assert!(!tracker.observe_failure("api/rev/1", "c", later));
        assert!(!tracker.observe_failure("api/rev/1", "d", later));
    }

    #[test]
    fn sustained_health_resets_but_pending_interrupts_recovery() {
        let now = Instant::now();
        let mut tracker = CrashLoopTracker::new(CrashLoopPolicy::default());
        tracker.observe_failure("api", "a", now);
        tracker.observe_failure("api", "b", now);
        tracker.observe_healthy("api", now);
        tracker.observe_pending("api");
        tracker.observe_healthy("api", now + Duration::from_secs(20));
        assert!(tracker.observe_failure("api", "c", now + Duration::from_secs(40)));
        tracker.observe_healthy("api", now + Duration::from_secs(41));
        tracker.observe_healthy("api", now + Duration::from_secs(71));
        assert!(!tracker.observe_failure("api", "d", now + Duration::from_secs(72)));
        assert!(!tracker.observe_failure("api", "c", now + Duration::from_secs(73)));
    }
}
