use std::{
    fs,
    io::{self, BufRead, Write},
    path::PathBuf,
};

use anyhow::{Context, Result};
use bookforge_core::{
    GlossaryCategory, GlossaryScopeKind, GlossaryStatus, GlossaryTerm, JsonMode, RetryAfterPolicy,
    extract_glossary_candidates, glossary::glossary_candidate_excerpt, ir::Block,
};
use bookforge_llm::{
    GlossaryProposalInput, GlossaryProposalRun, LlmProvider, MockProvider, OpenAiCompatibleConfig,
    OpenAiCompatibleProvider, propose_glossary_renderings,
};
use bookforge_store::{GlossaryFilter, JobStore, NewGlossaryCandidate, StoredGlossaryCandidate};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Debug, Args)]
pub struct GlossaryArgs {
    #[command(subcommand)]
    command: GlossaryCommand,
}

#[derive(Debug, Subcommand)]
enum GlossaryCommand {
    /// List stored terms, optionally filtered by scope or language pair.
    List(ListArgs),
    /// Add one source-to-target term.
    Add(AddArgs),
    /// Remove a term by its numeric ID.
    Remove(RemoveArgs),
    /// Remove all terms in a selected scope.
    Clear(ClearArgs),
    /// Import terms from a BookForge glossary TOML file.
    Import(ImportArgs),
    /// Export matching terms to a BookForge glossary TOML file.
    Export(ExportArgs),
    /// Find repeated names and terms in an EPUB for later review.
    ExtractCandidates(ExtractCandidatesArgs),
    /// Ask a review model for target renderings of pending candidates.
    Propose(ProposeArgs),
    /// Interactively accept, translate, or reject extracted candidates.
    ReviewCandidates(ReviewCandidatesArgs),
}

#[derive(Debug, Args)]
struct ListArgs {
    #[arg(long)]
    book: Option<String>,

    #[arg(long)]
    series: Option<String>,

    #[arg(long)]
    language: Option<String>,
}

#[derive(Debug, Args)]
struct AddArgs {
    source: String,
    target: String,

    #[arg(long, value_enum)]
    category: GlossaryCategory,

    #[arg(long, value_enum, default_value_t = GlossaryScopeKind::Global)]
    scope: GlossaryScopeKind,

    #[arg(long)]
    scope_id: Option<String>,

    #[arg(long)]
    source_lang: Option<String>,

    #[arg(long)]
    target_lang: Option<String>,

    #[arg(long)]
    case_sensitive: bool,

    #[arg(long)]
    always_active: bool,

    #[arg(long)]
    notes: Option<String>,
}

#[derive(Debug, Args)]
struct RemoveArgs {
    id: i64,
}

#[derive(Debug, Args)]
struct ClearArgs {
    #[arg(long, value_enum)]
    scope: GlossaryScopeKind,

    #[arg(long)]
    scope_id: Option<String>,
}

#[derive(Debug, Args)]
struct ImportArgs {
    file: PathBuf,
}

#[derive(Debug, Args)]
struct ExportArgs {
    file: PathBuf,

    #[arg(long, value_enum)]
    scope: Option<GlossaryScopeKind>,

    #[arg(long)]
    scope_id: Option<String>,

    #[arg(long)]
    language: Option<String>,
}

#[derive(Debug, Args)]
struct ExtractCandidatesArgs {
    input: PathBuf,

    #[arg(long)]
    book_id: String,

    #[arg(long)]
    source_lang: String,

    #[arg(long)]
    target_lang: String,

    #[arg(long, default_value_t = 4)]
    min_count: usize,

    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Debug, Args)]
struct ReviewCandidatesArgs {
    book_id: String,

    #[arg(long)]
    language: Option<String>,
}

#[derive(Debug, Args)]
struct ProposeArgs {
    input: PathBuf,

    #[arg(long)]
    book_id: String,

    #[arg(long)]
    language: Option<String>,

    #[arg(long, default_value = "deepseek")]
    qa_provider: String,

    /// Strong model used for terminology proposals; intentionally has no cheap default.
    #[arg(long)]
    qa_model: String,

    #[arg(long)]
    qa_base_url: Option<String>,

    #[arg(long)]
    qa_api_key_env: Option<String>,

    /// Output-token budget for the proposal request. Defaults to a budget
    /// scaled to the number of pending candidates.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    qa_max_output_tokens: Option<u32>,

    /// Maximum characters of source context supplied for each candidate.
    #[arg(long, default_value_t = 320)]
    context_chars: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct GlossaryToml {
    meta: GlossaryTomlMeta,
    #[serde(default, rename = "term")]
    terms: Vec<GlossaryTomlTerm>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GlossaryTomlMeta {
    schema_version: u32,
    source_language: String,
    target_language: String,
    scope: GlossaryTomlScope,
}

#[derive(Debug, Deserialize, Serialize)]
struct GlossaryTomlScope {
    kind: GlossaryScopeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GlossaryTomlTerm {
    source: String,
    target: String,
    category: GlossaryCategory,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    always_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    #[serde(default = "default_user_seeded")]
    status: GlossaryStatus,
    #[serde(default)]
    source_count: usize,
}

pub async fn run(args: GlossaryArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    match args.command {
        GlossaryCommand::List(args) => list_terms(&store, args),
        GlossaryCommand::Add(args) => add_term(&store, args),
        GlossaryCommand::Remove(args) => remove_term(&store, args),
        GlossaryCommand::Clear(args) => clear_terms(&store, args),
        GlossaryCommand::Import(args) => import_terms(&store, args),
        GlossaryCommand::Export(args) => export_terms(&store, args),
        GlossaryCommand::ExtractCandidates(args) => extract_candidates(&store, args),
        GlossaryCommand::Propose(args) => propose_candidates(&store, args).await,
        GlossaryCommand::ReviewCandidates(args) => review_candidates(&store, args),
    }
}

pub(crate) fn read_glossary_file(path: &PathBuf) -> Result<Vec<GlossaryTerm>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read glossary file {}", path.display()))?;
    let parsed: GlossaryToml = toml::from_str(&raw)
        .with_context(|| format!("failed to parse glossary TOML {}", path.display()))?;
    glossary_toml_to_terms(parsed)
}

fn import_terms(store: &JobStore, args: ImportArgs) -> Result<()> {
    let terms = read_glossary_file(&args.file)?;
    let imported = store.upsert_glossary_terms(&terms)?;
    println!("Imported {imported} glossary terms.");
    Ok(())
}

fn export_terms(store: &JobStore, args: ExportArgs) -> Result<()> {
    let (source_language, target_language) = match args.language.as_deref() {
        Some(language) => {
            let (source, target) = parse_language_pair(language)?;
            (Some(source), Some(target))
        }
        None => (None, None),
    };
    let terms = store.list_glossary_terms(GlossaryFilter {
        scope_kind: args.scope,
        scope_id: args.scope_id.as_deref(),
        source_language: source_language.as_deref(),
        target_language: target_language.as_deref(),
        active_only: false,
    })?;
    if terms.is_empty() {
        anyhow::bail!("no glossary terms matched the export filters");
    }

    let output = terms_to_glossary_toml(&terms)?;
    fs::write(&args.file, toml::to_string_pretty(&output)?)?;
    println!("Exported {} glossary terms.", output.terms.len());
    Ok(())
}

fn extract_candidates(store: &JobStore, args: ExtractCandidatesArgs) -> Result<()> {
    let book = bookforge_epub::read_epub(&args.input)
        .with_context(|| format!("failed to read EPUB {}", args.input.display()))?;
    let extracted =
        extract_glossary_candidates(&book.blocks, &args.source_lang, args.min_count, args.limit);
    let candidates = extracted
        .iter()
        .map(|candidate| NewGlossaryCandidate {
            source_text: candidate.source_text.as_str(),
            category: candidate.category,
            source_count: candidate.source_count,
        })
        .collect::<Vec<_>>();
    let result = store.upsert_glossary_candidates(
        &args.book_id,
        &args.source_lang,
        &args.target_lang,
        &candidates,
    )?;
    println!(
        "Extracted {} candidates: {} inserted, {} updated, {} skipped.",
        extracted.len(),
        result.inserted,
        result.updated,
        result.skipped
    );
    Ok(())
}

async fn propose_candidates(store: &JobStore, args: ProposeArgs) -> Result<()> {
    let Some((source_language, target_language)) =
        resolve_candidate_language_pair(store, &args.book_id, args.language.as_deref())?
    else {
        println!("No pending glossary candidates.");
        return Ok(());
    };
    let pending = store
        .list_glossary_candidates(&args.book_id, &source_language, &target_language)?
        .into_iter()
        .filter(|candidate| {
            candidate
                .target_text
                .as_deref()
                .is_none_or(|target| target.trim().is_empty())
        })
        .count();
    if pending == 0 {
        println!("No pending glossary candidates without proposals.");
        return Ok(());
    }

    let book = bookforge_epub::read_epub(&args.input)
        .with_context(|| format!("failed to read EPUB {}", args.input.display()))?;
    println!(
        "Requesting {pending} glossary proposals for {} {}->{} from {}/{}.",
        args.book_id, source_language, target_language, args.qa_provider, args.qa_model
    );

    let run = match args.qa_provider.as_str() {
        "mock" => {
            let provider = MockProvider::new(
                crate::commands::translate::mock_mode(&args.qa_model),
                &target_language,
            );
            propose_candidates_with_provider(
                store,
                &book.blocks,
                &args.book_id,
                &source_language,
                &target_language,
                &args.qa_provider,
                &args.qa_model,
                args.context_chars,
                args.qa_max_output_tokens,
                &provider,
            )
            .await?
        }
        "deepseek" | "openrouter" | "openai-compatible" => {
            let config = glossary_proposal_provider_config(&args)?;
            let provider = OpenAiCompatibleProvider::new(config)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            propose_candidates_with_provider(
                store,
                &book.blocks,
                &args.book_id,
                &source_language,
                &target_language,
                &args.qa_provider,
                &args.qa_model,
                args.context_chars,
                args.qa_max_output_tokens,
                &provider,
            )
            .await?
        }
        provider => anyhow::bail!("unsupported glossary proposal provider '{provider}'"),
    };

    let rendered = run
        .proposals
        .iter()
        .filter(|proposal| proposal.target_text.is_some())
        .count();
    let declined = run.proposals.len().saturating_sub(rendered);
    println!(
        "Saved {rendered} proposed renderings and {declined} declines; all remain auto_candidate."
    );
    if declined > 0 {
        let candidates =
            store.list_glossary_candidates(&args.book_id, &source_language, &target_language)?;
        for proposal in run
            .proposals
            .iter()
            .filter(|proposal| proposal.target_text.is_none())
        {
            let source = candidates
                .iter()
                .find(|candidate| candidate.id == proposal.id)
                .map(|candidate| candidate.source_text.as_str())
                .unwrap_or("<unknown candidate>");
            println!("Declined {source}: {}", proposal.reason);
        }
    }
    println!(
        "Tokens: estimated input {}, provider input {}, provider output {}.",
        run.estimated_input_tokens,
        format_optional_tokens(run.input_tokens),
        format_optional_tokens(run.output_tokens)
    );
    println!(
        "Review explicitly with: bookforge glossary review-candidates {} --language \"{}->{}\"",
        args.book_id, source_language, target_language
    );
    Ok(())
}

/// Output-token budget for a proposal request covering `candidates` terms.
///
/// One request carries every pending candidate, so a flat default is wrong at
/// both ends. A measured run â€” Kimi K3, 40 candidates â€” used 8,277 output
/// tokens, about 207 per candidate including reasoning. 320 per candidate
/// leaves headroom for a more verbose model; the 8,192 floor keeps small books
/// from being starved by a tight budget, and the 65,536 ceiling stops a
/// pathological candidate list from requesting an unbounded completion.
fn proposal_output_budget(candidates: usize) -> u32 {
    let scaled = candidates.saturating_mul(320).min(u32::MAX as usize) as u32;
    scaled.clamp(8_192, 65_536)
}

#[allow(clippy::too_many_arguments)]
async fn propose_candidates_with_provider<P>(
    store: &JobStore,
    blocks: &[Block],
    book_id: &str,
    source_language: &str,
    target_language: &str,
    provider_name: &str,
    model: &str,
    context_chars: usize,
    max_output_tokens: Option<u32>,
    provider: &P,
) -> Result<GlossaryProposalRun>
where
    P: LlmProvider,
{
    let candidates = store.list_glossary_candidates(book_id, source_language, target_language)?;
    let items = candidates
        .iter()
        .filter(|candidate| {
            candidate
                .target_text
                .as_deref()
                .is_none_or(|target| target.trim().is_empty())
        })
        .map(|candidate| GlossaryProposalInput {
            id: candidate.id,
            source_text: candidate.source_text.clone(),
            category: candidate.category,
            source_count: candidate.source_count,
            source_excerpt: glossary_candidate_excerpt(
                blocks,
                &candidate.source_text,
                context_chars.max(1),
            ),
        })
        .collect::<Vec<_>>();
    let run = propose_glossary_renderings(
        provider,
        source_language,
        target_language,
        &items,
        provider_name,
        model,
        max_output_tokens.unwrap_or_else(|| proposal_output_budget(items.len())),
    )
    .await
    .map_err(|error| anyhow::anyhow!("glossary proposal request failed: {error}"))?;
    let proposals_by_id = run
        .proposals
        .iter()
        .map(|proposal| (proposal.id, proposal))
        .collect::<std::collections::HashMap<_, _>>();
    let still_pending =
        store.list_glossary_candidates(book_id, source_language, target_language)?;
    let updates = still_pending
        .iter()
        .filter_map(|candidate| {
            let proposal = proposals_by_id.get(&candidate.id)?;
            let target_text = proposal.target_text.as_ref()?;
            if candidate
                .target_text
                .as_deref()
                .is_some_and(|target| !target.trim().is_empty())
            {
                return None;
            }
            let note = format!(
                "model proposal ({}): {}",
                proposal.policy.as_str(),
                proposal.reason
            );
            Some(GlossaryTerm {
                id: Some(candidate.id),
                scope_kind: GlossaryScopeKind::Book,
                scope_id: Some(book_id.to_string()),
                source_text: candidate.source_text.clone(),
                target_text: target_text.clone(),
                category: candidate.category,
                notes: Some(note),
                case_sensitive: candidate.case_sensitive,
                always_active: candidate.always_active,
                status: GlossaryStatus::AutoCandidate,
                source_language: source_language.to_string(),
                target_language: target_language.to_string(),
                source_count: candidate.source_count,
            })
        })
        .collect::<Vec<_>>();
    let updated = store.upsert_glossary_terms(&updates)?;
    if updated != updates.len() {
        anyhow::bail!(
            "only {updated} of {} proposal rows were still pending; no settled term was overwritten",
            updates.len()
        );
    }
    Ok(run)
}

fn glossary_proposal_provider_config(args: &ProposeArgs) -> Result<OpenAiCompatibleConfig> {
    let (default_url, default_key_env) = match args.qa_provider.as_str() {
        "deepseek" => ("https://api.deepseek.com/v1", "DEEPSEEK_API_KEY"),
        "openrouter" => ("https://openrouter.ai/api/v1", "OPENROUTER_API_KEY"),
        "openai-compatible" => (
            args.qa_base_url.as_deref().ok_or_else(|| {
                anyhow::anyhow!("--qa-base-url is required for --qa-provider openai-compatible")
            })?,
            "OPENAI_API_KEY",
        ),
        provider => anyhow::bail!("unsupported glossary proposal provider '{provider}'"),
    };

    Ok(OpenAiCompatibleConfig {
        base_url: args
            .qa_base_url
            .clone()
            .unwrap_or_else(|| default_url.to_string()),
        api_key_env: args
            .qa_api_key_env
            .clone()
            .unwrap_or_else(|| default_key_env.to_string()),
        model: args.qa_model.clone(),
        timeout_seconds: 180,
        provider_max_attempts: 2,
        thinking_disabled: false,
        retry_after_policy: RetryAfterPolicy::JitteredExponential,
        max_backoff_seconds: 30,
        max_idle_per_host: 4,
        json_mode: JsonMode::Auto,
    })
}

fn format_optional_tokens(tokens: Option<u64>) -> String {
    tokens
        .map(|tokens| tokens.to_string())
        .unwrap_or_else(|| "unreported".to_string())
}

fn review_candidates(store: &JobStore, args: ReviewCandidatesArgs) -> Result<()> {
    let Some((source_language, target_language)) =
        resolve_candidate_language_pair(store, &args.book_id, args.language.as_deref())?
    else {
        println!("No pending glossary candidates.");
        return Ok(());
    };

    let mut candidates =
        store.list_glossary_candidates(&args.book_id, &source_language, &target_language)?;
    if candidates.is_empty() {
        println!("No pending glossary candidates.");
        return Ok(());
    }

    println!(
        "Reviewing {} candidates for {} {}->{}.",
        candidates.len(),
        args.book_id,
        source_language,
        target_language
    );
    print_candidate_help();
    print_candidates(&candidates);

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut lines = stdin.lock().lines();
    loop {
        print!("glossary> ");
        stdout.flush()?;
        let Some(line) = lines.next() else {
            break;
        };
        let line = line?;
        match parse_review_command(&line) {
            Ok(ReviewCommand::Accept(number)) => {
                let candidate = match candidate_by_number(&candidates, number) {
                    Ok(candidate) => candidate,
                    Err(err) => {
                        eprintln!("{err}");
                        continue;
                    }
                };
                if store.accept_glossary_candidate(candidate.id, None)? {
                    println!("Accepted {}.", candidate.source_text);
                }
            }
            Ok(ReviewCommand::AcceptAll) => {
                let proposed = candidates
                    .iter()
                    .filter(|candidate| {
                        candidate
                            .target_text
                            .as_deref()
                            .is_some_and(|target| !target.trim().is_empty())
                    })
                    .collect::<Vec<_>>();
                let mut accepted = 0usize;
                for candidate in proposed {
                    if store.accept_glossary_candidate(candidate.id, None)? {
                        accepted += 1;
                    }
                }
                println!(
                    "Accepted {accepted} proposed renderings; candidates without a rendering remain pending."
                );
            }
            Ok(ReviewCommand::Set(number, target)) => {
                let candidate = match candidate_by_number(&candidates, number) {
                    Ok(candidate) => candidate,
                    Err(err) => {
                        eprintln!("{err}");
                        continue;
                    }
                };
                if store.accept_glossary_candidate(candidate.id, Some(&target))? {
                    println!("Accepted {} -> {}.", candidate.source_text, target);
                }
            }
            Ok(ReviewCommand::Reject(number)) => {
                let candidate = match candidate_by_number(&candidates, number) {
                    Ok(candidate) => candidate,
                    Err(err) => {
                        eprintln!("{err}");
                        continue;
                    }
                };
                if store.reject_glossary_candidate(candidate.id)? {
                    println!("Rejected {}.", candidate.source_text);
                }
            }
            Ok(ReviewCommand::List) => {}
            Ok(ReviewCommand::Help) => {
                print_candidate_help();
                continue;
            }
            Ok(ReviewCommand::Quit) => break,
            Ok(ReviewCommand::Empty) => continue,
            Err(err) => {
                eprintln!("{err}");
                continue;
            }
        }

        candidates =
            store.list_glossary_candidates(&args.book_id, &source_language, &target_language)?;
        if candidates.is_empty() {
            println!("No pending glossary candidates.");
        } else {
            print_candidates(&candidates);
        }
    }
    Ok(())
}

fn resolve_candidate_language_pair(
    store: &JobStore,
    book_id: &str,
    language: Option<&str>,
) -> Result<Option<(String, String)>> {
    if let Some(language) = language {
        let (source, target) = parse_language_pair(language)?;
        return Ok(Some((source, target)));
    }

    let pairs = store.list_glossary_candidate_language_pairs(book_id)?;
    match pairs.as_slice() {
        [] => Ok(None),
        [(source, target)] => Ok(Some((source.clone(), target.clone()))),
        _ => {
            let available = pairs
                .iter()
                .map(|(source, target)| format!("{source}->{target}"))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "multiple candidate language pairs exist for book '{book_id}'; pass --language with one of: {available}"
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReviewCommand {
    Accept(usize),
    AcceptAll,
    Set(usize, String),
    Reject(usize),
    List,
    Help,
    Quit,
    Empty,
}

fn parse_review_command(line: &str) -> Result<ReviewCommand> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(ReviewCommand::Empty);
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default();
    match command {
        "accept" => Ok(ReviewCommand::Accept(parse_candidate_number(rest)?)),
        "accept-all" => {
            if !rest.trim().is_empty() {
                anyhow::bail!("usage: accept-all");
            }
            Ok(ReviewCommand::AcceptAll)
        }
        "reject" => Ok(ReviewCommand::Reject(parse_candidate_number(rest)?)),
        "set" => {
            let rest = rest.trim();
            let mut parts = rest.splitn(2, char::is_whitespace);
            let Some(number) = parts.next() else {
                anyhow::bail!("usage: set N \"translation\"");
            };
            let Some(target) = parts.next() else {
                anyhow::bail!("usage: set N \"translation\"");
            };
            let target = unquote(target.trim());
            if target.is_empty() {
                anyhow::bail!("usage: set N \"translation\"");
            }
            Ok(ReviewCommand::Set(
                parse_candidate_number(number)?,
                target.to_string(),
            ))
        }
        "list" => Ok(ReviewCommand::List),
        "help" => Ok(ReviewCommand::Help),
        "quit" | "exit" => Ok(ReviewCommand::Quit),
        other => anyhow::bail!(
            "unknown command '{other}'; expected accept, accept-all, set, reject, list, help, or quit"
        ),
    }
}

fn parse_candidate_number(value: &str) -> Result<usize> {
    let number = value.trim().parse::<usize>()?;
    if number == 0 {
        anyhow::bail!("candidate number must be 1 or greater");
    }
    Ok(number)
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn candidate_by_number(
    candidates: &[StoredGlossaryCandidate],
    number: usize,
) -> Result<&StoredGlossaryCandidate> {
    candidates
        .get(number - 1)
        .ok_or_else(|| anyhow::anyhow!("candidate {number} is not in the current list"))
}

fn print_candidate_help() {
    println!("Commands: accept N, accept-all, set N \"translation\", reject N, list, help, quit");
}

fn print_candidates(candidates: &[StoredGlossaryCandidate]) {
    for (index, candidate) in candidates.iter().enumerate() {
        println!(
            "{}\t{}\t{}\t{}\t{} -> {}",
            index + 1,
            candidate.source_count,
            candidate.category,
            candidate.status.as_str(),
            candidate.source_text,
            candidate
                .target_text
                .as_deref()
                .filter(|target| !target.trim().is_empty())
                .unwrap_or("-")
        );
        if let Some(note) = candidate.notes.as_deref() {
            println!("\t{note}");
        }
    }
}

fn list_terms(store: &JobStore, args: ListArgs) -> Result<()> {
    let (source_language, target_language) = match args.language.as_deref() {
        Some(language) => {
            let (source, target) = parse_language_pair(language)?;
            (Some(source), Some(target))
        }
        None => (None, None),
    };
    let (scope_kind, scope_id) = if let Some(book) = args.book.as_deref() {
        (Some(GlossaryScopeKind::Book), Some(book))
    } else if let Some(series) = args.series.as_deref() {
        (Some(GlossaryScopeKind::Series), Some(series))
    } else {
        (None, None)
    };
    let terms = store.list_glossary_terms(GlossaryFilter {
        scope_kind,
        scope_id,
        source_language: source_language.as_deref(),
        target_language: target_language.as_deref(),
        active_only: false,
    })?;
    if terms.is_empty() {
        println!("No glossary terms.");
        return Ok(());
    }
    for term in terms {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{} -> {}",
            term.id.unwrap_or_default(),
            term.source_language,
            term.target_language,
            term.scope_kind,
            term.scope_id.as_deref().unwrap_or("-"),
            term.source_text,
            term.target_text
        );
    }
    Ok(())
}

fn add_term(store: &JobStore, args: AddArgs) -> Result<()> {
    let source_language = args
        .source_lang
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--source-lang is required for glossary add"))?;
    let target_language = args
        .target_lang
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--target-lang is required for glossary add"))?;
    validate_scope(args.scope, args.scope_id.as_deref())?;
    let term = GlossaryTerm {
        id: None,
        scope_kind: args.scope,
        scope_id: normalized_scope_id(args.scope, args.scope_id),
        source_text: args.source,
        target_text: args.target,
        category: args.category,
        notes: args.notes,
        case_sensitive: args.case_sensitive,
        always_active: args.always_active,
        status: GlossaryStatus::UserSeeded,
        source_language: source_language.to_string(),
        target_language: target_language.to_string(),
        source_count: 0,
    };
    let id = store.add_glossary_term(&term)?;
    println!("Glossary term {id} saved.");
    Ok(())
}

fn remove_term(store: &JobStore, args: RemoveArgs) -> Result<()> {
    let removed = store.remove_glossary_term(args.id)?;
    println!("Removed {removed} glossary terms.");
    Ok(())
}

fn clear_terms(store: &JobStore, args: ClearArgs) -> Result<()> {
    validate_scope(args.scope, args.scope_id.as_deref())?;
    let removed = store.clear_glossary_scope(args.scope, args.scope_id.as_deref())?;
    println!("Removed {removed} glossary terms.");
    Ok(())
}

fn glossary_toml_to_terms(parsed: GlossaryToml) -> Result<Vec<GlossaryTerm>> {
    if parsed.meta.schema_version != 1 {
        anyhow::bail!(
            "unsupported glossary schema_version {}; expected 1",
            parsed.meta.schema_version
        );
    }
    validate_scope(parsed.meta.scope.kind, parsed.meta.scope.id.as_deref())?;
    let scope_id = normalized_scope_id(parsed.meta.scope.kind, parsed.meta.scope.id);
    let terms = parsed
        .terms
        .into_iter()
        .map(|term| GlossaryTerm {
            id: None,
            scope_kind: parsed.meta.scope.kind,
            scope_id: scope_id.clone(),
            source_text: term.source,
            target_text: term.target,
            category: term.category,
            notes: term.notes,
            case_sensitive: term.case_sensitive,
            always_active: term.always_active,
            status: term.status,
            source_language: parsed.meta.source_language.clone(),
            target_language: parsed.meta.target_language.clone(),
            source_count: term.source_count,
        })
        .collect::<Vec<_>>();
    Ok(terms)
}

fn terms_to_glossary_toml(terms: &[GlossaryTerm]) -> Result<GlossaryToml> {
    let Some(first) = terms.first() else {
        anyhow::bail!("cannot export an empty glossary");
    };
    let same_tuple = terms.iter().all(|term| {
        term.scope_kind == first.scope_kind
            && term.scope_id == first.scope_id
            && term.source_language == first.source_language
            && term.target_language == first.target_language
    });
    if !same_tuple {
        anyhow::bail!(
            "export matched multiple scope/language tuples; narrow with --scope, --scope-id, and --language"
        );
    }
    Ok(GlossaryToml {
        meta: GlossaryTomlMeta {
            schema_version: 1,
            source_language: first.source_language.clone(),
            target_language: first.target_language.clone(),
            scope: GlossaryTomlScope {
                kind: first.scope_kind,
                id: first.scope_id.clone(),
            },
        },
        terms: terms
            .iter()
            .map(|term| GlossaryTomlTerm {
                source: term.source_text.clone(),
                target: term.target_text.clone(),
                category: term.category,
                case_sensitive: term.case_sensitive,
                always_active: term.always_active,
                notes: term.notes.clone(),
                status: term.status,
                source_count: term.source_count,
            })
            .collect(),
    })
}

fn validate_scope(scope: GlossaryScopeKind, scope_id: Option<&str>) -> Result<()> {
    match scope {
        GlossaryScopeKind::Global => Ok(()),
        GlossaryScopeKind::Series | GlossaryScopeKind::Book => {
            if scope_id.is_some_and(|id| !id.trim().is_empty()) {
                Ok(())
            } else {
                anyhow::bail!("--scope-id is required for {scope} glossary terms")
            }
        }
    }
}

fn normalized_scope_id(scope: GlossaryScopeKind, scope_id: Option<String>) -> Option<String> {
    if scope == GlossaryScopeKind::Global {
        None
    } else {
        scope_id
    }
}

fn parse_language_pair(value: &str) -> Result<(String, String)> {
    for delimiter in ["->", ":", "/"] {
        if let Some((source, target)) = value.split_once(delimiter) {
            let source = source.trim();
            let target = target.trim();
            if !source.is_empty() && !target.is_empty() {
                return Ok((source.to_string(), target.to_string()));
            }
        }
    }
    anyhow::bail!("language must be formatted as SOURCE->TARGET, SOURCE:TARGET, or SOURCE/TARGET")
}

fn default_user_seeded() -> GlossaryStatus {
    GlossaryStatus::UserSeeded
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_llm::{
        CompletionRequest, CompletionResponse, FinishReason, LlmError, ProviderCapabilities,
    };

    #[test]
    fn proposal_budget_scales_with_candidates_between_a_floor_and_a_ceiling() {
        assert_eq!(proposal_output_budget(0), 8_192);
        assert_eq!(proposal_output_budget(20), 8_192);
        // The measured 40-candidate run needed 8,277 output tokens, which the
        // old flat 4,096 default could not cover.
        assert!(proposal_output_budget(40) > 8_277);
        assert_eq!(proposal_output_budget(100), 32_000);
        assert_eq!(proposal_output_budget(1_000_000), 65_536);
    }

    #[derive(Clone)]
    struct FailingProvider;

    impl LlmProvider for FailingProvider {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> std::result::Result<CompletionResponse, LlmError> {
            Err(LlmError::Provider("offline test failure".to_string()))
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    #[derive(Clone)]
    struct DecliningProvider {
        id: i64,
    }

    impl LlmProvider for DecliningProvider {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> std::result::Result<CompletionResponse, LlmError> {
            let content = serde_json::json!({
                "proposals": [{
                    "id": self.id,
                    "target_text": null,
                    "policy": "decline",
                    "reason": "The excerpt does not expose the wordplay."
                }]
            })
            .to_string();
            Ok(CompletionResponse {
                content,
                input_tokens: Some(10),
                input_cached_tokens: Some(0),
                output_tokens: Some(5),
                finish_reason: FinishReason::Stop,
                provider_latency_ms: 1,
                raw: serde_json::Value::Null,
            })
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_json_response_format: true,
                supports_usage_tokens: true,
            }
        }
    }

    fn stored_term(source_text: &str, target_text: &str, status: GlossaryStatus) -> GlossaryTerm {
        GlossaryTerm {
            id: None,
            scope_kind: GlossaryScopeKind::Book,
            scope_id: Some("book".to_string()),
            source_text: source_text.to_string(),
            target_text: target_text.to_string(),
            category: GlossaryCategory::Invented,
            notes: None,
            case_sensitive: true,
            always_active: false,
            status,
            source_language: "English".to_string(),
            target_language: "Italian".to_string(),
            source_count: 3,
        }
    }

    #[test]
    fn parses_glossary_toml() {
        let parsed: GlossaryToml = toml::from_str(
            r#"
[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "book"
id = "fellowship"

[[term]]
source = "Aragorn"
target = "Aragorn"
category = "person"
case_sensitive = true
"#,
        )
        .expect("TOML should parse");

        let terms = glossary_toml_to_terms(parsed).expect("terms should convert");
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].scope_kind, GlossaryScopeKind::Book);
        assert_eq!(terms[0].scope_id.as_deref(), Some("fellowship"));
        assert!(terms[0].case_sensitive);
    }

    #[test]
    fn parses_language_pair() {
        assert_eq!(
            parse_language_pair("English->Italian").expect("pair"),
            ("English".to_string(), "Italian".to_string())
        );
    }

    #[test]
    fn parses_candidate_review_commands() {
        assert_eq!(
            parse_review_command("accept 2").expect("accept command"),
            ReviewCommand::Accept(2)
        );
        assert_eq!(
            parse_review_command("accept-all").expect("accept-all command"),
            ReviewCommand::AcceptAll
        );
        assert_eq!(
            parse_review_command("set 3 \"Monte Fato\"").expect("set command"),
            ReviewCommand::Set(3, "Monte Fato".to_string())
        );
        assert_eq!(
            parse_review_command("reject 4").expect("reject command"),
            ReviewCommand::Reject(4)
        );
        assert_eq!(
            parse_review_command("list").expect("list command"),
            ReviewCommand::List
        );
        assert_eq!(
            parse_review_command("help").expect("help command"),
            ReviewCommand::Help
        );
        assert_eq!(
            parse_review_command("quit").expect("quit command"),
            ReviewCommand::Quit
        );
    }

    #[test]
    fn exported_toml_reimports_same_term_fields() {
        let terms = vec![GlossaryTerm {
            id: Some(7),
            scope_kind: GlossaryScopeKind::Series,
            scope_id: Some("lotr".to_string()),
            source_text: "the One Ring".to_string(),
            target_text: "l'Unico Anello".to_string(),
            category: GlossaryCategory::Object,
            notes: Some("canonical series term".to_string()),
            case_sensitive: false,
            always_active: false,
            status: GlossaryStatus::UserSeeded,
            source_language: "English".to_string(),
            target_language: "Italian".to_string(),
            source_count: 12,
        }];

        let exported = terms_to_glossary_toml(&terms).expect("terms should export");
        let encoded = toml::to_string_pretty(&exported).expect("TOML should encode");
        let reparsed: GlossaryToml = toml::from_str(&encoded).expect("TOML should parse");
        let imported = glossary_toml_to_terms(reparsed).expect("terms should import");

        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].scope_kind, terms[0].scope_kind);
        assert_eq!(imported[0].scope_id, terms[0].scope_id);
        assert_eq!(imported[0].source_text, terms[0].source_text);
        assert_eq!(imported[0].target_text, terms[0].target_text);
        assert_eq!(imported[0].category, terms[0].category);
        assert_eq!(imported[0].notes, terms[0].notes);
        assert_eq!(imported[0].source_count, terms[0].source_count);
    }

    #[tokio::test]
    async fn proposal_pass_only_submits_unrendered_auto_candidates() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = JobStore::open(directory.path().join("jobs.sqlite")).expect("store");
        store
            .upsert_glossary_terms(&[
                stored_term("pending", "", GlossaryStatus::AutoCandidate),
                stored_term(
                    "already proposed",
                    "esistente",
                    GlossaryStatus::AutoCandidate,
                ),
                stored_term("seeded", "manuale", GlossaryStatus::UserSeeded),
                stored_term("accepted", "accettato", GlossaryStatus::Accepted),
                stored_term("rejected", "", GlossaryStatus::Rejected),
            ])
            .expect("terms");
        let provider =
            MockProvider::new(bookforge_llm::MockMode::PrefixTarget, "Italian".to_string());

        let run = propose_candidates_with_provider(
            &store,
            &[],
            "book",
            "English",
            "Italian",
            "mock",
            "mock-prefix-target",
            320,
            Some(1_024),
            &provider,
        )
        .await
        .expect("proposal pass");

        assert_eq!(run.proposals.len(), 1);
        let terms = store
            .list_glossary_terms(GlossaryFilter {
                scope_kind: Some(GlossaryScopeKind::Book),
                scope_id: Some("book"),
                source_language: Some("English"),
                target_language: Some("Italian"),
                active_only: false,
            })
            .expect("terms");
        let term = |source: &str| {
            terms
                .iter()
                .find(|term| term.source_text == source)
                .expect("term")
        };
        assert_eq!(term("pending").target_text, "[Italian] pending");
        assert_eq!(term("pending").status, GlossaryStatus::AutoCandidate);
        assert_eq!(term("already proposed").target_text, "esistente");
        assert_eq!(term("seeded").target_text, "manuale");
        assert_eq!(term("accepted").target_text, "accettato");
        assert_eq!(term("rejected").status, GlossaryStatus::Rejected);
    }

    #[tokio::test]
    async fn declined_proposal_leaves_candidate_unrendered_and_pending() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = JobStore::open(directory.path().join("jobs.sqlite")).expect("store");
        store
            .upsert_glossary_candidates(
                "book",
                "English",
                "Italian",
                &[NewGlossaryCandidate {
                    source_text: "dracotron",
                    category: GlossaryCategory::Invented,
                    source_count: 4,
                }],
            )
            .expect("candidate");
        let before = store
            .list_glossary_candidates("book", "English", "Italian")
            .expect("candidate");
        let provider = DecliningProvider { id: before[0].id };

        propose_candidates_with_provider(
            &store,
            &[],
            "book",
            "English",
            "Italian",
            "test",
            "declining",
            320,
            Some(1_024),
            &provider,
        )
        .await
        .expect("decline should be usable");

        let after = store
            .list_glossary_candidates("book", "English", "Italian")
            .expect("candidate");
        assert_eq!(after, before);
        assert_eq!(after[0].status, GlossaryStatus::AutoCandidate);
        assert_eq!(after[0].target_text, None);
    }

    #[tokio::test]
    async fn provider_failure_does_not_modify_candidates() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = JobStore::open(directory.path().join("jobs.sqlite")).expect("store");
        store
            .upsert_glossary_candidates(
                "book",
                "English",
                "Italian",
                &[NewGlossaryCandidate {
                    source_text: "Steelypips",
                    category: GlossaryCategory::Invented,
                    source_count: 5,
                }],
            )
            .expect("candidate");
        let before = store
            .list_glossary_candidates("book", "English", "Italian")
            .expect("candidate");

        let error = propose_candidates_with_provider(
            &store,
            &[],
            "book",
            "English",
            "Italian",
            "test",
            "failing",
            320,
            Some(1_024),
            &FailingProvider,
        )
        .await
        .expect_err("provider failure should surface");

        assert!(
            error.to_string().contains("offline test failure"),
            "{error}"
        );
        let after = store
            .list_glossary_candidates("book", "English", "Italian")
            .expect("candidate");
        assert_eq!(after, before);
    }
}
