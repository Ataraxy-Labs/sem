use serde::Deserialize;

// ── Tool parameter structs ──

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntitiesParams {
    #[schemars(description = "Optional path to a file or directory. If omitted, defaults to '.'.")]
    pub path: Option<String>,
}

impl EntitiesParams {
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref().filter(|p| !p.is_empty())
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DiffParams {
    #[schemars(description = "Base ref to compare from (branch, tag, or commit hash, e.g. 'main')")]
    pub base_ref: String,
    #[schemars(description = "Target ref to compare to. Defaults to HEAD.")]
    pub target_ref: Option<String>,
    #[schemars(description = "Optional: diff only this file")]
    pub file_path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlameParams {
    #[schemars(description = "Path to the file (relative to repo root or absolute)")]
    pub file_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ImpactAnalysisParams {
    #[schemars(description = "Path to the file containing the entity")]
    pub file_path: String,
    #[schemars(description = "Name of the entity to analyze impact for")]
    pub entity_name: String,
    #[schemars(description = "Analysis mode: 'all' (default, shows deps + dependents + transitive impact + tests), 'deps' (direct dependencies only), 'dependents' (direct dependents only), 'tests' (affected test entities only)")]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LogParams {
    #[schemars(description = "Name of the entity to trace history for")]
    pub entity_name: String,
    #[schemars(description = "Path to the file containing the entity. If omitted, auto-detects.")]
    pub file_path: Option<String>,
    #[schemars(description = "Maximum number of commits to analyze. Defaults to 50.")]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContextParams {
    #[schemars(description = "Path to the file containing the entity")]
    pub file_path: String,
    #[schemars(description = "Name of the target entity")]
    pub entity_name: String,
    #[schemars(description = "Maximum token budget. Defaults to 8000.")]
    pub token_budget: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::EntitiesParams;

    #[test]
    fn entities_params_accepts_path() {
        let params: EntitiesParams =
            serde_json::from_value(serde_json::json!({ "path": "src/lib.rs" })).unwrap();

        assert_eq!(params.path(), Some("src/lib.rs"));
    }

    #[test]
    fn entities_params_allows_missing_path() {
        let params: EntitiesParams = serde_json::from_value(serde_json::json!({})).unwrap();

        assert_eq!(params.path(), None);
    }

    #[test]
    fn entities_params_rejects_unknown_fields() {
        let err = serde_json::from_value::<EntitiesParams>(serde_json::json!({
            "unexpected": "src/lib.rs"
        }))
        .unwrap_err();

        assert!(err.to_string().contains("unknown field"));
    }
}
