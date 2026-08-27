use super::*;

use bookforge_store::{NewStyleSheet, StoredStyleSheet};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/styles",
            get(list_style_sheets_route).post(add_style_sheet),
        )
        .route(
            "/api/styles/{id}",
            get(get_style_sheet)
                .put(update_style_sheet)
                .delete(remove_style_sheet),
        )
}

// ---------------------------------------------------------------------------
// Style sheets (wraps the `style` command's JobStore methods)
//
// The store keys a style sheet on (scope, scope_id, target_language) and has
// no single-row delete, so "remove one sheet" is implemented as
// snapshot-everything-in-the-scope -> clear the scope -> restore every other
// row through the same public upsert API. Each route opens its own store
// connection per request exactly like the glossary routes do.
//
// Consequence worth knowing before depending on it: sibling row ids are
// re-assigned by SQLite during a removal, so treat ids as session-local
// handles (the bundled UI always reloads the list after mutating).
//
// Parity note with the CLI surface: a stored style sheet keys on target
// language only — there is no source-language or display-name column, so the
// API accepts neither.
// ---------------------------------------------------------------------------

fn parse_style_scope(value: &str) -> GlossaryScopeKind {
    super::glossary::parse_glossary_scope(value)
}

fn style_record_json(record: &StoredStyleSheet) -> serde_json::Value {
    json!({
        "id": record.id,
        "target_language": record.target_language,
        "scope": record.scope_kind.as_str(),
        "scope_id": record.scope_id,
        "fingerprint": record.fingerprint,
        "content_toml": record.content_toml,
    })
}

/// Resolve the scope tuple a request targets, mirroring the glossary routes'
/// validation (scoped rows require a non-empty `scope_id`).
fn resolved_style_scope(
    scope: Option<&str>,
    scope_id: Option<&str>,
) -> std::result::Result<(GlossaryScopeKind, Option<String>), String> {
    let scope_kind = scope
        .map(parse_style_scope)
        .unwrap_or(GlossaryScopeKind::Global);
    let scope_id = if scope_kind == GlossaryScopeKind::Global {
        None
    } else {
        match scope_id.filter(|value| !value.trim().is_empty()) {
            Some(id) => Some(id.to_string()),
            None => return Err("scope_id is required for series/book scope".to_string()),
        }
    };
    Ok((scope_kind, scope_id))
}

#[derive(Deserialize)]
struct StyleUpsertRequest {
    target_language: String,
    content_toml: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    scope_id: Option<String>,
}

#[derive(Deserialize)]
struct StyleUpdateRequest {
    content_toml: String,
    #[serde(default)]
    target_language: Option<String>,
}

/// Validate an uploaded payload against the exact TOML schema the CLI import
/// path parses, verify it agrees with the requested identity, and derive the
/// stored fingerprint — the same pipeline `style import` runs. Returns the
/// fingerprint on success.
fn validated_style_sheet(
    target_language: &str,
    content_toml: &str,
    requested_scope: (GlossaryScopeKind, Option<String>),
) -> std::result::Result<String, String> {
    if target_language.trim().is_empty() {
        return Err("target language is required".to_string());
    }
    if content_toml.trim().is_empty() {
        return Err("style sheet content is required".to_string());
    }
    let parsed = crate::commands::style::parse_style_toml(content_toml)
        .map_err(|error| format!("invalid style sheet: {error:#}"))?;
    if parsed.target_language != target_language.trim() {
        return Err(format!(
            "content declares target language '{}' but '{}' was requested",
            parsed.target_language,
            target_language.trim()
        ));
    }
    let (scope_kind, scope_id) = requested_scope;
    if parsed.scope_kind != scope_kind || parsed.scope_id != scope_id {
        return Err(
            "the [meta.scope] inside the content must match the requested scope".to_string(),
        );
    }
    let merged = bookforge_core::style::merge_style_sheets(&[parsed]);
    Ok(bookforge_core::style::style_fingerprint(merged.as_ref()))
}

fn locate_style_record(store: &JobStore, id: i64) -> Result<Option<StoredStyleSheet>> {
    Ok(store
        .list_style_sheets(None, None, None)?
        .into_iter()
        .find(|record| record.id == id))
}

fn style_not_found_response() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "no such style sheet" })),
    )
        .into_response()
}

async fn list_style_sheets_route(
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let store_path = state.store_path.clone();
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
        let store = JobStore::open(store_path)?;
        let records = store.list_style_sheets(
            params
                .get("language")
                .map(String::as_str)
                .filter(|value| !value.is_empty()),
            params
                .get("scope")
                .map(String::as_str)
                .filter(|value| !value.is_empty())
                .map(parse_style_scope),
            params
                .get("scope_id")
                .map(String::as_str)
                .filter(|value| !value.is_empty()),
        )?;
        Ok(records.iter().map(style_record_json).collect())
    })
    .await??;
    Ok(Json(items))
}

async fn get_style_sheet(
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let store_path = state.store_path.clone();
    let found = tokio::task::spawn_blocking(move || -> Result<Option<StoredStyleSheet>> {
        locate_style_record(&JobStore::open(store_path)?, id)
    })
    .await??;
    match found {
        Some(record) => Ok(Json(style_record_json(&record)).into_response()),
        None => Ok(style_not_found_response()),
    }
}

async fn add_style_sheet(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<StyleUpsertRequest>,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let requested_scope = match resolved_style_scope(req.scope.as_deref(), req.scope_id.as_deref())
    {
        Ok(scope) => scope,
        Err(message) => return Ok(bad_request(&message)),
    };
    let fingerprint = match validated_style_sheet(
        &req.target_language,
        &req.content_toml,
        (requested_scope.0, requested_scope.1.clone()),
    ) {
        Ok(fingerprint) => fingerprint,
        Err(message) => return Ok(bad_request(&message)),
    };

    let store_path = state.store_path.clone();
    let target_language = req.target_language.trim().to_string();
    let content = req.content_toml.clone();
    let (scope_kind, scope_id) = requested_scope;
    let upserted = tokio::task::spawn_blocking(move || -> Result<i64> {
        let store = JobStore::open(store_path)?;
        Ok(store.upsert_style_sheet(&NewStyleSheet {
            scope_kind,
            scope_id: scope_id.as_deref(),
            target_language: &target_language,
            content_toml: &content,
            fingerprint: &fingerprint,
        })?)
    })
    .await??;
    Ok(Json(json!({ "id": upserted })).into_response())
}

async fn update_style_sheet(
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<StyleUpdateRequest>,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    if req.content_toml.trim().is_empty() {
        return Ok(bad_request("style sheet content is required"));
    }

    let store_path = state.store_path.clone();
    let fetched = tokio::task::spawn_blocking(move || -> Result<Option<StoredStyleSheet>> {
        locate_style_record(&JobStore::open(store_path)?, id)
    })
    .await??;
    // Identity (scope + target language) is the merge key, so relanguaging is
    // expressed as create + delete rather than a silent fork of the row.
    let Some(existing) = fetched else {
        return Ok(style_not_found_response());
    };
    if let Some(language) = req.target_language.as_deref()
        && language.trim() != existing.target_language
    {
        return Ok(bad_request(
            "style sheets cannot change target language; delete and recreate instead",
        ));
    }
    let requested_scope = (existing.scope_kind, existing.scope_id.clone());
    let fingerprint = match validated_style_sheet(
        &existing.target_language,
        &req.content_toml,
        requested_scope,
    ) {
        Ok(fingerprint) => fingerprint,
        Err(message) => return Ok(bad_request(&message)),
    };

    let store_path = state.store_path.clone();
    let target_language = existing.target_language.clone();
    let scope_kind = existing.scope_kind;
    let scope_id = existing.scope_id;
    let content = req.content_toml.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let store = JobStore::open(store_path)?;
        store.upsert_style_sheet(&NewStyleSheet {
            scope_kind,
            scope_id: scope_id.as_deref(),
            target_language: &target_language,
            content_toml: &content,
            fingerprint: &fingerprint,
        })?;
        Ok(())
    })
    .await??;
    Ok(Json(json!({ "updated": true, "id": id })).into_response())
}

async fn remove_style_sheet(
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let store_path = state.store_path.clone();
    let removed = tokio::task::spawn_blocking(move || -> Result<bool> {
        let store = JobStore::open(store_path)?;
        let Some(record) = locate_style_record(&store, id)? else {
            return Ok(false);
        };
        // Precise single-row removal without a store-layer delete primitive:
        // snapshot every sibling sheet in this scope, clear the scope, then
        // restore the siblings verbatim through the public upsert.
        let mut others =
            store.list_style_sheets(None, Some(record.scope_kind), record.scope_id.as_deref())?;
        others.retain(|other| other.id != id);
        store.clear_style_scope(record.scope_kind, record.scope_id.as_deref())?;
        for other in &others {
            store.upsert_style_sheet(&NewStyleSheet {
                scope_kind: other.scope_kind,
                scope_id: other.scope_id.as_deref(),
                target_language: &other.target_language,
                content_toml: &other.content_toml,
                fingerprint: &other.fingerprint,
            })?;
        }
        Ok(true)
    })
    .await??;
    if removed {
        Ok(Json(json!({ "removed": 1 })).into_response())
    } else {
        Ok(style_not_found_response())
    }
}
