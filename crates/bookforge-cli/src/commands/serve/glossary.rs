use super::*;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/glossary", get(list_glossary).post(add_glossary))
        .route("/api/glossary/{id}", delete(remove_glossary))
}

// ---------------------------------------------------------------------------
// Glossary (wraps the `glossary` command's JobStore methods)
// ---------------------------------------------------------------------------

pub(super) fn parse_glossary_scope(value: &str) -> GlossaryScopeKind {
    match value {
        "series" => GlossaryScopeKind::Series,
        "book" => GlossaryScopeKind::Book,
        _ => GlossaryScopeKind::Global,
    }
}

pub(super) fn parse_glossary_category(value: &str) -> GlossaryCategory {
    match value {
        "person" => GlossaryCategory::Person,
        "place" => GlossaryCategory::Place,
        "object" => GlossaryCategory::Object,
        "invented" => GlossaryCategory::Invented,
        "style" => GlossaryCategory::Style,
        "phrase" => GlossaryCategory::Phrase,
        _ => GlossaryCategory::Other,
    }
}

fn glossary_term_json(term: &GlossaryTerm) -> serde_json::Value {
    json!({
        "id": term.id,
        "source": term.source_text,
        "target": term.target_text,
        "category": term.category.as_str(),
        "scope": term.scope_kind.as_str(),
        "scope_id": term.scope_id,
        "source_language": term.source_language,
        "target_language": term.target_language,
        "always_active": term.always_active,
        "case_sensitive": term.case_sensitive,
        "notes": term.notes,
    })
}

/// List glossary terms, optionally filtered by `source`/`target` language,
/// `scope` (global/series/book) and `scope_id`.
async fn list_glossary(
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let store_path = state.store_path.clone();
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
        let store = JobStore::open(store_path)?;
        let terms = store.list_glossary_terms(GlossaryFilter {
            scope_kind: params.get("scope").map(|value| parse_glossary_scope(value)),
            scope_id: params
                .get("scope_id")
                .map(String::as_str)
                .filter(|value| !value.is_empty()),
            source_language: params
                .get("source")
                .map(String::as_str)
                .filter(|value| !value.is_empty()),
            target_language: params
                .get("target")
                .map(String::as_str)
                .filter(|value| !value.is_empty()),
            active_only: false,
        })?;
        Ok(terms.iter().map(glossary_term_json).collect())
    })
    .await??;
    Ok(Json(items))
}

#[derive(Deserialize)]
struct GlossaryAddRequest {
    source: String,
    target: String,
    source_language: String,
    target_language: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    scope_id: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    always_active: bool,
}

async fn add_glossary(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<GlossaryAddRequest>,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    if req.source.trim().is_empty() || req.target.trim().is_empty() {
        return Ok(bad_request("source term and translation are required"));
    }
    if req.source_language.trim().is_empty() || req.target_language.trim().is_empty() {
        return Ok(bad_request("source and target languages are required"));
    }
    let scope_kind = req
        .scope
        .as_deref()
        .map(parse_glossary_scope)
        .unwrap_or(GlossaryScopeKind::Global);
    let scope_id = if scope_kind == GlossaryScopeKind::Global {
        None
    } else {
        req.scope_id.filter(|value| !value.trim().is_empty())
    };
    if scope_kind != GlossaryScopeKind::Global && scope_id.is_none() {
        return Ok(bad_request("scope_id is required for series/book scope"));
    }

    let term = GlossaryTerm {
        id: None,
        scope_kind,
        scope_id,
        source_text: req.source.trim().to_string(),
        target_text: req.target.trim().to_string(),
        category: req
            .category
            .as_deref()
            .map(parse_glossary_category)
            .unwrap_or(GlossaryCategory::Other),
        notes: req.notes.filter(|value| !value.trim().is_empty()),
        case_sensitive: req.case_sensitive,
        always_active: req.always_active,
        status: GlossaryStatus::UserSeeded,
        source_language: req.source_language.trim().to_string(),
        target_language: req.target_language.trim().to_string(),
        source_count: 0,
    };

    let store_path = state.store_path.clone();
    let id = tokio::task::spawn_blocking(move || -> Result<i64> {
        let store = JobStore::open(store_path)?;
        Ok(store.add_glossary_term(&term)?)
    })
    .await??;
    Ok(Json(json!({ "id": id })).into_response())
}

async fn remove_glossary(
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let store_path = state.store_path.clone();
    let removed = tokio::task::spawn_blocking(move || -> Result<usize> {
        let store = JobStore::open(store_path)?;
        let removed = store.remove_glossary_term(id)?;
        if removed > 0 {
            eprintln!("[serve] glossary delete id={id} removed={removed}");
        }
        Ok(removed)
    })
    .await??;
    Ok(Json(json!({ "removed": removed })).into_response())
}
