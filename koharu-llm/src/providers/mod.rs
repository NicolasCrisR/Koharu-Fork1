use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use reqwest_middleware::ClientWithMiddleware;

use crate::prompt::{BLOCK_TAG_INSTRUCTIONS, system_prompt};
use crate::{Language, language::tags as language_tags, supported_locales};

/// Resolve the effective system prompt: custom (with block instructions appended) or default.
pub(crate) fn resolve_system_prompt(custom: Option<&str>, target_language: Language) -> String {
    match custom {
        Some(p) if !p.trim().is_empty() => format!("{p} {BLOCK_TAG_INSTRUCTIONS}"),
        _ => system_prompt(target_language),
    }
}

pub mod caiyun;
mod chat_completions;
pub mod claude;
pub mod deepl;
pub mod deepseek;
pub mod gemini;
pub mod google_translate;
pub mod managed;
pub mod openai;
pub mod openai_compatible;

pub use managed::ManagedProvider;

#[derive(Debug, Clone, Copy)]
pub struct ProviderModelDescriptor {
    pub id: &'static str,
    pub name: &'static str,
}

#[derive(Debug, Clone)]
pub struct DiscoveredProviderModel {
    pub id: String,
    pub name: String,
}

pub type ProviderDiscoveryFuture =
    Pin<Box<dyn Future<Output = anyhow::Result<Vec<DiscoveredProviderModel>>> + Send>>;

pub enum ProviderCatalogModels {
    Static(&'static [ProviderModelDescriptor]),
    Dynamic(fn(ProviderConfig) -> ProviderDiscoveryFuture),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSupportedLanguages {
    All,
    Limited(&'static [Language]),
}

impl ProviderSupportedLanguages {
    pub fn tags(self) -> Vec<String> {
        match self {
            Self::All => supported_locales(),
            Self::Limited(languages) => language_tags(languages),
        }
    }
}

pub struct ProviderDescriptor {
    pub id: &'static str,
    pub name: &'static str,
    pub requires_api_key: bool,
    pub requires_base_url: bool,
    pub supported_languages: ProviderSupportedLanguages,
    pub models: ProviderCatalogModels,
    pub build: fn(ProviderConfig) -> anyhow::Result<Box<dyn AnyProvider>>,
}
/// What kind of quota limit a 429 corresponds to, per Gemini's
/// `QuotaFailure.violations[].quotaId` (falls back to a duration heuristic,
/// or `Unknown`, when that field is absent or unrecognized).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaKind {
    /// Per-minute (or otherwise short-window) rate limit.
    RateLimit,
    /// Per-day quota. NOTE: observed in practice (see `retry_after` below)
    /// this does *not* imply a multi-hour wait — Gemini's free tier sends
    /// `quotaId: GenerateRequestsPerDayPerProjectPerModel-FreeTier` together
    /// with a `retryDelay` of well under a minute. Always prefer the actual
    /// `retry_after` value over assumptions based on this variant.
    DailyQuota,
    /// Couldn't determine the kind (no recognizable `quotaId`, and no
    /// `retry_after` to fall back on either).
    Unknown,
}

impl QuotaKind {
    /// Classify from Gemini's `quotaId` (e.g.
    /// `GenerateRequestsPerMinutePerProjectPerModel-FreeTier` vs
    /// `GenerateRequestsPerDayPerProjectPerModel-FreeTier`), falling back to
    /// a duration heuristic when `quota_id` is absent/unrecognized: anything
    /// under 10 minutes is assumed to be a rate limit, since we've observed
    /// "daily" quota errors reporting short `retryDelay`s in practice.
    fn classify(quota_id: Option<&str>, retry_after: Option<Duration>) -> Self {
        if let Some(id) = quota_id {
            let lower = id.to_ascii_lowercase();
            if lower.contains("perminute") || lower.contains("per_minute") {
                return Self::RateLimit;
            }
            if lower.contains("perday") || lower.contains("per_day") {
                return Self::DailyQuota;
            }
        }
        match retry_after {
            Some(wait) if wait < Duration::from_secs(10 * 60) => Self::RateLimit,
            Some(_) => Self::DailyQuota,
            None => Self::Unknown,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("quota exceeded for provider {provider}")]
    QuotaExceeded {
        provider: String,
        retry_after: Option<Duration>,
        quota_kind: QuotaKind,
    },
    /// Every key for this provider is currently in cooldown. Returned
    /// immediately (Fase 6) instead of silently burning quota-retry
    /// attempts scanning keys that are all known to be unavailable.
    #[error("all keys cooling down for provider {provider}, next available in {:?}", .retry_after)]
    AllProvidersCoolingDown {
        provider: String,
        retry_after: Duration,
    },
    #[error("{provider} API request failed ({status}): {body}")]
    Fatal {
        provider: String,
        status: u16,
        body: String,
    },
}

/// Gemini's error body shape (also used by other Google APIs), e.g.:
/// ```json
/// { "error": { "code": 429, "message": "...", "status": "RESOURCE_EXHAUSTED",
///   "details": [
///     { "@type": ".../google.rpc.QuotaFailure",
///       "violations": [{ "quotaId": "GenerateRequestsPerDayPerProjectPerModel-FreeTier", ... }] },
///     { "@type": ".../google.rpc.RetryInfo", "retryDelay": "55s" }
///   ] } }
/// ```
/// Both `details` entries are optional and may be absent depending on the
/// error; every field here is therefore best-effort.
#[derive(Debug, serde::Deserialize)]
struct GoogleErrorBody {
    error: Option<GoogleErrorInner>,
}

#[derive(Debug, serde::Deserialize)]
struct GoogleErrorInner {
    #[serde(default)]
    details: Vec<serde_json::Value>,
}

/// Extracts `retry_after` (from `google.rpc.RetryInfo.retryDelay`, e.g.
/// `"55s"`) and the first `quotaId` (from `google.rpc.QuotaFailure`) out of
/// a Gemini-style JSON error body. Returns `(None, None)` if the body isn't
/// JSON, isn't this shape, or doesn't carry these fields — callers should
/// treat that as "unknown", not as an error, since not every provider (or
/// every error from Gemini) uses this format.
fn parse_google_error_details(body: &str) -> (Option<Duration>, Option<String>) {
    let Ok(parsed) = serde_json::from_str::<GoogleErrorBody>(body) else {
        return (None, None);
    };
    let Some(inner) = parsed.error else {
        return (None, None);
    };

    let mut retry_after = None;
    let mut quota_id = None;

    for detail in &inner.details {
        let type_field = detail.get("@type").and_then(|v| v.as_str()).unwrap_or("");

        if type_field.ends_with("RetryInfo")
            && let Some(delay) = detail.get("retryDelay").and_then(|v| v.as_str())
        {
            retry_after = parse_retry_delay(delay);
        }

        if type_field.ends_with("QuotaFailure")
            && let Some(violations) = detail.get("violations").and_then(|v| v.as_array())
            && let Some(first) = violations.first()
            && let Some(id) = first.get("quotaId").and_then(|v| v.as_str())
        {
            quota_id = Some(id.to_string());
        }
    }

    (retry_after, quota_id)
}

/// Parses a Google-style `retryDelay` string, e.g. `"55s"` or
/// `"1.500s"` (seconds, with an optional fractional part, per the
/// `google.protobuf.Duration` JSON mapping).
fn parse_retry_delay(value: &str) -> Option<Duration> {
    let seconds_str = value.strip_suffix('s')?;
    let seconds: f64 = seconds_str.parse().ok()?;
    if seconds.is_finite() && seconds >= 0.0 {
        Some(Duration::from_secs_f64(seconds))
    } else {
        None
    }
}

pub async fn ensure_provider_success(
    provider: &str,
    response: reqwest::Response,
) -> anyhow::Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    // `Retry-After` is the HTTP-standard place for this, but Gemini (and
    // most Google APIs) don't set it — the real info lives in the JSON
    // body's `RetryInfo`/`QuotaFailure` details (see
    // `parse_google_error_details` above). We still check the header first
    // since it costs nothing and some other provider might use it; the body
    // is the primary source now.
    let header_retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs);

    let body = response
        .text()
        .await
        .with_context(|| format!("Failed to read {provider} error response body"))?;
    let body_lower = body.to_ascii_lowercase();
    let quota_exceeded = status.as_u16() == 429
        || body_lower.contains("insufficient_quota")
        || body_lower.contains("quota")
        || body_lower.contains("resource_exhausted")
        || body_lower.contains("rate limit exceeded")
        || body_lower.contains("credit balance is too low");

    if quota_exceeded {
        let (body_retry_after, quota_id) = parse_google_error_details(&body);
        // Prefer the body's `retryDelay` — it's what Gemini actually sends —
        // then the header, then leave it to the caller's default.
        let retry_after = body_retry_after.or(header_retry_after);
        let quota_kind = QuotaKind::classify(quota_id.as_deref(), retry_after);

        tracing::debug!(
            %provider,
            ?retry_after,
            ?quota_kind,
            quota_id = quota_id.as_deref().unwrap_or("none"),
            "quota exceeded, parsed details from error body"
        );

        return Err(ProviderError::QuotaExceeded {
            provider: provider.to_string(),
            retry_after,
            quota_kind,
        }
        .into());
    }

    Err(ProviderError::Fatal {
        provider: provider.to_string(),
        status: status.as_u16(),
        body,
    }
    .into())
}

pub trait AnyProvider: Send + Sync {
    fn translate<'a>(
        &'a self,
        source: &'a str,
        target_language: Language,
        model: &'a str,
        custom_system_prompt: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'a>>;
}

#[derive(Clone)]
pub struct ProviderConfig {
    pub http_client: Arc<ClientWithMiddleware>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
}

const OPENAI_MODELS: &[ProviderModelDescriptor] = &[
    ProviderModelDescriptor {
        id: "gpt-5.5",
        name: "GPT-5.5",
    },
    ProviderModelDescriptor {
        id: "gpt-5.4",
        name: "GPT-5.4",
    },
    ProviderModelDescriptor {
        id: "gpt-5.4-mini",
        name: "GPT-5.4 mini",
    },
    ProviderModelDescriptor {
        id: "gpt-5.4-nano",
        name: "GPT-5.4 nano",
    },
    ProviderModelDescriptor {
        id: "gpt-5.2",
        name: "GPT-5.2",
    },
    ProviderModelDescriptor {
        id: "gpt-5.1",
        name: "GPT-5.1",
    },
    ProviderModelDescriptor {
        id: "gpt-5",
        name: "GPT-5",
    },
    ProviderModelDescriptor {
        id: "gpt-5-mini",
        name: "GPT-5 mini",
    },
    ProviderModelDescriptor {
        id: "gpt-5-nano",
        name: "GPT-5 nano",
    },
    ProviderModelDescriptor {
        id: "gpt-5-chat-latest",
        name: "GPT-5 Chat latest",
    },
    ProviderModelDescriptor {
        id: "gpt-4.1",
        name: "GPT-4.1",
    },
    ProviderModelDescriptor {
        id: "gpt-4.1-mini",
        name: "GPT-4.1 mini",
    },
    ProviderModelDescriptor {
        id: "gpt-4.1-nano",
        name: "GPT-4.1 nano",
    },
    ProviderModelDescriptor {
        id: "o3",
        name: "o3",
    },
    ProviderModelDescriptor {
        id: "o4-mini",
        name: "o4-mini",
    },
    ProviderModelDescriptor {
        id: "o3-mini",
        name: "o3-mini",
    },
    ProviderModelDescriptor {
        id: "o1",
        name: "o1",
    },
    ProviderModelDescriptor {
        id: "o1-mini",
        name: "o1-mini",
    },
    ProviderModelDescriptor {
        id: "o1-preview",
        name: "o1 preview",
    },
    ProviderModelDescriptor {
        id: "gpt-4o",
        name: "GPT-4o",
    },
    ProviderModelDescriptor {
        id: "gpt-4o-mini",
        name: "GPT-4o mini",
    },
    ProviderModelDescriptor {
        id: "gpt-4-turbo",
        name: "GPT-4 Turbo",
    },
    ProviderModelDescriptor {
        id: "gpt-4",
        name: "GPT-4",
    },
    ProviderModelDescriptor {
        id: "gpt-3.5-turbo",
        name: "GPT-3.5 Turbo",
    },
];

const GEMINI_MODELS: &[ProviderModelDescriptor] = &[
    ProviderModelDescriptor {
        id: "gemini-flash-lite-latest",
        name: "Gemini Flash-Lite Latest",
    },
    ProviderModelDescriptor {
        id: "gemini-flash-latest",
        name: "Gemini Flash Latest",
    },
    ProviderModelDescriptor {
        id: "gemini-pro-latest",
        name: "Gemini Pro Latest",
    },
    ProviderModelDescriptor {
        id: "gemini-3.5-flash",
        name: "Gemini 3.5 Flash",
    },
    ProviderModelDescriptor {
        id: "gemini-3.1-pro-preview",
        name: "Gemini 3.1 Pro Preview",
    },
    ProviderModelDescriptor {
        id: "gemini-3.1-pro-preview-customtools",
        name: "Gemini 3.1 Pro Preview Custom Tools",
    },
    ProviderModelDescriptor {
        id: "gemini-3.1-flash-lite",
        name: "Gemini 3.1 Flash-Lite",
    },
    ProviderModelDescriptor {
        id: "gemini-3-flash-preview",
        name: "Gemini 3 Flash Preview",
    },
    ProviderModelDescriptor {
        id: "gemini-2.5-pro",
        name: "Gemini 2.5 Pro",
    },
    ProviderModelDescriptor {
        id: "gemini-2.5-flash",
        name: "Gemini 2.5 Flash",
    },
    ProviderModelDescriptor {
        id: "gemini-2.5-flash-lite",
        name: "Gemini 2.5 Flash-Lite",
    },
    ProviderModelDescriptor {
        id: "gemini-2.0-flash",
        name: "Gemini 2.0 Flash",
    },
    ProviderModelDescriptor {
        id: "gemini-2.0-flash-001",
        name: "Gemini 2.0 Flash 001",
    },
    ProviderModelDescriptor {
        id: "gemini-2.0-flash-lite",
        name: "Gemini 2.0 Flash-Lite",
    },
    ProviderModelDescriptor {
        id: "gemini-2.0-flash-lite-001",
        name: "Gemini 2.0 Flash-Lite 001",
    },
    ProviderModelDescriptor {
        id: "gemma-4-31b-it",
        name: "Gemma 4 31B",
    },
    ProviderModelDescriptor {
        id: "gemma-4-26b-a4b-it",
        name: "Gemma 4 26B",
    },
];

const CLAUDE_MODELS: &[ProviderModelDescriptor] = &[
    ProviderModelDescriptor {
        id: "claude-opus-4-7",
        name: "Claude Opus 4.7",
    },
    ProviderModelDescriptor {
        id: "claude-sonnet-4-6",
        name: "Claude Sonnet 4.6",
    },
    ProviderModelDescriptor {
        id: "claude-haiku-4-5",
        name: "Claude Haiku 4.5",
    },
    ProviderModelDescriptor {
        id: "claude-opus-4-6",
        name: "Claude Opus 4.6",
    },
    ProviderModelDescriptor {
        id: "claude-opus-4-5-20251101",
        name: "Claude Opus 4.5",
    },
    ProviderModelDescriptor {
        id: "claude-opus-4-1-20250805",
        name: "Claude Opus 4.1",
    },
    ProviderModelDescriptor {
        id: "claude-sonnet-4-5-20250929",
        name: "Claude Sonnet 4.5",
    },
    ProviderModelDescriptor {
        id: "claude-haiku-4-5-20251001",
        name: "Claude Haiku 4.5 snapshot",
    },
    ProviderModelDescriptor {
        id: "claude-opus-4-20250514",
        name: "Claude Opus 4 (deprecated)",
    },
    ProviderModelDescriptor {
        id: "claude-sonnet-4-20250514",
        name: "Claude Sonnet 4 (deprecated)",
    },
];

const DEEPSEEK_MODELS: &[ProviderModelDescriptor] = &[
    ProviderModelDescriptor {
        id: "deepseek-v4-flash",
        name: "DeepSeek V4 Flash",
    },
    ProviderModelDescriptor {
        id: "deepseek-v4-pro",
        name: "DeepSeek V4 Pro",
    },
    ProviderModelDescriptor {
        id: "deepseek-chat",
        name: "DeepSeek Chat",
    },
    ProviderModelDescriptor {
        id: "deepseek-reasoner",
        name: "DeepSeek Reasoner",
    },
];

const MT_MODELS: &[ProviderModelDescriptor] = &[ProviderModelDescriptor {
    id: "mt",
    name: "Machine Translation",
}];

const PROVIDERS: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        id: "openai",
        name: "OpenAI",
        requires_api_key: true,
        requires_base_url: false,
        supported_languages: ProviderSupportedLanguages::All,
        models: ProviderCatalogModels::Static(OPENAI_MODELS),
        build: build_openai_provider,
    },
    ProviderDescriptor {
        id: "gemini",
        name: "Gemini",
        requires_api_key: true,
        requires_base_url: false,
        supported_languages: ProviderSupportedLanguages::All,
        models: ProviderCatalogModels::Static(GEMINI_MODELS),
        build: build_gemini_provider,
    },
    ProviderDescriptor {
        id: "claude",
        name: "Claude",
        requires_api_key: true,
        requires_base_url: false,
        supported_languages: ProviderSupportedLanguages::All,
        models: ProviderCatalogModels::Static(CLAUDE_MODELS),
        build: build_claude_provider,
    },
    ProviderDescriptor {
        id: "deepseek",
        name: "DeepSeek",
        requires_api_key: true,
        requires_base_url: false,
        supported_languages: ProviderSupportedLanguages::All,
        models: ProviderCatalogModels::Static(DEEPSEEK_MODELS),
        build: build_deepseek_provider,
    },
    ProviderDescriptor {
        id: "deepl",
        name: "DeepL",
        requires_api_key: true,
        requires_base_url: false,
        supported_languages: ProviderSupportedLanguages::All,
        models: ProviderCatalogModels::Static(MT_MODELS),
        build: build_deepl_mt_provider,
    },
    ProviderDescriptor {
        id: "google-translate",
        name: "Google Cloud Translation",
        requires_api_key: true,
        requires_base_url: false,
        supported_languages: ProviderSupportedLanguages::All,
        models: ProviderCatalogModels::Static(MT_MODELS),
        build: build_google_translate_mt_provider,
    },
    ProviderDescriptor {
        id: "caiyun",
        name: "Caiyun",
        requires_api_key: true,
        requires_base_url: false,
        supported_languages: ProviderSupportedLanguages::Limited(
            caiyun::SUPPORTED_TARGET_LANGUAGES,
        ),
        models: ProviderCatalogModels::Static(MT_MODELS),
        build: build_caiyun_mt_provider,
    },
    ProviderDescriptor {
        id: "openai-compatible",
        name: "OpenAI-compatible",
        requires_api_key: false,
        requires_base_url: true,
        supported_languages: ProviderSupportedLanguages::All,
        models: ProviderCatalogModels::Dynamic(discover_openai_compatible_models),
        build: build_openai_compatible_provider,
    },
];

pub fn all_provider_descriptors() -> &'static [ProviderDescriptor] {
    PROVIDERS
}

pub fn find_provider_descriptor(provider_id: &str) -> Option<&'static ProviderDescriptor> {
    PROVIDERS
        .iter()
        .find(|descriptor| descriptor.id == provider_id)
}

pub fn discover_models(
    provider_id: &str,
    config: ProviderConfig,
) -> anyhow::Result<ProviderDiscoveryFuture> {
    let descriptor = find_provider_descriptor(provider_id)
        .ok_or_else(|| anyhow::anyhow!("Unknown API provider: {provider_id}"))?;
    Ok(match descriptor.models {
        ProviderCatalogModels::Static(models) => {
            let models = models
                .iter()
                .map(|model| DiscoveredProviderModel {
                    id: model.id.to_string(),
                    name: model.name.to_string(),
                })
                .collect::<Vec<_>>();
            Box::pin(async move { Ok(models) })
        }
        ProviderCatalogModels::Dynamic(discover) => discover(config),
    })
}

pub fn build_provider(
    provider_id: &str,
    config: ProviderConfig,
) -> anyhow::Result<Box<dyn AnyProvider>> {
    let descriptor = find_provider_descriptor(provider_id)
        .ok_or_else(|| anyhow::anyhow!("Unknown API provider: {provider_id}"))?;

    if descriptor.requires_api_key
        && config
            .api_key
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        anyhow::bail!("api_key is required for {}", descriptor.id);
    }

    if descriptor.requires_base_url
        && config
            .base_url
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        anyhow::bail!("base_url is required for {}", descriptor.id);
    }

    (descriptor.build)(config)
}

fn required_api_key(config: &ProviderConfig, provider_id: &str) -> anyhow::Result<String> {
    config
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("api_key is required for {provider_id}"))
}

fn required_base_url(config: &ProviderConfig, provider_id: &str) -> anyhow::Result<String> {
    config
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("base_url is required for {provider_id}"))
}

fn build_openai_provider(config: ProviderConfig) -> anyhow::Result<Box<dyn AnyProvider>> {
    Ok(Box::new(openai::OpenAiProvider {
        http_client: Arc::clone(&config.http_client),
        api_key: required_api_key(&config, "openai")?,
    }))
}

fn build_gemini_provider(config: ProviderConfig) -> anyhow::Result<Box<dyn AnyProvider>> {
    Ok(Box::new(gemini::GeminiProvider {
        http_client: Arc::clone(&config.http_client),
        api_key: required_api_key(&config, "gemini")?,
    }))
}

fn build_claude_provider(config: ProviderConfig) -> anyhow::Result<Box<dyn AnyProvider>> {
    Ok(Box::new(claude::ClaudeProvider {
        http_client: Arc::clone(&config.http_client),
        api_key: required_api_key(&config, "claude")?,
    }))
}

fn build_deepseek_provider(config: ProviderConfig) -> anyhow::Result<Box<dyn AnyProvider>> {
    Ok(Box::new(deepseek::DeepSeekProvider {
        http_client: Arc::clone(&config.http_client),
        api_key: required_api_key(&config, "deepseek")?,
    }))
}

fn build_openai_compatible_provider(
    config: ProviderConfig,
) -> anyhow::Result<Box<dyn AnyProvider>> {
    Ok(Box::new(openai_compatible::OpenAiCompatibleProvider {
        http_client: Arc::clone(&config.http_client),
        base_url: required_base_url(&config, "openai-compatible")?,
        api_key: config.api_key,
        temperature: config.temperature,
        max_tokens: config.max_tokens,
    }))
}

fn build_deepl_mt_provider(config: ProviderConfig) -> anyhow::Result<Box<dyn AnyProvider>> {
    Ok(Box::new(deepl::DeeplMtProvider {
        http_client: Arc::clone(&config.http_client),
        api_key: required_api_key(&config, "deepl")?,
        base_url: config.base_url,
    }))
}

fn build_google_translate_mt_provider(
    config: ProviderConfig,
) -> anyhow::Result<Box<dyn AnyProvider>> {
    Ok(Box::new(google_translate::GoogleTranslateMtProvider {
        http_client: Arc::clone(&config.http_client),
        api_key: required_api_key(&config, "google-translate")?,
    }))
}

fn build_caiyun_mt_provider(config: ProviderConfig) -> anyhow::Result<Box<dyn AnyProvider>> {
    Ok(Box::new(caiyun::CaiyunMtProvider {
        http_client: Arc::clone(&config.http_client),
        api_key: required_api_key(&config, "caiyun")?,
    }))
}

fn discover_openai_compatible_models(config: ProviderConfig) -> ProviderDiscoveryFuture {
    Box::pin(async move {
        let base_url = required_base_url(&config, "openai-compatible")?;
        let models = openai_compatible::list_models(
            config.http_client,
            &base_url,
            config.api_key.as_deref(),
        )
        .await?;
        Ok(models
            .into_iter()
            .map(|id| DiscoveredProviderModel {
                name: id.clone(),
                id,
            })
            .collect())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(models: &[ProviderModelDescriptor]) -> Vec<&'static str> {
        models.iter().map(|model| model.id).collect()
    }

    fn assert_contains_all(provider: &str, models: &[ProviderModelDescriptor], expected: &[&str]) {
        let ids = ids(models);
        for expected_id in expected {
            assert!(
                ids.contains(expected_id),
                "{provider} model catalog should include {expected_id}"
            );
        }
    }

    #[test]
    fn static_llm_provider_catalogs_cover_current_model_families() {
        assert_contains_all(
            "openai",
            OPENAI_MODELS,
            &[
                "gpt-5.5",
                "gpt-5.4-mini",
                "gpt-5-mini",
                "gpt-4.1",
                "gpt-4o",
                "o3",
            ],
        );
        assert_contains_all(
            "gemini",
            GEMINI_MODELS,
            &[
                "gemini-3.1-pro-preview",
                "gemini-3.1-flash-lite",
                "gemini-3.5-flash",
                "gemma-4-26b-a4b-it",
            ],
        );
        assert_contains_all(
            "claude",
            CLAUDE_MODELS,
            &["claude-opus-4-7", "claude-sonnet-4-6", "claude-haiku-4-5"],
        );
        assert_contains_all(
            "deepseek",
            DEEPSEEK_MODELS,
            &[
                "deepseek-v4-flash",
                "deepseek-v4-pro",
                "deepseek-chat",
                "deepseek-reasoner",
            ],
        );
    }
}
