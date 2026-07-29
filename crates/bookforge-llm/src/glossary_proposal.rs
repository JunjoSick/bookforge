use std::collections::{BTreeMap, BTreeSet};

use bookforge_core::GlossaryCategory;
use serde::{Deserialize, Serialize};

use crate::{
    CompletionRequest, FinishReason, LlmError, LlmProvider, PromptTemplate, RequestMetadata,
    ResponseFormat, Substitutions,
};

const PROMPT_SOURCE: &str = include_str!("../prompts/glossary_propose.v2.md");
pub const GLOSSARY_PROPOSAL_PROMPT_NAME: &str = "glossary_propose";
pub const GLOSSARY_PROPOSAL_PROMPT_VERSION: &str = "v2";

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
    pub estimated_input_tokens: usize,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
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
            estimated_input_tokens: 0,
            input_tokens: Some(0),
            output_tokens: Some(0),
        });
    }

    ensure_unique_input_ids(items)?;
    let template = PromptTemplate::parse(
        GLOSSARY_PROPOSAL_PROMPT_NAME,
        GLOSSARY_PROPOSAL_PROMPT_VERSION,
        PROMPT_SOURCE,
    )
    .map_err(|error| LlmError::Provider(error.to_string()))?;
    let mut vars = Substitutions::new();
    vars.string("source_language", source_language)
        .string("target_language", target_language)
        .json_compact("items_json", &items);
    let rendered = template
        .render(&vars)
        .map_err(|error| LlmError::Provider(error.to_string()))?;
    let estimated_input_tokens =
        estimate_tokens(&rendered.system).saturating_add(estimate_tokens(&rendered.user));

    let response = provider
        .complete(CompletionRequest {
            system: rendered.system,
            user: rendered.user,
            response_format: ResponseFormat::Json,
            temperature: 0.1,
            max_output_tokens: Some(max_output_tokens.max(1)),
            metadata: RequestMetadata {
                prompt_template: Some(template.name),
                prompt_version: Some(template.version),
                provider: Some(provider_name.to_string()),
                model: Some(model.to_string()),
                provider_max_attempts: Some(2),
                ..RequestMetadata::default()
            },
        })
        .await?;

    if response.finish_reason == FinishReason::Length {
        return Err(LlmError::InvalidResponse(format!(
            "glossary proposal output was truncated at {max_output_tokens} tokens"
        )));
    }

    let parsed: ProposalResponse = serde_json::from_str(&response.content)?;
    let proposals = validate_proposals(items, parsed.proposals)?;
    Ok(GlossaryProposalRun {
        proposals,
        estimated_input_tokens,
        input_tokens: response.input_tokens,
        output_tokens: response.output_tokens,
    })
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
        assert!(run.input_tokens.is_some());
        assert!(run.output_tokens.is_some());
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
    async fn incomplete_response_is_rejected_as_a_whole() {
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

        let error = propose_glossary_renderings(
            &provider,
            "English",
            "Italian",
            &[input(1, "dracotron"), input(2, "Jubilators")],
            "test",
            "static",
            1_024,
        )
        .await
        .expect_err("omitted IDs must fail the batch");

        assert!(error.to_string().contains("omitted IDs: 2"), "{error}");
    }
}
