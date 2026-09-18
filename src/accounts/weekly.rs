use super::{
    account_key, codex_window_bucket, collapse_representatives, unix_now, AccountConfig,
    AccountPool, CodexWindow, Duration, HeaderMap, Instant, UsageSnapshot, UsageWindow,
    WINDOW_7D_SECS,
};

/// Preserve shared weekly evidence before usage normalization drops invalid windows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WeeklyUsageEvidence {
    Unreported,
    Invalid,
    Reported(UsageWindow),
}

impl WeeklyUsageEvidence {
    pub(crate) fn from_snapshot(usage: &UsageSnapshot) -> Self {
        usage
            .seven_day
            .clone()
            .map(Self::Reported)
            .unwrap_or(Self::Unreported)
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Source {
    AnthropicHeaders,
    CodexHeaders,
    AnthropicUsage,
    CodexUsage,
}

impl Source {
    fn maximum_age(self) -> Duration {
        match self {
            Self::AnthropicHeaders | Self::CodexHeaders => Duration::from_secs(300),
            Self::AnthropicUsage | Self::CodexUsage => Duration::from_secs(300),
        }
    }
}

/// One live response supplies every field. Persistence never supplies this value.
#[derive(Debug)]
pub(super) struct Observation {
    source: Source,
    observed_at: Instant,
    observed_unix: u64,
    exhausted: bool,
    reset: Option<u64>,
}

impl Observation {
    pub(super) fn anthropic(headers: &HeaderMap, at: Instant, unix: u64) -> Option<Self> {
        const STATUS: &str = "anthropic-ratelimit-unified-7d-status";
        const UTILIZATION: &str = "anthropic-ratelimit-unified-7d-utilization";
        const RESET: &str = "anthropic-ratelimit-unified-7d-reset";
        if ![STATUS, UTILIZATION, RESET]
            .iter()
            .any(|name| headers.contains_key(*name))
        {
            return None;
        }
        let evidence = (|| {
            let status = match unique_header::<String>(headers, STATUS)?.as_deref() {
                Some("rejected") => Some(true),
                Some("allowed" | "allowed_warning") => Some(false),
                None => None,
                Some(_) => return Err(()),
            };
            let utilization = unique_header::<f64>(headers, UTILIZATION)?;
            if utilization.is_some_and(|value| !value.is_finite() || value < 0.0) {
                return Err(());
            }
            let utilized = utilization.map(|value| value >= 1.0);
            if status.zip(utilized).is_some_and(|(a, b)| a != b) {
                return Err(());
            }
            Ok((
                status.or(utilized) == Some(true),
                unique_header::<u64>(headers, RESET)?,
            ))
        })();
        Some(Self::new(Source::AnthropicHeaders, at, unix, evidence))
    }

    pub(super) fn codex(headers: &HeaderMap, at: Instant, unix: u64) -> Option<Self> {
        let mut weekly = None;
        let mut invalid = false;
        for (duration, utilization, reset) in [
            (
                "x-codex-primary-window-minutes",
                "x-codex-primary-used-percent",
                "x-codex-primary-reset-at",
            ),
            (
                "x-codex-secondary-window-minutes",
                "x-codex-secondary-used-percent",
                "x-codex-secondary-reset-at",
            ),
        ] {
            let is_weekly = headers.get_all(duration).iter().any(|value| {
                matches!(
                    value
                        .to_str()
                        .ok()
                        .and_then(|value| value.parse::<i64>().ok())
                        .and_then(codex_window_bucket),
                    Some(CodexWindow::Weekly)
                )
            });
            if !is_weekly {
                continue;
            }
            let evidence = (|| {
                unique_header::<i64>(headers, duration)?;
                let percent = unique_header::<f64>(headers, utilization)?.ok_or(())?;
                if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
                    return Err(());
                }
                Ok((percent / 100.0, unique_header::<u64>(headers, reset)?))
            })();
            if let Ok(window) = evidence {
                if weekly.is_some_and(|previous| previous != window) {
                    invalid = true;
                }
                weekly = Some(window);
            } else {
                invalid = true;
            }
        }
        if weekly.is_none() && !invalid {
            return None;
        }
        let evidence = weekly
            .filter(|_| !invalid)
            .map(|(utilization, reset)| (utilization >= 1.0, reset))
            .ok_or(());
        Some(Self::new(Source::CodexHeaders, at, unix, evidence))
    }

    pub(super) fn usage(window: &UsageWindow, source: Source, at: Instant, unix: u64) -> Self {
        let utilization = window.utilization;
        let exhausted = utilization.is_finite()
            && utilization >= 1.0
            && (!matches!(source, Source::CodexUsage) || utilization <= 1.0);
        Self::new(source, at, unix, Ok((exhausted, window.resets_at)))
    }

    fn new(
        source: Source,
        observed_at: Instant,
        observed_unix: u64,
        evidence: Result<(bool, Option<u64>), ()>,
    ) -> Self {
        let (exhausted, reset) = evidence.unwrap_or((false, None));
        Self {
            source,
            observed_at,
            observed_unix,
            exhausted,
            reset,
        }
    }

    fn proves_exhaustion(&self, now: Instant, unix: u64) -> bool {
        self.exhausted
            && unix >= self.observed_unix
            && now
                .checked_duration_since(self.observed_at)
                .is_some_and(|age| age <= self.source.maximum_age())
            && self.reset.is_some_and(|reset| {
                reset > unix && reset.saturating_sub(self.observed_unix) <= WINDOW_7D_SECS
            })
    }
}

fn unique_header<T: std::str::FromStr + PartialEq>(
    headers: &HeaderMap,
    name: &str,
) -> Result<Option<T>, ()> {
    let mut parsed = None;
    for value in headers.get_all(name).iter() {
        let value = value.to_str().map_err(|_| ())?.parse().map_err(|_| ())?;
        if parsed.as_ref().is_some_and(|previous| previous != &value) {
            return Err(());
        }
        parsed = Some(value);
    }
    Ok(parsed)
}

impl AccountPool {
    /// Invalidate strict evidence without an ordinary quota observation.
    pub(crate) fn invalidate_weekly_usage(
        &self,
        provider: &str,
        account: &AccountConfig,
        evidence: &WeeklyUsageEvidence,
    ) {
        if !matches!(evidence, WeeklyUsageEvidence::Invalid) {
            return;
        }
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        if let Some(health) = entries.get_mut(&account_key(provider, account)) {
            health.strict_weekly = None;
        }
    }

    /// Require fresh shared weekly exhaustion for every selected physical account.
    pub fn strict_weekly_exhausted(&self, provider: &str, accounts: &[AccountConfig]) -> bool {
        let entries = self.entries.lock().expect("account health lock poisoned");
        let now = Instant::now();
        let unix = unix_now();
        let mut enabled = collapse_representatives(provider, accounts)
            .into_iter()
            .map(|index| &accounts[index])
            .filter(|account| !account.disabled)
            .peekable();
        enabled.peek().is_some()
            && enabled.all(|account| {
                entries
                    .get(&account_key(provider, account))
                    .and_then(|health| health.strict_weekly.as_ref())
                    .is_some_and(|observation| observation.proves_exhaustion(now, unix))
            })
    }
}

#[cfg(test)]
mod tests;
