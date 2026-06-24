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
}

impl EntitiesParams {
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref().filter(|p| !p.is_empty())
    }

    pub fn no_default_excludes(&self) -> bool {
        self.no_default_excludes.unwrap_or(false)
    }

    pub fn query(&self) -> Option<&str> {
        self.query.as_deref().map(str::trim).filter(|q| !q.is_empty())
    }

    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(10).clamp(1, 100)
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
    #[schemars(description = "Name of the entity to trace history for")]
    pub entity_name: String,
    #[schemars(description = "Path to the file containing the entity. If omitted, auto-detects.")]
    pub file_path: Option<String>,
    #[schemars(description = "Maximum number of commits to analyze. Defaults to 50.")]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextParams {
    #[schemars(description = "Path to the file containing the entity")]
    pub file_path: String,
    #[schemars(description = "Name of the target entity")]
    pub entity_name: String,
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
    }
}
