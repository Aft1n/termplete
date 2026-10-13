//! Warm decision client with bounded caching and a short failure cooldown.
//!
//! Failures remain explicit; the caller decides whether a safe prompt-only
//! Hybrid fallback is permitted by the configured privacy policy.

use std::collections::{hash_map::DefaultHasher, VecDeque};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::config::{Config, ProviderPreset};
use crate::decision::{CandidateEvidence, Decision};
use crate::provider::{ChatRequest, Client};
use crate::{auth, Error, Result};

const CACHE_SIZE: usize = 32;
const CACHE_LIFETIME: Duration = Duration::from_secs(3);
const FAILURE_COOLDOWN: Duration = Duration::from_secs(2);

struct Cached {
    key: u64,
    at: Instant,
    value: Decision,
}

/// Per-daemon decision state; nothing here is persisted or shared externally.
pub struct Runtime {
    provider: Result<ProviderPreset, String>,
    model: String,
    client: Client,
    credential: Mutex<Option<std::result::Result<Option<String>, String>>>,
    cache: Mutex<VecDeque<Cached>>,
    failed_at: Mutex<Option<Instant>>,
    in_flight: Mutex<()>,
    review_private_metadata: bool,
}

impl Runtime {
    pub fn new(cfg: &Config) -> Self {
        let timeout = Duration::from_millis(cfg.decision.timeout_ms.clamp(100, 10_000));
        let provider = cfg.decision_provider().map_err(|error| error.to_string());
        let review_private_metadata = cfg.decision.active()
            && cfg.privacy.sensitive_context == crate::config::SensitiveContext::Decision
            && provider
                .as_ref()
                .is_ok_and(|preset| crate::provider::is_local_endpoint(&preset.base_url));
        Self {
            provider,
            model: cfg.decision.model.clone(),
            // The decision client itself bypasses proxies for loopback URLs.
            client: Client::with_timeout(timeout),
            credential: Mutex::new(None),
            cache: Mutex::new(VecDeque::new()),
            failed_at: Mutex::new(None),
            in_flight: Mutex::new(()),
            review_private_metadata,
        }
    }

    pub fn reviews_private_metadata(&self) -> bool {
        self.review_private_metadata
    }

    pub fn evaluate(&self, request: &ChatRequest, candidates: &[String]) -> Result<Decision> {
        self.evaluate_with_evidence(request, candidates, &[])
    }

    /// Evaluate a request with the caller's local evidence for each candidate.
    /// Evidence is aligned with candidate indexes and participates in caching.
    pub fn evaluate_with_evidence(
        &self,
        request: &ChatRequest,
        candidates: &[String],
        evidence: &[CandidateEvidence],
    ) -> Result<Decision> {
        let provider = self
            .provider
            .as_ref()
            .map_err(|issue| Error::Provider(issue.clone()))?;
        let key = cache_key_with_evidence(request, candidates, evidence);
        if let Some(value) = self.cached(key) {
            return Ok(value);
        }
        if self
            .failed_at
            .lock()
            .is_some_and(|at| at.elapsed() < FAILURE_COOLDOWN)
        {
            return Err(Error::Provider(
                "Decision helper is cooling down after a failed request.".into(),
            ));
        }
        // Avoid letting one slow model serialize every terminal's keystrokes.
        let Some(_running) = self.in_flight.try_lock() else {
            return Err(Error::Provider("Decision helper is busy.".into()));
        };
        if let Some(value) = self.cached(key) {
            return Ok(value);
        }
        let credential = {
            let mut cached = self.credential.lock();
            cached
                .get_or_insert_with(|| {
                    if provider.no_auth {
                        Ok(None)
                    } else {
                        auth::require(provider, None).map_err(|error| error.to_string())
                    }
                })
                .clone()
                .map_err(Error::Auth)?
        };
        let result = crate::decision::decide_with_review_and_evidence(
            &self.client,
            provider,
            &self.model,
            credential.as_deref(),
            request,
            candidates,
            evidence,
            self.review_private_metadata,
        );
        match result {
            Ok(value) => {
                *self.failed_at.lock() = None;
                let mut cache = self.cache.lock();
                cache.retain(|entry| entry.at.elapsed() < CACHE_LIFETIME);
                if cache.len() >= CACHE_SIZE {
                    cache.pop_front();
                }
                cache.push_back(Cached {
                    key,
                    at: Instant::now(),
                    value: value.clone(),
                });
                Ok(value)
            }
            Err(error) => {
                *self.failed_at.lock() = Some(Instant::now());
                // API errors can echo request payloads. Keep diagnostics generic.
                let message = match error {
                    Error::Network(_) => "Decision helper could not connect or respond within its latency budget.",
                    _ => "Decision helper returned an unavailable or invalid decision. Check its provider, model and API version.",
                };
                Err(Error::Provider(message.into()))
            }
        }
    }

    fn cached(&self, key: u64) -> Option<Decision> {
        self.cache
            .lock()
            .iter()
            .find(|entry| entry.key == key && entry.at.elapsed() < CACHE_LIFETIME)
            .map(|entry| entry.value.clone())
    }
}

fn cache_key_with_evidence(
    request: &ChatRequest,
    candidates: &[String],
    evidence: &[CandidateEvidence],
) -> u64 {
    let mut hasher = DefaultHasher::new();
    for message in &request.messages {
        serde_json::to_string(message)
            .expect("message serialization")
            .hash(&mut hasher);
    }
    candidates.hash(&mut hasher);
    evidence.len().hash(&mut hasher);
    for item in evidence {
        item.cache_identity().hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    #[test]
    fn private_review_requires_an_active_loopback_helper() {
        let mut cfg = Config::default();
        cfg.privacy.sensitive_context = crate::config::SensitiveContext::Decision;
        assert!(!Runtime::new(&cfg).reviews_private_metadata());
        cfg.decision.strategy = crate::config::DecisionStrategy::Hybrid;
        assert!(Runtime::new(&cfg).reviews_private_metadata());
        cfg.decision.enabled = false;
        assert!(!Runtime::new(&cfg).reviews_private_metadata());
        cfg.decision.enabled = true;
        cfg.decision.local_only = false;
        cfg.decision.base_url = Some("https://judge.example/v1".into());
        assert!(!Runtime::new(&cfg).reviews_private_metadata());
    }

    #[test]
    fn cache_changes_with_context_or_candidates() {
        let first = ChatRequest::new("", vec![Message::user("docker\nproject-a")]);
        let second = ChatRequest::new("", vec![Message::user("docker\nproject-b")]);
        let candidates = vec!["docker ps".into()];
        assert_ne!(
            cache_key_with_evidence(&first, &candidates, &[]),
            cache_key_with_evidence(&second, &candidates, &[])
        );
        assert_ne!(
            cache_key_with_evidence(&first, &candidates, &[]),
            cache_key_with_evidence(&first, &["docker images".into()], &[])
        );
    }

    #[test]
    fn cache_ignores_small_age_drift_but_tracks_source_and_score_changes() {
        let request = ChatRequest::new("", vec![Message::user("git st")]);
        let candidates = vec!["git status".into()];
        let recent = [CandidateEvidence {
            uses: 2,
            last_used_age_ms: Some(1_000),
            accepts: 2,
            last_accepted_age_ms: Some(2_000),
            specificity: 1,
            local_score: 250,
            freshness: Some(80),
            local_confidence: 90,
            context_match: true,
            context_age_ms: Some(1_000),
            project_scope: true,
            project_match: true,
            project_uses: 2,
            project_last_used_age_ms: Some(1_000),
            project_accepts: 2,
            project_last_accepted_age_ms: Some(2_000),
            project_score: 500,
            source_last_used_ms: 10_000,
            source_last_accepted_ms: 9_000,
            source_context_last_seen: 8_000,
            source_project_last_used_ms: 10_000,
            source_project_last_accepted_ms: 9_000,
        }];
        let age_drift = [CandidateEvidence {
            last_used_age_ms: Some(1_300),
            last_accepted_age_ms: Some(2_300),
            context_age_ms: Some(1_300),
            ..recent[0]
        }];
        let newer_source = [CandidateEvidence {
            source_last_used_ms: 10_300,
            ..recent[0]
        }];
        let changed_uses = [CandidateEvidence {
            uses: 3,
            ..recent[0]
        }];
        let changed_accepts = [CandidateEvidence {
            accepts: 3,
            ..recent[0]
        }];
        let changed_score = [CandidateEvidence {
            local_score: 251,
            ..recent[0]
        }];
        let changed_context_match = [CandidateEvidence {
            context_match: false,
            context_age_ms: None,
            ..recent[0]
        }];
        let changed_project_source = [CandidateEvidence {
            source_project_last_used_ms: 10_300,
            ..recent[0]
        }];
        let newer_context_source = [CandidateEvidence {
            source_context_last_seen: 8_300,
            ..recent[0]
        }];
        let base = cache_key_with_evidence(&request, &candidates, &recent);
        assert_eq!(
            base,
            cache_key_with_evidence(&request, &candidates, &age_drift)
        );
        for changed in [
            newer_source,
            changed_uses,
            changed_accepts,
            changed_score,
            changed_context_match,
            changed_project_source,
            newer_context_source,
        ] {
            assert_ne!(
                base,
                cache_key_with_evidence(&request, &candidates, &changed)
            );
        }
        let history = crate::memory::Evidence {
            uses: 2,
            last_used: 10_000,
            last_used_known: true,
            last_used_order: 0,
            accepts: 2,
            last_accepted: 9_000,
            specificity: 1,
            context_match: true,
            context_last_seen: 10_500,
            ..crate::memory::Evidence::default()
        };
        let first_rank = crate::memory::RankedEvidence {
            score: 250,
            confidence: 0.901,
            ..crate::memory::RankedEvidence::default()
        };
        let slightly_different_rank = crate::memory::RankedEvidence {
            confidence: 0.903,
            ..first_rank
        };
        let first = CandidateEvidence::from_ranked(&history, &first_rank, 11_000);
        let millisecond_later =
            CandidateEvidence::from_ranked(&history, &slightly_different_rank, 11_300);
        assert_eq!(first.local_confidence, 90);
        assert_eq!(first.local_confidence, millisecond_later.local_confidence);
        assert_eq!(
            cache_key_with_evidence(&request, &candidates, &[first]),
            cache_key_with_evidence(&request, &candidates, &[millisecond_later])
        );
    }

    #[test]
    fn busy_or_cooling_down_helpers_return_without_a_model_request() {
        let runtime = Runtime::new(&Config::default());
        let request = ChatRequest::new("", vec![Message::user("git st")]);
        let candidates = vec!["git status".into()];
        let running = runtime.in_flight.lock();
        assert!(runtime
            .evaluate(&request, &candidates)
            .unwrap_err()
            .to_string()
            .contains("busy"));
        drop(running);
        *runtime.failed_at.lock() = Some(Instant::now());
        assert!(runtime
            .evaluate(&request, &candidates)
            .unwrap_err()
            .to_string()
            .contains("cooling down"));
    }

    #[test]
    fn cached_decisions_survive_busy_helpers_but_expire() {
        let runtime = Runtime::new(&Config::default());
        let request = ChatRequest::new("", vec![Message::user("git st")]);
        let candidates = vec!["git status".into()];
        let key = cache_key_with_evidence(&request, &candidates, &[]);
        let value = Decision {
            allow_remote: false,
            omit_context: true,
            ranking: vec![0],
        };
        runtime.cache.lock().push_back(Cached {
            key,
            at: Instant::now(),
            value: value.clone(),
        });
        let _running = runtime.in_flight.lock();
        assert_eq!(runtime.evaluate(&request, &candidates).unwrap(), value);
        runtime.cache.lock()[0].at = Instant::now() - CACHE_LIFETIME - Duration::from_secs(1);
        assert!(runtime.cached(key).is_none());
    }
}
