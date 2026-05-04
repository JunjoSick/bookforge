use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error("prompt template '{name}' is missing the '## {section}' section")]
    MissingSection { name: String, section: String },

    #[error("prompt template '{0}' produced an empty section")]
    EmptySection(String),

    #[error("prompt template '{name}' references unknown placeholder '{placeholder}'")]
    UnknownPlaceholder { name: String, placeholder: String },
}

pub type Result<T> = std::result::Result<T, PromptError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    pub name: String,
    pub version: String,
    pub system: String,
    pub user: String,
}

impl PromptTemplate {
    pub fn parse(name: &str, version: &str, source: &str) -> Result<Self> {
        let system = extract_section(name, source, "System")?;
        let user = extract_section(name, source, "User")?;
        Ok(Self {
            name: name.to_string(),
            version: version.to_string(),
            system,
            user,
        })
    }

    pub fn render(&self, vars: &Substitutions) -> Result<Rendered> {
        let system = render_template(&self.name, &self.system, vars)?;
        let user = render_template(&self.name, &self.user, vars)?;
        Ok(Rendered { system, user })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub system: String,
    pub user: String,
}

#[derive(Debug, Default, Clone)]
pub struct Substitutions {
    inner: HashMap<String, String>,
}

impl Substitutions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a string value, JSON-escaping it so it is safe to drop into a
    /// JSON-string context like `"key": "{{value}}"`. The surrounding quotes
    /// stay in the template, only the inner contents are produced.
    pub fn string(&mut self, name: impl Into<String>, value: impl AsRef<str>) -> &mut Self {
        let escaped = json_escape_inner(value.as_ref());
        self.inner.insert(name.into(), escaped);
        self
    }

    /// Insert an integer; the rendered value has no quotes around it.
    pub fn number(&mut self, name: impl Into<String>, value: usize) -> &mut Self {
        self.inner.insert(name.into(), value.to_string());
        self
    }

    /// Insert a value that is already a complete JSON expression (object,
    /// array, or scalar). The expression is pretty-printed and substituted as
    /// a literal block.
    pub fn json<T: Serialize>(&mut self, name: impl Into<String>, value: &T) -> &mut Self {
        let printed = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string());
        self.inner.insert(name.into(), printed);
        self
    }

    /// Insert a JSON value as compact (single-line) output, without
    /// pretty-printing. Use for batch prompts to reduce token usage.
    pub fn json_compact<T: Serialize>(&mut self, name: impl Into<String>, value: &T) -> &mut Self {
        let printed = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
        self.inner.insert(name.into(), printed);
        self
    }

    /// Insert a raw, already-escaped string verbatim. Use when the caller has
    /// already prepared the substitution.
    pub fn raw(&mut self, name: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.inner.insert(name.into(), value.into());
        self
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.inner.get(name).map(String::as_str)
    }
}

fn extract_section(template_name: &str, source: &str, section: &str) -> Result<String> {
    let header = format!("## {section}");
    let start = source
        .find(&header)
        .ok_or_else(|| PromptError::MissingSection {
            name: template_name.to_string(),
            section: section.to_string(),
        })?;

    let after_header = &source[start + header.len()..];
    let body_start = after_header.find('\n').map(|n| n + 1).unwrap_or(0);
    let body = &after_header[body_start..];

    let body = match body.find("\n## ") {
        Some(end) => &body[..end],
        None => body,
    };

    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(PromptError::EmptySection(template_name.to_string()));
    }

    Ok(trimmed.to_string())
}

fn render_template(template_name: &str, body: &str, vars: &Substitutions) -> Result<String> {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;

    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 2..];
        let close = after_open
            .find("}}")
            .ok_or_else(|| PromptError::UnknownPlaceholder {
                name: template_name.to_string(),
                placeholder: "<unterminated>".to_string(),
            })?;
        let placeholder = after_open[..close].trim();
        let value = vars
            .get(placeholder)
            .ok_or_else(|| PromptError::UnknownPlaceholder {
                name: template_name.to_string(),
                placeholder: placeholder.to_string(),
            })?;
        out.push_str(value);
        rest = &after_open[close + 2..];
    }

    out.push_str(rest);
    Ok(out)
}

fn json_escape_inner(value: &str) -> String {
    let serialized = match serde_json::to_string(&Value::String(value.to_string())) {
        Ok(text) => text,
        Err(_) => return value.to_string(),
    };
    if serialized.len() < 2 || !serialized.starts_with('"') || !serialized.ends_with('"') {
        return serialized;
    }
    serialized[1..serialized.len() - 1].to_string()
}

const PLAIN_TEMPLATE_SOURCE: &str = include_str!("../../../prompts/translate_segment.v1.md");
const MARKER_SAFE_TEMPLATE_SOURCE: &str =
    include_str!("../../../prompts/translate_marker_safe.v1.md");
const RUN_PRESERVING_TEMPLATE_SOURCE: &str =
    include_str!("../../../prompts/translate_run_preserving.v1.md");
const QA_TEMPLATE_SOURCE: &str = include_str!("../../../prompts/qa_segment.v1.md");

const BATCH_PLAIN_TEMPLATE_SOURCE: &str =
    include_str!("../../../prompts/translate_batch_plain.v1.md");
const BATCH_MARKER_SAFE_TEMPLATE_SOURCE: &str =
    include_str!("../../../prompts/translate_batch_marker_safe.v1.md");
const BATCH_RUN_PRESERVING_TEMPLATE_SOURCE: &str =
    include_str!("../../../prompts/translate_batch_run_preserving.v1.md");
const BATCH_REPAIR_TEMPLATE_SOURCE: &str =
    include_str!("../../../prompts/translate_batch_repair.v1.md");
const QA_BATCH_TEMPLATE_SOURCE: &str = include_str!("../../../prompts/qa_batch.v1.md");
const DOUBLE_CHECK_BATCH_TEMPLATE_SOURCE: &str =
    include_str!("../../../prompts/double_check_batch.v1.md");
const CORRECT_BATCH_TEMPLATE_SOURCE: &str = include_str!("../../../prompts/correct_batch.v1.md");

#[derive(Debug, Clone)]
pub struct PromptLibrary {
    pub plain: PromptTemplate,
    pub marker_safe: PromptTemplate,
    pub run_preserving: PromptTemplate,
    pub qa: PromptTemplate,
    pub batch_plain: PromptTemplate,
    pub batch_marker_safe: PromptTemplate,
    pub batch_run_preserving: PromptTemplate,
    pub batch_repair: PromptTemplate,
    pub qa_batch: PromptTemplate,
    pub double_check_batch: PromptTemplate,
    pub correct_batch: PromptTemplate,
}

impl PromptLibrary {
    pub fn embedded() -> Self {
        let plain = PromptTemplate::parse("translate_segment", "v1", PLAIN_TEMPLATE_SOURCE)
            .expect("embedded plain template must parse");
        let marker_safe =
            PromptTemplate::parse("translate_marker_safe", "v1", MARKER_SAFE_TEMPLATE_SOURCE)
                .expect("embedded marker-safe template must parse");
        let run_preserving = PromptTemplate::parse(
            "translate_run_preserving",
            "v1",
            RUN_PRESERVING_TEMPLATE_SOURCE,
        )
        .expect("embedded run-preserving template must parse");
        let qa = PromptTemplate::parse("qa_segment", "v1", QA_TEMPLATE_SOURCE)
            .expect("embedded QA template must parse");

        let batch_plain =
            PromptTemplate::parse("translate_batch_plain", "v1", BATCH_PLAIN_TEMPLATE_SOURCE)
                .expect("embedded batch plain template must parse");
        let batch_marker_safe = PromptTemplate::parse(
            "translate_batch_marker_safe",
            "v1",
            BATCH_MARKER_SAFE_TEMPLATE_SOURCE,
        )
        .expect("embedded batch marker-safe template must parse");
        let batch_run_preserving = PromptTemplate::parse(
            "translate_batch_run_preserving",
            "v1",
            BATCH_RUN_PRESERVING_TEMPLATE_SOURCE,
        )
        .expect("embedded batch run-preserving template must parse");
        let batch_repair =
            PromptTemplate::parse("translate_batch_repair", "v1", BATCH_REPAIR_TEMPLATE_SOURCE)
                .expect("embedded batch repair template must parse");
        let qa_batch = PromptTemplate::parse("qa_batch", "v1", QA_BATCH_TEMPLATE_SOURCE)
            .expect("embedded QA batch template must parse");
        let double_check_batch = PromptTemplate::parse(
            "double_check_batch",
            "v1",
            DOUBLE_CHECK_BATCH_TEMPLATE_SOURCE,
        )
        .expect("embedded double-check batch template must parse");
        let correct_batch =
            PromptTemplate::parse("correct_batch", "v1", CORRECT_BATCH_TEMPLATE_SOURCE)
                .expect("embedded correct batch template must parse");

        Self {
            plain,
            marker_safe,
            run_preserving,
            qa,
            batch_plain,
            batch_marker_safe,
            batch_run_preserving,
            batch_repair,
            qa_batch,
            double_check_batch,
            correct_batch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_system_and_user_sections() {
        let source = "# title\n\n## System\n\nrole text\n\n## User\n\npayload text\n";
        let template = PromptTemplate::parse("test", "v1", source).expect("parse");
        assert_eq!(template.system, "role text");
        assert_eq!(template.user, "payload text");
    }

    #[test]
    fn missing_section_is_an_error() {
        let source = "## System\n\nonly system\n";
        let err = PromptTemplate::parse("test", "v1", source).unwrap_err();
        assert!(matches!(err, PromptError::MissingSection { .. }));
    }

    #[test]
    fn substitutes_string_number_and_json_placeholders() {
        let template = PromptTemplate {
            name: "demo".into(),
            version: "v1".into(),
            system: "lang={{language}}".into(),
            user: "id={{id}}\nidx={{index}}\nspans={{spans}}".into(),
        };
        let mut vars = Substitutions::new();
        vars.string("language", "Italian")
            .string("id", "seg_1")
            .number("index", 3)
            .json("spans", &serde_json::json!(["a", "b"]));

        let rendered = template.render(&vars).expect("render");
        assert_eq!(rendered.system, "lang=Italian");
        assert!(rendered.user.contains("id=seg_1"));
        assert!(rendered.user.contains("idx=3"));
        assert!(rendered.user.contains("\"a\""));
    }

    #[test]
    fn json_escapes_quotes_in_string_substitutions() {
        let mut vars = Substitutions::new();
        vars.string("title", r#"Tom "B" Jerry"#);
        let template = PromptTemplate {
            name: "demo".into(),
            version: "v1".into(),
            system: r#""title": "{{title}}""#.into(),
            user: "x".into(),
        };
        let rendered = template.render(&vars).expect("render");
        assert_eq!(rendered.system, r#""title": "Tom \"B\" Jerry""#);
        // The rendered system section must be valid JSON when parsed standalone.
        let parsed: serde_json::Value =
            serde_json::from_str(&format!("{{{}}}", rendered.system)).expect("valid JSON");
        assert_eq!(parsed["title"], "Tom \"B\" Jerry");
    }

    #[test]
    fn unknown_placeholder_is_an_error() {
        let template = PromptTemplate {
            name: "demo".into(),
            version: "v1".into(),
            system: "{{missing}}".into(),
            user: "x".into(),
        };
        let err = template.render(&Substitutions::new()).unwrap_err();
        match err {
            PromptError::UnknownPlaceholder { placeholder, .. } => {
                assert_eq!(placeholder, "missing");
            }
            other => panic!("expected UnknownPlaceholder, got {other:?}"),
        }
    }

    #[test]
    fn embedded_library_loads_all_templates() {
        let library = PromptLibrary::embedded();
        assert_eq!(library.plain.name, "translate_segment");
        assert_eq!(library.plain.version, "v1");
        assert!(library.plain.system.contains("Translate"));
        assert!(library.plain.user.contains("{{segment_id}}"));
        assert_eq!(library.marker_safe.name, "translate_marker_safe");
        assert!(library.marker_safe.user.contains("{{source_blocks_json}}"));
        assert_eq!(library.run_preserving.name, "translate_run_preserving");
        assert!(
            library
                .run_preserving
                .user
                .contains("{{source_run_blocks_json}}")
        );
        assert_eq!(library.qa.name, "qa_segment");
        assert!(library.qa.user.contains("{{translation_text}}"));

        assert_eq!(library.batch_plain.name, "translate_batch_plain");
        assert!(library.batch_plain.user.contains("{{items_json}}"));
        assert_eq!(
            library.batch_marker_safe.name,
            "translate_batch_marker_safe"
        );
        assert!(library.batch_marker_safe.user.contains("{{items_json}}"));
        assert_eq!(
            library.batch_run_preserving.name,
            "translate_batch_run_preserving"
        );
        assert_eq!(library.batch_repair.name, "translate_batch_repair");
        assert!(library.batch_repair.user.contains("{{errors_json}}"));
        assert_eq!(library.qa_batch.name, "qa_batch");
        assert_eq!(library.double_check_batch.name, "double_check_batch");
        assert_eq!(library.correct_batch.name, "correct_batch");
    }
}
