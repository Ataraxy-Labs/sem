use serde::Deserialize;

// ── Tool parameter structs ──

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntitiesParams {
    #[schemars(description = "Optional path to a file or directory. If omitted, defaults to '.'.")]
    pub path: Option<String>,
    #[schemars(
        description = "Include files and directories excluded by default, including generated, fixture, vendor, and benchmark paths."
    )]
    pub no_default_excludes: Option<bool>,
    #[schemars(
        description = "Optional free-text query to find entities by intent across the whole repo (e.g. \"where is the retry logic\"), when you don't know the entity name. Ranks by name/signature relevance and graph centrality; returns file:line and dependent counts. Ignores `path` when set."
    )]
    pub query: Option<String>,
    #[schemars(description = "Max results for `query` mode (default 10).")]
    pub limit: Option<usize>,
    #[schemars(
        description = "Exact substring to search for inside entity bodies across the whole repo (use instead of grep for strings, error messages, config keys). Case-sensitive. Hits come back entity-addressed: file, innermost entity, line, matched line text."
    )]
    pub text: Option<String>,
    #[schemars(
        description = "Show each listed entity's header: its signature up to the body plus the first doc-comment line — more than a name, far less than a body."
    )]
    pub signatures: Option<bool>,
    #[schemars(description = "Output format: \"text\" (default) or \"json\".")]
    pub format: Option<String>,
}

impl EntitiesParams {
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref().filter(|p| !p.is_empty())
    }

    pub fn no_default_excludes(&self) -> bool {
        self.no_default_excludes.unwrap_or(false)
    }

    pub fn query(&self) -> Option<&str> {
        self.query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
    }

    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(10).clamp(1, 100)
    }

    pub fn text(&self) -> Option<&str> {
        self.text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    }

    pub fn signatures(&self) -> bool {
        self.signatures.unwrap_or(false)
    }

    pub fn format(&self) -> &str {
        self.format.as_deref().unwrap_or("text")
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiffParams {
    #[schemars(
        description = "Base ref to compare from (branch, tag, or commit hash, e.g. 'main'). If omitted, shows working-tree changes (like `sem diff`)."
    )]
    pub base_ref: Option<String>,
    #[schemars(description = "Target ref to compare to. Defaults to HEAD.")]
    pub target_ref: Option<String>,
    #[schemars(description = "Optional: diff only this file")]
    pub file_path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameParams {
    #[schemars(description = "Path to the file (relative to repo root or absolute)")]
    pub file_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImpactAnalysisParams {
    #[schemars(description = "Path to the file containing the entity")]
    pub file_path: String,
    #[schemars(description = "Name of the entity to analyze impact for")]
    pub entity_name: String,
    #[schemars(
        description = "Analysis mode: 'all' (default, shows deps + dependents + transitive impact + tests), 'deps' (direct dependencies only), 'dependents' (direct dependents only), 'tests' (affected test entities only)"
    )]
    pub mode: Option<String>,
    #[schemars(
        description = "Include files and directories excluded by default, including generated, fixture, vendor, and benchmark paths."
    )]
    pub no_default_excludes: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogParams {
    #[schemars(
        description = "Name of the entity to trace history for. Omit for repo-level history analytics: hotspots (most-changed entities) and co-change pairs (entities that change in the same commits)."
    )]
    pub entity_name: Option<String>,
    #[schemars(description = "Path to the file containing the entity. If omitted, auto-detects.")]
    pub file_path: Option<String>,
    #[schemars(description = "Maximum number of commits to analyze. Defaults to 50.")]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextParams {
    #[schemars(
        description = "Path to the file containing the entity. OPTIONAL: omit it for one-call lookup — the entity_name is resolved across the whole repo (ambiguity returns a compact candidate list)."
    )]
    pub file_path: Option<String>,
    #[schemars(description = "Name of the target entity")]
    pub entity_name: Option<String>,
    #[schemars(
        description = "Several target entities to pack in one call (batch form of `entity_name`), each packed under the same token_budget. Text output concatenates the per-entity blocks; json output is newline-delimited, one object per entity."
    )]
    pub entities: Option<Vec<String>>,
    #[schemars(description = "Maximum token budget. Defaults to 8000.")]
    pub token_budget: Option<usize>,
    #[schemars(
        description = "Bound related entities to this many graph hops from the target (0 or omitted = unbounded, fill to the token budget). Use e.g. 1-2 for the immediate neighborhood."
    )]
    pub hops: Option<usize>,
    #[schemars(
        description = "Include files and directories excluded by default, including generated, fixture, vendor, and benchmark paths."
    )]
    pub no_default_excludes: Option<bool>,
    #[schemars(
        description = "Force a full re-send even if this session already received an identical fill for the entity (use when your context was compacted and the earlier body is gone)."
    )]
    pub fresh: Option<bool>,
    #[schemars(description = "Output format: \"text\" (default) or \"json\".")]
    pub format: Option<String>,
    #[schemars(
        description = "Set to \"headers\" to render each packed entity as its header (signature plus first doc-comment line) instead of its body — the same token_budget buys a much wider map."
    )]
    pub mode: Option<String>,
}

impl ContextParams {
    pub fn format(&self) -> &str {
        self.format.as_deref().unwrap_or("text")
    }

    /// Batch entries, when the batch form is in use (non-empty `entities`).
    pub fn entities(&self) -> Option<&[String]> {
        self.entities.as_deref().filter(|e| !e.is_empty())
    }

    pub fn wants_headers(&self) -> bool {
        self.mode.as_deref() == Some("headers")
    }
}

// ── Find / Grep tool parameter structs ──

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindParams {
    #[schemars(
        description = "Entity name to look up, optionally as \"type name\" (e.g. \"function createProgram\") to disambiguate by kind."
    )]
    pub query: Option<String>,
    #[schemars(
        description = "Several names to look up in one call (batch form of `query`). Each resolves independently; a miss on one never affects the others."
    )]
    pub queries: Option<Vec<String>>,
    #[schemars(description = "Restrict to entities defined in this file.")]
    pub file: Option<String>,
    #[schemars(description = "Output format: \"text\" (default) or \"json\".")]
    pub format: Option<String>,
}

impl FindParams {
    pub fn file(&self) -> Option<&str> {
        self.file.as_deref().filter(|f| !f.is_empty())
    }

    pub fn format(&self) -> &str {
        self.format.as_deref().unwrap_or("text")
    }

    /// Batch entries, when the batch form is in use (non-empty `queries`).
    pub fn queries(&self) -> Option<&[String]> {
        self.queries.as_deref().filter(|q| !q.is_empty())
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepParams {
    #[schemars(
        description = "Regex or literal pattern to search file contents for (rg-compatible)."
    )]
    pub pattern: Option<String>,
    #[schemars(
        description = "Several patterns to search in one call (batch form of `pattern`). Each pattern's hits are reported separately, never merged."
    )]
    pub patterns: Option<Vec<String>>,
    #[schemars(description = "Case-insensitive match.")]
    pub case_insensitive: Option<bool>,
    #[schemars(description = "Output format: \"text\" (default) or \"json\".")]
    pub format: Option<String>,
}

impl GrepParams {
    pub fn case_insensitive(&self) -> bool {
        self.case_insensitive.unwrap_or(false)
    }

    pub fn format(&self) -> &str {
        self.format.as_deref().unwrap_or("text")
    }

    /// Batch entries, when the batch form is in use (non-empty `patterns`).
    pub fn patterns(&self) -> Option<&[String]> {
        self.patterns.as_deref().filter(|p| !p.is_empty())
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CallersParams {
    #[schemars(
        description = "Entity whose callers to list, optionally as \"type name\" (e.g. \"function createProgram\") to disambiguate by kind. Must resolve to exactly one definition; an ambiguous name is refused with the full candidate list."
    )]
    pub query: String,
    #[schemars(description = "Restrict to the definition in this file (disambiguates).")]
    pub file: Option<String>,
    #[schemars(description = "Return at most this many callers (all by default).")]
    pub limit: Option<usize>,
    #[schemars(description = "Output format: \"text\" (default) or \"json\".")]
    pub format: Option<String>,
}

impl CallersParams {
    pub fn file(&self) -> Option<&str> {
        self.file.as_deref().filter(|f| !f.is_empty())
    }

    pub fn format(&self) -> &str {
        self.format.as_deref().unwrap_or("text")
    }
}

// ── Review listener tool parameter structs ──

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JoinReviewParams {
    #[schemars(description = "The sem-cloud diff id to listen on (from the review URL).")]
    pub diff_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitForBranchParams {
    #[schemars(description = "The diff id to poll (same one passed to join_review).")]
    pub diff_id: String,
    #[schemars(
        description = "How long to long-poll for the next reviewer question, in seconds (default 40, clamped to 45). Keep calling this tool back-to-back — on timeout or right after replying — for as long as you are listening."
    )]
    pub wait_seconds: Option<u64>,
}

impl WaitForBranchParams {
    /// Clamped, defaulted wait, in whole seconds.
    pub fn wait_seconds(&self) -> u64 {
        self.wait_seconds.unwrap_or(40).min(45)
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplyToBranchParams {
    #[schemars(description = "The diff id the comment belongs to.")]
    pub diff_id: String,
    #[schemars(
        description = "The commentId of the branch/question being answered (from the branch payload wait_for_branch returned)."
    )]
    pub comment_id: String,
    #[schemars(
        description = "The answer text. When partial is true this must be the FULL cumulative answer composed so far, not just the newest chunk — each partial call replaces the streamed text."
    )]
    pub content: String,
    #[schemars(
        description = "true while still composing (streams `content` so the reviewer watches it build); omit or pass false on the final call, which commits the answer."
    )]
    pub partial: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListOpenBranchesParams {
    #[schemars(description = "The diff id to inspect (same one passed to join_review).")]
    pub diff_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::tool::parse_json_object;
    use rmcp::model::ErrorCode;
    use serde::de::DeserializeOwned;
    use std::fmt::Debug;

    fn assert_unknown_fields_return_invalid_params<T>()
    where
        T: Debug + DeserializeOwned,
    {
        let arguments = serde_json::json!({
            "deliberate_bogus_field": 1
        })
        .as_object()
        .unwrap()
        .clone();
        let err = parse_json_object::<T>(arguments).unwrap_err();

        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        assert!(
            err.message.contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn entities_params_accepts_path() {
        let params: EntitiesParams =
            serde_json::from_value(serde_json::json!({ "path": "src/lib.rs" })).unwrap();

        assert_eq!(params.path(), Some("src/lib.rs"));
        assert!(!params.no_default_excludes());
    }

    #[test]
    fn entities_params_allows_missing_path() {
        let params: EntitiesParams = serde_json::from_value(serde_json::json!({})).unwrap();

        assert_eq!(params.path(), None);
        assert!(!params.no_default_excludes());
    }

    #[test]
    fn entities_params_accepts_no_default_excludes() {
        let params: EntitiesParams =
            serde_json::from_value(serde_json::json!({ "no_default_excludes": true })).unwrap();

        assert!(params.no_default_excludes());
    }

    #[test]
    fn all_tool_params_return_invalid_params_for_unknown_fields() {
        assert_unknown_fields_return_invalid_params::<EntitiesParams>();
        assert_unknown_fields_return_invalid_params::<DiffParams>();
        assert_unknown_fields_return_invalid_params::<BlameParams>();
        assert_unknown_fields_return_invalid_params::<ImpactAnalysisParams>();
        assert_unknown_fields_return_invalid_params::<LogParams>();
        assert_unknown_fields_return_invalid_params::<ContextParams>();
        assert_unknown_fields_return_invalid_params::<FindParams>();
        assert_unknown_fields_return_invalid_params::<GrepParams>();
        assert_unknown_fields_return_invalid_params::<CallersParams>();
        assert_unknown_fields_return_invalid_params::<JoinReviewParams>();
        assert_unknown_fields_return_invalid_params::<WaitForBranchParams>();
        assert_unknown_fields_return_invalid_params::<ReplyToBranchParams>();
        assert_unknown_fields_return_invalid_params::<ListOpenBranchesParams>();
    }
}
