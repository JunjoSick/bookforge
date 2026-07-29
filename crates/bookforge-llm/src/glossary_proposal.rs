use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bookforge_core::GlossaryCategory;
use serde::{Deserialize, Serialize};

use crate::{
    CompletionRequest, FinishReason, LlmError, LlmProvider, PromptTemplate, RequestMetadata,
    ResponseFormat, Substitutions,
};

const PROMPT_SOURCE: &str = include_str!("../prompts/glossary_propose.v2.md");
pub const GLOSSARY_PROPOSAL_PROMPT_NAME: &str = "glossary_propose";
pub const GLOSSARY_PROPOSAL_PROMPT_VERSION: &str = "v2";
const OUTPUT_TOKENS_PER_CANDIDATE: u32 = 320;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GlossaryProposalInput {
    pub id: i64,
    pub source_text: String,
    pub category: GlossaryCategory,
    pub source_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_excerpt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlossaryProposalPolicy {
    Preserve,
    Translate,
    Calque,
    Recreate,
    Decline,
    NotTerminology,
}

impl GlossaryProposalPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::Translate => "translate",
            Self::Calque => "calque",
            Self::Recreate => "recreate",
            Self::Decline => "decline",
            Self::NotTerminology => "not_terminology",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlossaryProposal {
    pub id: i64,
    pub target_text: Option<String>,
    pub policy: GlossaryProposalPolicy,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlossaryProposalRun {
    pub proposals: Vec<GlossaryProposal>,
    pub failures: Vec<GlossaryProposalFailure>,
    pub estimated_input_tokens: usize,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub request_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlossaryProposalFailure {
    pub candidate_ids: Vec<i64>,
    pub error: String,
}

#[derive(Debug, Deserialize)]
struct ProposalResponse {
    proposals: Vec<RawProposal>,
}

#[derive(Debug, Deserialize)]
struct RawProposal {
    id: i64,
    target_text: Option<String>,
    policy: GlossaryProposalPolicy,
    reason: String,
}

#[derive(Debug)]
struct ProposalBatchRequestError {
    error: LlmError,
    usage: ProposalUsage,
}

impl ProposalBatchRequestError {
    fn should_split(&self) -> bool {
        matches!(self.error, LlmError::Json(_) | LlmError::InvalidResponse(_))
    }
}

impl std::fmt::Display for ProposalBatchRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy)]
struct ProposalUsage {
    estimated_input_tokens: usize,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    request_count: usize,
}

impl ProposalUsage {
    fn for_request(estimated_input_tokens: usize) -> Self {
        Self {
            estimated_input_tokens,
            input_tokens: None,
            output_tokens: None,
            request_count: 1,
        }
    }

    fn with_response(mut self, input_tokens: Option<u64>, output_tokens: Option<u64>) -> Self {
        self.input_tokens = input_tokens;
        self.output_tokens = output_tokens;
        self
    }

    fn accumulate(&mut self, usage: Self) {
        self.estimated_input_tokens = self
            .estimated_input_tokens
            .saturating_add(usage.estimated_input_tokens);
        self.input_tokens = sum_optional_tokens(self.input_tokens, usage.input_tokens);
        self.output_tokens = sum_optional_tokens(self.output_tokens, usage.output_tokens);
        self.request_count = self.request_count.saturating_add(usage.request_count);
    }
}

pub async fn propose_glossary_renderings<P>(
    provider: &P,
    source_language: &str,
    target_language: &str,
    items: &[GlossaryProposalInput],
    provider_name: &str,
    model: &str,
    max_output_tokens: u32,
) -> Result<GlossaryProposalRun, LlmError>
where
    P: LlmProvider,
{
    if items.is_empty() {
        return Ok(GlossaryProposalRun {
            proposals: Vec::new(),
            failures: Vec::new(),
            estimated_input_tokens: 0,
            input_tokens: Some(0),
            output_tokens: Some(0),
            request_count: 0,
        });
    }

    ensure_unique_input_ids(items)?;
    let template = PromptTemplate::parse(
        GLOSSARY_PROPOSAL_PROMPT_NAME,
        GLOSSARY_PROPOSAL_PROMPT_VERSION,
        PROMPT_SOURCE,
    )
    .map_err(|error| LlmError::Provider(error.to_string()))?;

    let chunk_size = proposal_chunk_size(max_output_tokens);
    let mut queue = items
        .chunks(chunk_size)
        .map(<[GlossaryProposalInput]>::to_vec)
        .collect::<VecDeque<_>>();
    let mut proposals = Vec::with_capacity(items.len());
    let mut failures = Vec::new();
    let mut usage = ProposalUsage {
        estimated_input_tokens: 0,
        input_tokens: Some(0),
        output_tokens: Some(0),
        request_count: 0,
    };

    while let Some(chunk) = queue.pop_front() {
        match request_glossary_proposal_batch(
            provider,
            &template,
            source_language,
            target_language,
            &chunk,
            provider_name,
            model,
            max_output_tokens,
        )
        .await
        {
            Ok((mut chunk_proposals, chunk_usage)) => {
                usage.accumulate(chunk_usage);
                proposals.append(&mut chunk_proposals);
            }
            Err(error) if error.should_split() && chunk.len() > 1 => {
                usage.accumulate(error.usage);
                let mid = chunk.len() / 2;
                queue.push_front(chunk[mid..].to_vec());
                queue.push_front(chunk[..mid].to_vec());
            }
            Err(error) => {
                usage.accumulate(error.usage);
                failures.push(GlossaryProposalFailure {
                    candidate_ids: chunk.iter().map(|item| item.id).collect(),
                    error: error.to_string(),
                });
            }
        }
    }

    Ok(GlossaryProposalRun {
        proposals,
        failures,
        estimated_input_tokens: usage.estimated_input_tokens,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        request_count: usage.request_count,
    })
}

#[allow(clippy::too_many_arguments)]
async fn request_glossary_proposal_batch<P>(
    provider: &P,
    template: &PromptTemplate,
    source_language: &str,
    target_language: &str,
    items: &[GlossaryProposalInput],
    provider_name: &str,
    model: &str,
    max_output_tokens: u32,
) -> Result<(Vec<GlossaryProposal>, ProposalUsage), ProposalBatchRequestError>
where
    P: LlmProvider,
{
    let mut vars = Substitutions::new();
    vars.string("source_language", source_language)
        .string("target_language", target_language)
        .json_compact("items_json", &items);
    let rendered = template
        .render(&vars)
        .map_err(|error| ProposalBatchRequestError {
            error: LlmError::Provider(error.to_string()),
            usage: ProposalUsage::for_request(0),
        })?;
    let estimated_input_tokens =
        estimate_tokens(&rendered.system).saturating_add(estimate_tokens(&rendered.user));
    let request_usage = ProposalUsage::for_request(estimated_input_tokens);

    let response = provider
        .complete(CompletionRequest {
            system: rendered.system,
            user: rendered.user,
            response_format: ResponseFormat::Json,
            temperature: 0.1,
            max_output_tokens: Some(max_output_tokens.max(1)),
            metadata: RequestMetadata {
                prompt_template: Some(template.name.clone()),
                prompt_version: Some(template.version.clone()),
                provider: Some(provider_name.to_string()),
                model: Some(model.to_string()),
                provider_max_attempts: Some(2),
                ..RequestMetadata::default()
            },
        })
        .await
        .map_err(|error| ProposalBatchRequestError {
            error,
            usage: request_usage,
        })?;
    let request_usage = request_usage.with_response(response.input_tokens, response.output_tokens);

    if response.finish_reason == FinishReason::Length {
        return Err(ProposalBatchRequestError {
            error: LlmError::InvalidResponse(format!(
                "glossary proposal output was truncated at {max_output_tokens} tokens"
            )),
            usage: request_usage,
        });
    }

    let parsed: ProposalResponse =
        serde_json::from_str(&response.content).map_err(|error| ProposalBatchRequestError {
            error: LlmError::Json(error),
            usage: request_usage,
        })?;
    let proposals =
        validate_proposals(items, parsed.proposals).map_err(|error| ProposalBatchRequestError {
            error,
            usage: request_usage,
        })?;
    Ok((proposals, request_usage))
}

fn proposal_chunk_size(max_output_tokens: u32) -> usize {
    usize::try_from((max_output_tokens / OUTPUT_TOKENS_PER_CANDIDATE).max(1)).unwrap_or(usize::MAX)
}

fn sum_optional_tokens(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    Some(left?.saturating_add(right?))
}

fn ensure_unique_input_ids(items: &[GlossaryProposalInput]) -> Result<(), LlmError> {
    let mut ids = BTreeSet::new();
    for item in items {
        if !ids.insert(item.id) {
            return Err(LlmError::InvalidResponse(format!(
                "duplicate glossary proposal input ID {}",
                item.id
            )));
        }
    }
    Ok(())
}

fn validate_proposals(
    items: &[GlossaryProposalInput],
    raw: Vec<RawProposal>,
) -> Result<Vec<GlossaryProposal>, LlmError> {
    let expected = items.iter().map(|item| item.id).collect::<BTreeSet<_>>();
    let mut by_id = BTreeMap::new();

    for proposal in raw {
        if !expected.contains(&proposal.id) {
            return Err(LlmError::InvalidResponse(format!(
                "glossary proposal response returned unknown ID {}",
                proposal.id
            )));
        }
        let target_text = proposal
            .target_text
            .map(|target| target.trim().to_string())
            .filter(|target| !target.is_empty());
        let reason = proposal
            .reason
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if reason.is_empty() {
            return Err(LlmError::InvalidResponse(format!(
                "glossary proposal {} has no reason",
                proposal.id
            )));
        }
        match (proposal.policy, target_text.as_ref()) {
            (
                policy @ (GlossaryProposalPolicy::Decline | GlossaryProposalPolicy::NotTerminology),
                Some(_),
            ) => {
                return Err(LlmError::InvalidResponse(format!(
                    "{} glossary proposal {} included target_text",
                    policy.as_str(),
                    proposal.id,
                )));
            }
            (GlossaryProposalPolicy::Decline | GlossaryProposalPolicy::NotTerminology, None) => {}
            (_, None) => {
                return Err(LlmError::InvalidResponse(format!(
                    "glossary proposal {} omitted target_text without declining or rejecting it as not terminology",
                    proposal.id
                )));
            }
            (_, Some(_)) => {}
        }
        let normalized = GlossaryProposal {
            id: proposal.id,
            target_text,
            policy: proposal.policy,
            reason,
        };
        if by_id.insert(proposal.id, normalized).is_some() {
            return Err(LlmError::InvalidResponse(format!(
                "glossary proposal response duplicated ID {}",
                proposal.id
            )));
        }
    }

    let missing = expected
        .iter()
        .filter(|id| !by_id.contains_key(id))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(LlmError::InvalidResponse(format!(
            "glossary proposal response omitted IDs: {}",
            missing
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    Ok(items
        .iter()
        .filter_map(|item| by_id.remove(&item.id))
        .collect())
}

fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CompletionResponse, FinishReason, MockMode, MockProvider};

    fn input(id: i64, source_text: &str) -> GlossaryProposalInput {
        GlossaryProposalInput {
            id,
            source_text: source_text.to_string(),
            category: GlossaryCategory::Invented,
            source_count: 3,
            source_excerpt: Some(format!("The {source_text} hummed loudly.")),
        }
    }

    #[tokio::test]
    async fn mock_provider_returns_reviewable_proposals() {
        let provider = MockProvider::new(MockMode::PrefixTarget, "Italian");
        let run = propose_glossary_renderings(
            &provider,
            "English",
            "Italian",
            &[input(7, "phantasmatron"), input(9, "Steelypips")],
            "mock",
            "mock-prefix-target",
            1_024,
        )
        .await
        .expect("mock proposal should succeed");

        assert_eq!(run.proposals.len(), 2);
        assert_eq!(run.proposals[0].id, 7);
        assert_eq!(
            run.proposals[0].target_text.as_deref(),
            Some("[Italian] phantasmatron")
        );
        assert_eq!(run.proposals[0].policy, GlossaryProposalPolicy::Recreate);
        assert!(run.failures.is_empty());
        assert_eq!(run.request_count, 1);
        assert!(run.input_tokens.is_some());
        assert!(run.output_tokens.is_some());
    }

    #[tokio::test]
    async fn candidate_set_larger_than_one_chunk_uses_multiple_requests() {
        assert_eq!(proposal_chunk_size(8_192), 25);
        let provider = MockProvider::new(MockMode::PrefixTarget, "Italian");
        let items = (1..=26)
            .map(|id| input(id, format!("candidate-{id}").as_str()))
            .collect::<Vec<_>>();

        let run = propose_glossary_renderings(
            &provider,
            "English",
            "Italian",
            &items,
            "mock",
            "mock-prefix-target",
            8_192,
        )
        .await
        .expect("chunked mock proposal should succeed");

        assert_eq!(run.request_count, 2);
        assert_eq!(run.proposals.len(), items.len());
        assert!(run.failures.is_empty());
        assert_eq!(
            run.proposals
                .iter()
                .map(|proposal| proposal.id)
                .collect::<Vec<_>>(),
            items.iter().map(|item| item.id).collect::<Vec<_>>()
        );
    }

    #[derive(Clone)]
    struct StaticProvider {
        content: String,
    }

    impl LlmProvider for StaticProvider {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: self.content.clone(),
                input_tokens: Some(10),
                input_cached_tokens: Some(0),
                output_tokens: Some(5),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 1,
                raw: serde_json::Value::Null,
            })
        }

        fn capabilities(&self) -> crate::ProviderCapabilities {
            crate::ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    #[derive(Clone)]
    struct SplitRetryProvider;

    impl LlmProvider for SplitRetryProvider {
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let items = extract_prompt_items(&request.user);
            if items.len() > 1 {
                return Ok(CompletionResponse {
                    content: serde_json::json!({ "proposals": [] }).to_string(),
                    input_tokens: Some(10),
                    input_cached_tokens: Some(0),
                    output_tokens: Some(5),
                    finish_reason: FinishReason::Length,
                    provider_latency_ms: 1,
                    raw: serde_json::Value::Null,
                });
            }

            let proposals = items
                .iter()
                .filter_map(|item| {
                    let id = item.get("id")?.as_i64()?;
                    let source_text = item.get("source_text")?.as_str()?;
                    Some(serde_json::json!({
                        "id": id,
                        "target_text": format!("rendered {source_text}"),
                        "policy": "recreate",
                        "reason": "The isolated candidate has enough evidence."
                    }))
                })
                .collect::<Vec<_>>();
            Ok(CompletionResponse {
                content: serde_json::json!({ "proposals": proposals }).to_string(),
                input_tokens: Some(10),
                input_cached_tokens: Some(0),
                output_tokens: Some(5),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 1,
                raw: serde_json::Value::Null,
            })
        }

        fn capabilities(&self) -> crate::ProviderCapabilities {
            crate::ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    #[derive(Clone)]
    struct EmptyForCandidateProvider {
        empty_id: i64,
    }

    impl LlmProvider for EmptyForCandidateProvider {
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let items = extract_prompt_items(&request.user);
            if items.iter().any(|item| {
                item.get("id").and_then(serde_json::Value::as_i64) == Some(self.empty_id)
            }) {
                return Ok(CompletionResponse {
                    content: String::new(),
                    input_tokens: Some(10),
                    input_cached_tokens: Some(0),
                    output_tokens: Some(5),
                    finish_reason: FinishReason::Stop,
                    provider_latency_ms: 1,
                    raw: serde_json::Value::Null,
                });
            }

            let proposals = items
                .iter()
                .filter_map(|item| {
                    let id = item.get("id")?.as_i64()?;
                    let source_text = item.get("source_text")?.as_str()?;
                    Some(serde_json::json!({
                        "id": id,
                        "target_text": format!("rendered {source_text}"),
                        "policy": "recreate",
                        "reason": "The candidate has enough evidence."
                    }))
                })
                .collect::<Vec<_>>();
            Ok(CompletionResponse {
                content: serde_json::json!({ "proposals": proposals }).to_string(),
                input_tokens: Some(10),
                input_cached_tokens: Some(0),
                output_tokens: Some(5),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 1,
                raw: serde_json::Value::Null,
            })
        }

        fn capabilities(&self) -> crate::ProviderCapabilities {
            crate::ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    fn extract_prompt_items(user: &str) -> Vec<serde_json::Value> {
        user.lines()
            .find_map(|line| {
                let line = line.trim();
                line.starts_with('[')
                    .then(|| serde_json::from_str(line).ok())
                    .flatten()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn truncation_bisects_and_retries_until_every_candidate_lands() {
        let items = (1..=4)
            .map(|id| input(id, format!("candidate-{id}").as_str()))
            .collect::<Vec<_>>();

        let run = propose_glossary_renderings(
            &SplitRetryProvider,
            "English",
            "Italian",
            &items,
            "test",
            "split-retry",
            1_280,
        )
        .await
        .expect("split retries should recover");

        assert_eq!(run.proposals.len(), items.len());
        assert!(run.failures.is_empty());
        assert_eq!(
            run.request_count, 7,
            "one four-item request, two two-item retries, and four single-item retries"
        );
    }

    #[tokio::test]
    async fn empty_response_is_bisected_and_only_terminal_candidate_fails() {
        let items = (1..=4)
            .map(|id| input(id, format!("candidate-{id}").as_str()))
            .collect::<Vec<_>>();

        let run = propose_glossary_renderings(
            &EmptyForCandidateProvider { empty_id: 3 },
            "English",
            "Italian",
            &items,
            "test",
            "empty-on-three",
            1_280,
        )
        .await
        .expect("empty output should produce an explicitly partial result");

        assert_eq!(
            run.proposals
                .iter()
                .map(|proposal| proposal.id)
                .collect::<Vec<_>>(),
            vec![1, 2, 4]
        );
        assert_eq!(run.failures.len(), 1);
        assert_eq!(run.failures[0].candidate_ids, vec![3]);
        assert!(run.failures[0].error.contains("EOF"), "{run:?}");
        assert_eq!(run.request_count, 5);
    }

    #[tokio::test]
    async fn declined_proposal_has_no_fabricated_target() {
        let provider = StaticProvider {
            content: serde_json::json!({
                "proposals": [{
                    "id": 11,
                    "target_text": null,
                    "policy": "decline",
                    "reason": "The excerpt does not reveal the wordplay."
                }]
            })
            .to_string(),
        };

        let run = propose_glossary_renderings(
            &provider,
            "English",
            "Italian",
            &[input(11, "dracotron")],
            "test",
            "static",
            1_024,
        )
        .await
        .expect("decline should be valid");

        assert_eq!(run.proposals[0].target_text, None);
        assert_eq!(run.proposals[0].policy, GlossaryProposalPolicy::Decline);
    }

    #[tokio::test]
    async fn not_terminology_proposal_has_no_fabricated_target() {
        let provider = StaticProvider {
            content: serde_json::json!({
                "proposals": [{
                    "id": 12,
                    "target_text": null,
                    "policy": "not_terminology",
                    "reason": "This is an ordinary interjection, not a term needing a stable rendering."
                }]
            })
            .to_string(),
        };

        let run = propose_glossary_renderings(
            &provider,
            "English",
            "Italian",
            &[input(12, "Oh")],
            "test",
            "static",
            1_024,
        )
        .await
        .expect("not-terminology rejection should be valid");

        assert_eq!(run.proposals[0].target_text, None);
        assert_eq!(
            run.proposals[0].policy,
            GlossaryProposalPolicy::NotTerminology
        );
        assert!(run.proposals[0].reason.contains("ordinary interjection"));
    }

    #[tokio::test]
    async fn incomplete_response_is_bisected_and_terminal_omission_is_accounted_for() {
        let provider = StaticProvider {
            content: serde_json::json!({
                "proposals": [{
                    "id": 1,
                    "target_text": "dracotrone",
                    "policy": "recreate",
                    "reason": "It preserves the mechanical suffix."
                }]
            })
            .to_string(),
        };

        let run = propose_glossary_renderings(
            &provider,
            "English",
            "Italian",
            &[input(1, "dracotron"), input(2, "Jubilators")],
            "test",
            "static",
            1_024,
        )
        .await
        .expect("partial result should retain exact failure accounting");

        assert_eq!(run.proposals.len(), 1);
        assert_eq!(run.proposals[0].id, 1);
        assert_eq!(run.failures.len(), 1);
        assert_eq!(run.failures[0].candidate_ids, vec![2]);
        assert!(run.failures[0].error.contains("unknown ID 1"), "{run:?}");
        assert_eq!(run.request_count, 3);
    }
}
