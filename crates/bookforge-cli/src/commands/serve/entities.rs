use super::*;

use bookforge_core::EntityGender;
use bookforge_store::{NewEntity, StoredEntity};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/entities", get(list_entities_route).post(add_entity))
        .route(
            "/api/entities/{id}",
            get(get_entity).put(update_entity).delete(remove_entity),
        )
}

// ---------------------------------------------------------------------------
// Entities (wraps the `entities` command's JobStore methods)
//
// Field parity with `bookforge entities --help` / its import TOML: source and
// target name, optional target-side gender (m/f/n), role, notes, plus the
// scope and language-pair identity. The store has no per-row delete either,
// so removal follows the same snapshot-clear-restore strategy as the style
// routes; ids are therefore session-local handles that may renumber when a
// sibling is removed (the UI reloads after every mutation).
// ---------------------------------------------------------------------------

fn parse_entity_scope(value: &str) -> GlossaryScopeKind {
    super::glossary::parse_glossary_scope(value)
}

fn parse_entity_gender(value: &str) -> Option<EntityGender> {
    match value.trim() {
        "m" | "masculine" => Some(EntityGender::Masculine),
        "f" | "feminine" => Some(EntityGender::Feminine),
        "n" | "neuter" => Some(EntityGender::Neuter),
        _ => None,
    }
}

fn entity_record_json(record: &StoredEntity) -> serde_json::Value {
    json!({
        "id": record.id,
        "source": record.source_name,
        "target": record.target_name,
        "gender": record.gender_target.map(|gender| gender.as_short()),
        "role": record.role,
        "notes": record.notes,
        "scope": record.scope_kind.as_str(),
        "scope_id": record.scope_id,
        "source_language": record.source_language,
        "target_language": record.target_language,
    })
}

#[derive(Deserialize)]
struct EntityAddRequest {
    source_name: String,
    target_name: String,
    #[serde(default)]
    gender: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    source_language: String,
    target_language: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    scope_id: Option<String>,
}

#[derive(Deserialize)]
struct EntityUpdateRequest {
    target_name: String,
    #[serde(default)]
    gender: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    /// Optional echo of the row's identity fields; any mismatch is rejected so
    /// an update can never fork a row.
    #[serde(default)]
    source_name: Option<String>,
    #[serde(default)]
    source_language: Option<String>,
    #[serde(default)]
    target_language: Option<String>,
}

fn resolved_entity_scope(
    scope: Option<&str>,
    scope_id: Option<&str>,
) -> std::result::Result<(GlossaryScopeKind, Option<String>), String> {
    let scope_kind = scope
        .map(parse_entity_scope)
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

/// Shared field validation for create/update requests (import-level rules:
/// non-empty names and languages, known gender code or none).
fn validated_entity_fields(
    source_name: &str,
    target_name: &str,
    source_language: &str,
    target_language: &str,
    gender: Option<&str>,
) -> std::result::Result<Option<EntityGender>, String> {
    if source_name.trim().is_empty() || target_name.trim().is_empty() {
        return Err("source name and rendered name are required".to_string());
    }
    if source_language.trim().is_empty() || target_language.trim().is_empty() {
        return Err("source and target languages are required".to_string());
    }
    let gender = match gender.map(str::trim).filter(|value| !value.is_empty()) {
        Some(code) => Some(
            parse_entity_gender(code)
                .ok_or_else(|| format!("unknown gender '{code}'; use m, f, or n"))?,
        ),
        None => None,
    };
    Ok(gender)
}

fn new_entity<'a>(
    scope_kind: GlossaryScopeKind,
    scope_id: Option<&'a str>,
    source_name: &'a str,
    target_name: &'a str,
    gender_target: Option<EntityGender>,
    role: Option<&'a str>,
    notes: Option<&'a str>,
    source_language: &'a str,
    target_language: &'a str,
) -> NewEntity<'a> {
    NewEntity {
        scope_kind,
        scope_id,
        source_name,
        target_name,
        gender_target,
        role,
        notes,
        source_language,
        target_language,
    }
}

fn locate_entity_record(store: &JobStore, id: i64) -> Result<Option<StoredEntity>> {
    Ok(store
        .list_entities(None, None, None, None)?
        .into_iter()
        .find(|record| record.id == id))
}

fn entity_not_found_response() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "no such entity" })),
    )
        .into_response()
}

async fn list_entities_route(
    Query(params): Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let store_path = state.store_path.clone();
    let items = tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
        let store = JobStore::open(store_path)?;
        let records = store.list_entities(
            params
                .get("source")
                .map(String::as_str)
                .filter(|value| !value.is_empty()),
            params
                .get("target")
                .map(String::as_str)
                .filter(|value| !value.is_empty()),
            params
                .get("scope")
                .map(String::as_str)
                .filter(|value| !value.is_empty())
                .map(parse_entity_scope),
            params
                .get("scope_id")
                .map(String::as_str)
                .filter(|value| !value.is_empty()),
        )?;
        Ok(records.iter().map(entity_record_json).collect())
    })
    .await??;
    Ok(Json(items))
}

async fn add_entity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<EntityAddRequest>,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let requested_scope = match resolved_entity_scope(req.scope.as_deref(), req.scope_id.as_deref())
    {
        Ok(scope) => scope,
        Err(message) => return Ok(bad_request(&message)),
    };
    let gender = match validated_entity_fields(
        &req.source_name,
        &req.target_name,
        &req.source_language,
        &req.target_language,
        req.gender.as_deref(),
    ) {
        Ok(gender) => gender,
        Err(message) => return Ok(bad_request(&message)),
    };

    // upsert_entities reports how many rows changed but not which id, so the
    // freshly written row is read back through the same identity tuple.
    let store_path = state.store_path.clone();
    let source_name = req.source_name.trim().to_string();
    let target_name = req.target_name.trim().to_string();
    let role = req.role.clone();
    let notes = req.notes.clone();
    let source_language = req.source_language.trim().to_string();
    let target_language = req.target_language.trim().to_string();
    let created = tokio::task::spawn_blocking(move || -> Result<i64> {
        let store = JobStore::open(store_path)?;
        let row = new_entity(
            requested_scope.0,
            requested_scope.1.as_deref(),
            &source_name,
            &target_name,
            gender,
            role.as_deref(),
            notes.as_deref(),
            &source_language,
            &target_language,
        );
        store.upsert_entities(std::slice::from_ref(&row))?;
        Ok(store
            .list_entities(None, None, None, None)?
            .into_iter()
            .find(|record| {
                record.scope_kind == requested_scope.0
                    && record.scope_id == requested_scope.1
                    && record.source_name == source_name
                    && record.source_language == source_language
                    && record.target_language == target_language
            })
            .map(|record| record.id)
            .unwrap_or_default())
    })
    .await??;
    Ok(Json(json!({ "id": created })).into_response())
}

async fn get_entity(
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let store_path = state.store_path.clone();
    let found = tokio::task::spawn_blocking(move || -> Result<Option<StoredEntity>> {
        locate_entity_record(&JobStore::open(store_path)?, id)
    })
    .await??;
    match found {
        Some(record) => Ok(Json(entity_record_json(&record)).into_response()),
        None => Ok(entity_not_found_response()),
    }
}

async fn update_entity(
    AxumPath(id): AxumPath<i64>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<EntityUpdateRequest>,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let store_path = state.store_path.clone();
    let fetched = tokio::task::spawn_blocking(move || -> Result<Option<StoredEntity>> {
        locate_entity_record(&JobStore::open(store_path)?, id)
    })
    .await??;
    let Some(existing) = fetched else {
        return Ok(entity_not_found_response());
    };
    // Identity fields decide which row the upsert matches; changing them is a
    // different row, so an echoed mismatch is refused instead of silently
    // creating one.
    if let Some(source_name) = req.source_name.as_deref()
        && source_name.trim() != existing.source_name
    {
        return Ok(bad_request(
            "entity identity cannot change; delete and recreate instead",
        ));
    }
    for (supplied, stored, label) in [
        (
            req.source_language.as_deref(),
            existing.source_language.as_str(),
            "source language",
        ),
        (
            req.target_language.as_deref(),
            existing.target_language.as_str(),
            "target language",
        ),
    ] {
        if let Some(supplied) = supplied
            && supplied.trim() != stored
        {
            return Ok(bad_request(&format!(
                "entity {label} cannot change; delete and recreate instead"
            )));
        }
    }
    let gender = match validated_entity_fields(
        &existing.source_name,
        &req.target_name,
        &existing.source_language,
        &existing.target_language,
        req.gender.as_deref(),
    ) {
        Ok(gender) => gender,
        Err(message) => return Ok(bad_request(&message)),
    };

    let store_path = state.store_path.clone();
    let source_name = existing.source_name;
    let target_name = req.target_name.trim().to_string();
    let role = req.role.clone();
    let notes = req.notes.clone();
    let source_language = existing.source_language;
    let target_language = existing.target_language;
    let scope_kind = existing.scope_kind;
    let scope_id = existing.scope_id;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let store = JobStore::open(store_path)?;
        store.upsert_entities(&[new_entity(
            scope_kind,
            scope_id.as_deref(),
            &source_name,
            &target_name,
            gender,
            role.as_deref(),
            notes.as_deref(),
            &source_language,
            &target_language,
        )])?;
        Ok(())
    })
    .await??;
    Ok(Json(json!({ "updated": true, "id": id })).into_response())
}

async fn remove_entity(
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
        let Some(record) = locate_entity_record(&store, id)? else {
            return Ok(false);
        };
        // Precise single-row removal without a store-layer delete primitive:
        // snapshot every sibling entity in this scope, clear the scope, then
        // restore the siblings verbatim through the public upsert.
        let mut others = store.list_entities(
            None,
            None,
            Some(record.scope_kind),
            record.scope_id.as_deref(),
        )?;
        others.retain(|other| other.id != id);
        store.clear_entities_scope(record.scope_kind, record.scope_id.as_deref())?;
        let rows = others
            .iter()
            .map(|other| {
                new_entity(
                    other.scope_kind,
                    other.scope_id.as_deref(),
                    &other.source_name,
                    &other.target_name,
                    other.gender_target,
                    other.role.as_deref(),
                    other.notes.as_deref(),
                    &other.source_language,
                    &other.target_language,
                )
            })
            .collect::<Vec<_>>();
        store.upsert_entities(&rows)?;
        Ok(true)
    })
    .await??;
    if removed {
        Ok(Json(json!({ "removed": 1 })).into_response())
    } else {
        Ok(entity_not_found_response())
    }
}
