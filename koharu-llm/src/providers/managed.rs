//! `ManagedProvider` wraps one or more [`AnyProvider`] instances — one per
//! configured API key — and adds:
//!
//! - retry on [`super::ProviderError::QuotaExceeded`], honoring the real
//!   `retry_after` parsed from the provider's error body when available.
//!   The fallback default (when `retry_after` is missing) and the safety-net
//!   cap both depend on the error's [`QuotaKind`]: a per-minute rate limit
//!   gets a short leash ([`DEFAULT_RATE_LIMIT_WAIT`] / [`MAX_RATE_LIMIT_WAIT`]),
//!   while a daily quota is allowed to actually last most of a day
//!   ([`DEFAULT_DAILY_QUOTA_WAIT`] / [`MAX_DAILY_QUOTA_WAIT`]) instead of
//!   being clamped down to a few minutes and re-failing the same 429
//!   repeatedly;
//! - round-robin rotation across keys, skipping any that are in cooldown;
//! - a wait-and-retry fallback when only one key is configured;
//! - a fail-fast [`super::ProviderError::AllProvidersCoolingDown`] error
//!   (instead of blocking) when every key is currently cooling down, so the
//!   caller can requeue the work and move on rather than stalling.
//!
//! None of this requires any change to `GeminiProvider` (or any other
//! provider), `State`, or `Model::translate_texts`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::Language;

use super::{AnyProvider, ProviderError, QuotaKind};

/// Hard ceiling on total quota-related attempts (across all keys combined,
/// within a single `translate()` call) before giving up for good. Protects
/// against endlessly cycling through keys that are all permanently
/// rate-limited or invalid. Keys already in cooldown are skipped by
/// `pick_available_slot` without counting against this — only fresh 429s do.
const MAX_QUOTA_ATTEMPTS: u32 = 6;

/// Fallback wait for a [`QuotaKind::RateLimit`] (or [`QuotaKind::Unknown`])
/// error when the provider didn't send a usable `retryDelay`. 60s matches
/// Gemini's usual per-minute window.
const DEFAULT_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

/// Safety-net cap for a [`QuotaKind::RateLimit`] (or [`QuotaKind::Unknown`])
/// error, regardless of what `retryDelay` says. Rate limits reset fast;
/// anything claiming otherwise is almost certainly a mis-parse, so we don't
/// honor it.
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(5 * 60);

/// Fallback wait for a [`QuotaKind::DailyQuota`] error when the provider
/// didn't send a usable `retryDelay`. There's no reliable "time until the
/// quota resets" available here without a timezone-aware clock, so this is
/// a deliberately conservative middle ground: long enough that we're not
/// hammering a key we already know is dead for a while, short enough that
/// we don't sideline it for a full day purely on a guess.
const DEFAULT_DAILY_QUOTA_WAIT: Duration = Duration::from_secs(60 * 60);

/// Safety-net cap for a [`QuotaKind::DailyQuota`] error. Intentionally much
/// larger than [`MAX_RATE_LIMIT_WAIT`] — Gemini's `retryDelay` for a real
/// daily-quota error can legitimately be several hours, and clamping that
/// down to minutes (the old behavior, when both kinds shared one cap) just
/// meant the key got pulled back into rotation early and re-failed the same
/// 429 over and over instead of actually resting until it recovers.
const MAX_DAILY_QUOTA_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

struct SlotState {
    cooldown_until: Option<Instant>,
}

struct Rotation {
    /// Index of the key to try first on the next call. Sticks with the last
    /// key that worked instead of always starting over at 0.
    current: usize,
    slots: Vec<SlotState>,
}

pub struct ManagedProvider {
    provider_name: String,
    providers: Vec<Box<dyn AnyProvider>>,
    rotation: Mutex<Rotation>,
}

impl ManagedProvider {
    /// `provider_name` is used only for error/log messages (e.g.
    /// `AllProvidersCoolingDown`) — pass whatever id this set of keys
    /// belongs to (e.g. `"gemini"`).
    pub fn new(provider_name: impl Into<String>, providers: Vec<Box<dyn AnyProvider>>) -> Self {
        let slots = providers
            .iter()
            .map(|_| SlotState {
                cooldown_until: None,
            })
            .collect();
        Self {
            provider_name: provider_name.into(),
            providers,
            rotation: Mutex::new(Rotation { current: 0, slots }),
        }
    }

    /// Picks the next non-cooling-down key, starting from `rotation.current`
    /// and wrapping around. Returns `None` if every key is in cooldown.
    fn pick_available_slot(&self) -> Option<usize> {
        let mut rotation = self.rotation.lock().unwrap();
        let total = self.providers.len();
        let now = Instant::now();
        for offset in 0..total {
            let idx = (rotation.current + offset) % total;
            if rotation.slots[idx]
                .cooldown_until
                .is_none_or(|until| until <= now)
            {
                rotation.current = idx;
                return Some(idx);
            }
        }
        None
    }

    /// Earliest time any key's cooldown expires. Used only when every key is
    /// currently in cooldown, to know how long to wait before trying again.
    fn earliest_cooldown(&self) -> Option<Instant> {
        self.rotation
            .lock()
            .unwrap()
            .slots
            .iter()
            .filter_map(|s| s.cooldown_until)
            .min()
    }

    fn put_in_cooldown(&self, index: usize, wait: Duration) {
        self.rotation.lock().unwrap().slots[index].cooldown_until = Some(Instant::now() + wait);
    }

    async fn translate_with_retry(
        &self,
        source: &str,
        target_language: Language,
        model: &str,
        custom_system_prompt: Option<&str>,
    ) -> anyhow::Result<String> {
        if self.providers.is_empty() {
            anyhow::bail!("no provider configured");
        }
        let multi_key = self.providers.len() > 1;
        let mut attempts: u32 = 0;

        loop {
            let index = match self.pick_available_slot() {
                Some(idx) => idx,
                None => {
                    // Every key is cooling down. Previously this slept for
                    // the remaining cooldown and then looped internally —
                    // which meant a single `translate()` call could block
                    // for however long the longest cooldown was, silently,
                    // instead of letting the caller (the page pipeline)
                    // requeue the page and go work on something else in the
                    // meantime. Fase 6: fail fast instead.
                    let now = Instant::now();
                    let wait = self
                        .earliest_cooldown()
                        .unwrap_or(now)
                        .saturating_duration_since(now);
                    tracing::warn!(
                        wait_secs = wait.as_secs(),
                        "all keys cooling down, giving up immediately"
                    );
                    return Err(ProviderError::AllProvidersCoolingDown {
                        provider: self.provider_name.clone(),
                        retry_after: wait,
                    }
                    .into());
                }
            };

            let err = match self.providers[index]
                .translate(source, target_language, model, custom_system_prompt)
                .await
            {
                Ok(text) => return Ok(text),
                Err(err) => err,
            };

            let Some(ProviderError::QuotaExceeded {
                retry_after,
                quota_kind,
                ..
            }) = err.downcast_ref::<ProviderError>()
            else {
                // Not a quota error (auth failure, malformed request, etc.)
                // — retrying/rotating wouldn't help, so bail immediately.
                return Err(err);
            };

            let retry_after = *retry_after;
            let quota_kind = *quota_kind;

            attempts += 1;
            if attempts > MAX_QUOTA_ATTEMPTS {
                tracing::warn!(
                    attempts = attempts - 1,
                    "quota exceeded on every attempt, giving up"
                );
                return Err(err);
            }

            // Prefer the real `retry_after` Gemini gave us — it's accurate
            // per-key, whereas the default below is just a blind guess.
            // Which default/cap applies depends on *why* the key failed:
            // a per-minute rate limit should come back fast; a daily quota
            // should not. See the module-level doc comment.
            let (default_wait, max_wait) = match quota_kind {
                QuotaKind::DailyQuota => (DEFAULT_DAILY_QUOTA_WAIT, MAX_DAILY_QUOTA_WAIT),
                QuotaKind::RateLimit | QuotaKind::Unknown => {
                    (DEFAULT_RATE_LIMIT_WAIT, MAX_RATE_LIMIT_WAIT)
                }
            };
            let wait = retry_after.unwrap_or(default_wait).min(max_wait);
            self.put_in_cooldown(index, wait);

            if multi_key {
                tracing::info!(
                    key_index = index,
                    wait_secs = wait.as_secs(),
                    ?quota_kind,
                    "quota exceeded, rotating to next available key"
                );
                // Loop again — `pick_available_slot` will skip this one
                // until its cooldown expires.
            } else {
                tracing::debug!(
                    attempt = attempts,
                    wait_secs = wait.as_secs(),
                    ?quota_kind,
                    "quota exceeded, waiting before retry"
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
}

impl AnyProvider for ManagedProvider {
    fn translate<'a>(
        &'a self,
        source: &'a str,
        target_language: Language,
        model: &'a str,
        custom_system_prompt: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'a>> {
        Box::pin(async move {
            self.translate_with_retry(source, target_language, model, custom_system_prompt)
                .await
        })
    }
}
