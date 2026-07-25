//! `ManagedProvider` wraps one or more [`AnyProvider`] instances — one per
//! configured API key — and adds:
//!
//! - retry on [`super::ProviderError::QuotaExceeded`], honoring
//!   `Retry-After` (capped at [`MAX_RETRY_WAIT`] so a huge daily-quota reset
//!   can't hang the app);
//! - round-robin rotation across keys, skipping any that are in cooldown;
//! - a wait-and-retry fallback when only one key is configured (or when
//!   every key is currently cooling down).
//!
//! None of this requires any change to `GeminiProvider` (or any other
//! provider), `State`, or `Model::translate_texts`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::Language;

use super::{AnyProvider, ProviderError};

/// Hard ceiling on total quota-related attempts (across all keys combined)
/// before giving up for good. Protects against endlessly cycling through
/// keys that are all permanently rate-limited or invalid.
const MAX_QUOTA_ATTEMPTS: u32 = 6;

/// Used when the provider didn't send a `Retry-After` header.
const DEFAULT_RETRY_WAIT: Duration = Duration::from_secs(60);

/// Upper bound on any single wait, regardless of what `Retry-After` says.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(5 * 60);

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
    providers: Vec<Box<dyn AnyProvider>>,
    rotation: Mutex<Rotation>,
}

impl ManagedProvider {
    pub fn new(providers: Vec<Box<dyn AnyProvider>>) -> Self {
        let slots = providers
            .iter()
            .map(|_| SlotState {
                cooldown_until: None,
            })
            .collect();
        Self {
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
            if rotation.slots[idx].cooldown_until.is_none_or(|until| until <= now) {
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
                    // Every key is cooling down: wait for whichever comes
                    // back first, then loop and try again.
                    let now = Instant::now();
                    let wait = self
                        .earliest_cooldown()
                        .unwrap_or(now)
                        .saturating_duration_since(now);
                    tracing::warn!(
                        wait_secs = wait.as_secs(),
                        "all keys cooling down, waiting"
                    );
                    tokio::time::sleep(wait).await;
                    continue;
                }
            };

            let err = match self.providers[index]
                .translate(source, target_language, model, custom_system_prompt)
                .await
            {
                Ok(text) => return Ok(text),
                Err(err) => err,
            };

            let Some(ProviderError::QuotaExceeded { retry_after, .. }) =
                err.downcast_ref::<ProviderError>()
            else {
                // Not a quota error (auth failure, malformed request, etc.)
                // — retrying/rotating wouldn't help, so bail immediately.
                return Err(err);
            };

            attempts += 1;
            if attempts > MAX_QUOTA_ATTEMPTS {
                tracing::warn!(
                    attempts = attempts - 1,
                    "quota exceeded on every attempt, giving up"
                );
                return Err(err);
            }

            let wait = retry_after.unwrap_or(DEFAULT_RETRY_WAIT).min(MAX_RETRY_WAIT);
            self.put_in_cooldown(index, wait);

            if multi_key {
                tracing::warn!(
                    key_index = index,
                    wait_secs = wait.as_secs(),
                    "quota exceeded, rotating to next available key"
                );
                // Loop again — `pick_available_slot` will skip this one
                // until its cooldown expires.
            } else {
                tracing::warn!(
                    attempt = attempts,
                    wait_secs = wait.as_secs(),
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