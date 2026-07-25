//! `ManagedProvider` wraps any [`AnyProvider`] and, in later phases, will add
//! retry/backoff on [`super::ProviderError::QuotaExceeded`], multi-key
//! rotation, cooldown tracking, and diagnostics logging — without requiring
//! any change to `GeminiProvider` (or any other provider), `State`, or
//! `Model::translate_texts`.
//!
//! For now it only delegates, so the wrapper exists in the call path before
//! any behavior changes are added on top of it.

use std::future::Future;
use std::pin::Pin;

use crate::Language;

use super::AnyProvider;

pub struct ManagedProvider {
    provider: Box<dyn AnyProvider>,
}

impl ManagedProvider {
    pub fn new(provider: Box<dyn AnyProvider>) -> Self {
        Self { provider }
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
        self.provider
            .translate(source, target_language, model, custom_system_prompt)
    }
}