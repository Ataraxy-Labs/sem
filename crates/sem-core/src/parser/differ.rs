use rayon::prelude::*;
use serde::Serialize;

use crate::git::types::FileChange;
use crate::model::change::{ChangeType, SemanticChange};
use crate::model::identity::match_entities;
use crate::parser::registry::ParserRegistry;
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffResult {
    pub changes: Vec<SemanticChange>,
    pub file_count: usize,
    pub added_count: usize,
    pub modified_count: usize,
    pub deleted_count: usize,
    pub moved_count: usize,
    pub renamed_count: usize,
}

pub fn compute_semantic_diff(
    file_changes: &[FileChange],
    registry: &ParserRegistry,
    commit_sha: Option<&str>,
    author: Option<&str>,
) -> DiffResult {
    // Process files in parallel: each file's entity extraction and matching is independent
    let per_file_changes: Vec<(String, Vec<SemanticChange>)> = file_changes
        .par_iter()
        .filter_map(|file| {
            let plugin = registry.get_plugin(&file.file_path)?;

            let before_entities = if let Some(ref content) = file.before_content {
                let before_path = file.old_file_path.as_deref().unwrap_or(&file.file_path);
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    plugin.extract_entities(content, before_path)
                })) {
                    Ok(entities) => entities,
                    Err(_) => Vec::new(),
                }
            } else {
                Vec::new()
            };

            let after_entities = if let Some(ref content) = file.after_content {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    plugin.extract_entities(content, &file.file_path)
                })) {
                    Ok(entities) => entities,
                    Err(_) => Vec::new(),
                }
            } else {
                Vec::new()
            };

            let sim_fn = |a: &crate::model::entity::SemanticEntity,
                          b: &crate::model::entity::SemanticEntity|
             -> f64 { plugin.compute_similarity(a, b) };

            let result = match_entities(
                &before_entities,
                &after_entities,
                &file.file_path,
                Some(&sim_fn),
                commit_sha,
                author,
            );

            if result.changes.is_empty() {
                None
            } else {
                Some((file.file_path.clone(), result.changes))
            }
        })
        .collect();

    let mut all_changes: Vec<SemanticChange> = Vec::new();
    let mut files_with_changes: HashSet<String> = HashSet::new();
    for (file_path, changes) in per_file_changes {
        files_with_changes.insert(file_path);
        all_changes.extend(changes);
    }

    // Single-pass counting
    let mut added_count = 0;
    let mut modified_count = 0;
    let mut deleted_count = 0;
    let mut moved_count = 0;
    let mut renamed_count = 0;

    for c in &all_changes {
        match c.change_type {
            ChangeType::Added => added_count += 1,
            ChangeType::Modified => modified_count += 1,
            ChangeType::Deleted => deleted_count += 1,
            ChangeType::Moved => moved_count += 1,
            ChangeType::Renamed => renamed_count += 1,
        }
    }

    DiffResult {
        changes: all_changes,
        file_count: files_with_changes.len(),
        added_count,
        modified_count,
        deleted_count,
        moved_count,
        renamed_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::types::{FileChange, FileStatus};
    use crate::parser::plugins::create_default_registry;

    fn svelte_diff(before: Option<&str>, after: Option<&str>) -> DiffResult {
        let registry = create_default_registry();
        compute_semantic_diff(
            &[FileChange {
                file_path: "src/routes/+page.svelte".to_string(),
                status: if before.is_none() {
                    FileStatus::Added
                } else if after.is_none() {
                    FileStatus::Deleted
                } else {
                    FileStatus::Modified
                },
                old_file_path: None,
                before_content: before.map(str::to_string),
                after_content: after.map(str::to_string),
            }],
            &registry,
            Some("abc123"),
            Some("test-author"),
        )
    }

    #[test]
    fn test_svelte_new_file_all_entities_added() {
        // line 1: <script>
        // line 2:   let count = $state(0);
        // line 3: </script>
        // line 4: (blank)
        // line 5: <button ...>{count}</button>
        let after = r#"<script>
  let count = $state(0);
</script>

<button onclick={() => count++}>{count}</button>"#;

        let result = svelte_diff(None, Some(after));

        assert!(result.added_count > 0, "expected added entities");
        assert_eq!(result.deleted_count, 0);
        assert_eq!(result.modified_count, 0);
        assert_eq!(result.file_count, 1);

        assert!(
            result.changes.iter().all(|c| c.change_type == ChangeType::Added),
            "all changes should be Added for a new file: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type)).collect::<Vec<_>>()
        );

        assert!(
            result.changes.iter().any(|c| c.entity_name == "script" && c.entity_type == "svelte_instance_script"),
            "expected script entity: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type)).collect::<Vec<_>>()
        );
        assert!(
            result.changes.iter().any(|c| c.entity_name == "count" && c.entity_type == "variable"),
            "expected count variable: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type)).collect::<Vec<_>>()
        );
        // button is on line 5
        assert!(
            result.changes.iter().any(|c| c.entity_name == "button@5" && c.entity_type == "svelte_element"),
            "expected button@5 element: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type)).collect::<Vec<_>>()
        );
        // expression tag inside button, also line 5
        assert!(
            result.changes.iter().any(|c| c.entity_name == "expression@5" && c.entity_type == "svelte_expression_tag"),
            "expected expression@5 tag: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type)).collect::<Vec<_>>()
        );

        // commit_sha and author should be populated
        for c in &result.changes {
            assert_eq!(c.commit_sha.as_deref(), Some("abc123"));
            assert_eq!(c.author.as_deref(), Some("test-author"));
            assert_eq!(c.file_path, "src/routes/+page.svelte");
        }
    }

    #[test]
    fn test_svelte_deleted_file_all_entities_deleted() {
        // line 1: <script>
        // line 2:   let name = "world";
        // line 3: </script>
        // line 4: (blank)
        // line 5: <h1>Hello {name}!</h1>
        let before = r#"<script>
  let name = "world";
</script>

<h1>Hello {name}!</h1>"#;

        let result = svelte_diff(Some(before), None);

        assert!(result.deleted_count > 0, "expected deleted entities");
        assert_eq!(result.added_count, 0);
        assert_eq!(result.modified_count, 0);

        assert!(
            result.changes.iter().all(|c| c.change_type == ChangeType::Deleted),
            "all changes should be Deleted for a removed file: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type)).collect::<Vec<_>>()
        );

        // h1 is on line 5, expression inside it also line 5
        assert!(
            result.changes.iter().any(|c| c.entity_name == "h1@5" && c.entity_type == "svelte_element"),
            "expected h1@5 element in deleted changes: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type)).collect::<Vec<_>>()
        );
        assert!(
            result.changes.iter().any(|c| c.entity_name == "expression@5" && c.entity_type == "svelte_expression_tag"),
            "expected expression@5 tag in deleted changes: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_svelte_add_element_produces_added_change() {
        let before = r#"<div>hello</div>"#;
        // p is on line 2 of the after content
        let after = r#"<div>hello</div>
<p>new paragraph</p>"#;

        let result = svelte_diff(Some(before), Some(after));

        assert!(
            result.changes.iter().any(|c| c.entity_name == "p@2" && c.change_type == ChangeType::Added),
            "expected p@2 element to be Added: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_svelte_remove_element_produces_deleted_change() {
        // p is on line 2 of the before content
        let before = r#"<div>hello</div>
<p>paragraph</p>"#;
        let after = r#"<div>hello</div>"#;

        let result = svelte_diff(Some(before), Some(after));

        assert!(
            result.changes.iter().any(|c| c.entity_name == "p@2" && c.change_type == ChangeType::Deleted),
            "expected p@2 element to be Deleted: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_svelte_modify_element_content_is_structural() {
        // button is on line 1 in both before and after
        let before = r#"<button>Click me</button>"#;
        let after = r#"<button>Submit form</button>"#;

        let result = svelte_diff(Some(before), Some(after));

        assert!(
            result.changes.iter().any(|c| c.entity_name == "button@1"
                && c.change_type == ChangeType::Modified
                && c.structural_change == Some(true)),
            "expected button@1 to be Modified with structural_change=true: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type, &c.structural_change)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_svelte_add_script_block() {
        // before: div@1; after: script (lines 1-3), x (line 2), div@5
        let before = r#"<div>hello</div>"#;
        let after = r#"<script>
  let x = 1;
</script>

<div>hello</div>"#;

        let result = svelte_diff(Some(before), Some(after));

        assert!(
            result.changes.iter().any(|c| c.entity_name == "script"
                && c.entity_type == "svelte_instance_script"
                && c.change_type == ChangeType::Added),
            "expected script to be Added: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type, &c.change_type)).collect::<Vec<_>>()
        );

        assert!(
            result.changes.iter().any(|c| c.entity_name == "x"
                && c.entity_type == "variable"
                && c.change_type == ChangeType::Added),
            "expected variable x to be Added: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type, &c.change_type)).collect::<Vec<_>>()
        );

        // div moved from line 1 to line 5 but content unchanged
        assert!(
            result.changes.iter().any(|c| c.entity_name == "div@5"
                && c.entity_type == "svelte_element"),
            "expected div@5 in changes: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type, &c.change_type)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_svelte_remove_script_block() {
        let before = r#"<script>
  let x = 1;
</script>

<div>hello</div>"#;
        let after = r#"<div>hello</div>"#;

        let result = svelte_diff(Some(before), Some(after));

        assert!(
            result.changes.iter().any(|c| c.entity_name == "script"
                && c.entity_type == "svelte_instance_script"
                && c.change_type == ChangeType::Deleted),
            "expected script block to be Deleted: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type, &c.change_type)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_svelte_modify_script_logic() {
        // script lines 1-5, greet lines 2-4, p@7
        let before = r#"<script>
  function greet() {
    return "hello";
  }
</script>

<p>{greet()}</p>"#;

        let after = r#"<script>
  function greet() {
    return "goodbye";
  }
</script>

<p>{greet()}</p>"#;

        let result = svelte_diff(Some(before), Some(after));

        assert!(
            result.changes.iter().any(|c| c.entity_name == "greet"
                && c.entity_type == "function"
                && c.change_type == ChangeType::Modified
                && c.structural_change == Some(true)),
            "expected greet to be Modified structurally: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type, &c.change_type, &c.structural_change)).collect::<Vec<_>>()
        );

        // script itself should also be modified since its child changed
        assert!(
            result.changes.iter().any(|c| c.entity_name == "script"
                && c.entity_type == "svelte_instance_script"
                && c.change_type == ChangeType::Modified),
            "expected script to be Modified: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type, &c.change_type)).collect::<Vec<_>>()
        );

        // p@7 should NOT be modified (same content)
        assert!(
            !result.changes.iter().any(|c| c.entity_name == "p@7"
                && c.change_type == ChangeType::Modified),
            "p@7 should not be modified since content is identical: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_svelte_add_if_block() {
        // before: p@1; after: if@1, p@2 (inside if), p@4 (the original)
        let before = r#"<p>always shown</p>"#;
        let after = r#"{#if visible}
  <p>conditionally shown</p>
{/if}
<p>always shown</p>"#;

        let result = svelte_diff(Some(before), Some(after));

        assert!(
            result.changes.iter().any(|c| c.entity_name == "if@1"
                && c.entity_type == "svelte_if_block"
                && c.change_type == ChangeType::Added),
            "expected if@1 block to be Added: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type, &c.change_type)).collect::<Vec<_>>()
        );
        // p inside the if block is on line 2
        assert!(
            result.changes.iter().any(|c| c.entity_name == "p@2"
                && c.entity_type == "svelte_element"
                && c.change_type == ChangeType::Added),
            "expected p@2 inside if block to be Added: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type, &c.change_type)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_svelte_add_each_block() {
        // after: ul@1 (modified), each@2, li@3 inside each
        let before = r#"<ul></ul>"#;
        let after = r#"<ul>
  {#each items as item}
    <li>{item}</li>
  {/each}
</ul>"#;

        let result = svelte_diff(Some(before), Some(after));

        assert!(
            result.changes.iter().any(|c| c.entity_name == "each@2"
                && c.entity_type == "svelte_each_block"
                && c.change_type == ChangeType::Added),
            "expected each@2 block to be Added: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type, &c.change_type)).collect::<Vec<_>>()
        );
        // li inside each on line 3
        assert!(
            result.changes.iter().any(|c| c.entity_name == "li@3"
                && c.entity_type == "svelte_element"
                && c.change_type == ChangeType::Added),
            "expected li@3 inside each to be Added: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.entity_type, &c.change_type)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_svelte_whitespace_only_change_is_non_structural() {
        let before = r#"<div class="app"><span>text</span></div>"#;
        let after = r#"<div class="app">
  <span>text</span>
</div>"#;

        let result = svelte_diff(Some(before), Some(after));

        // If there are changes, they should all be non-structural
        for c in &result.changes {
            if c.change_type == ChangeType::Modified {
                assert_eq!(
                    c.structural_change,
                    Some(false),
                    "whitespace-only change for {} should be non-structural",
                    c.entity_name
                );
            }
        }
    }

    #[test]
    fn test_svelte_identical_content_produces_no_changes() {
        let content = r#"<script>
  let x = 1;
</script>

<div>{x}</div>"#;

        let result = svelte_diff(Some(content), Some(content));

        assert!(
            result.changes.is_empty(),
            "identical content should produce no changes: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type)).collect::<Vec<_>>()
        );
        assert_eq!(result.file_count, 0);
    }

    #[test]
    fn test_svelte_diff_counts_are_consistent() {
        // before: div@5, p@6; after: div@5, span@6
        let before = r#"<script>
  let a = 1;
</script>

<div>old</div>
<p>to remove</p>"#;

        let after = r#"<script>
  let a = 2;
</script>

<div>new</div>
<span>added</span>"#;

        let result = svelte_diff(Some(before), Some(after));

        // Verify counts match actual changes
        let actual_added = result.changes.iter().filter(|c| c.change_type == ChangeType::Added).count();
        let actual_modified = result.changes.iter().filter(|c| c.change_type == ChangeType::Modified).count();
        let actual_deleted = result.changes.iter().filter(|c| c.change_type == ChangeType::Deleted).count();

        assert_eq!(result.added_count, actual_added, "added_count mismatch");
        assert_eq!(result.modified_count, actual_modified, "modified_count mismatch");
        assert_eq!(result.deleted_count, actual_deleted, "deleted_count mismatch");
        assert_eq!(result.file_count, 1);

        // p@6 should be deleted, span@6 should be added
        assert!(
            result.changes.iter().any(|c| c.entity_name == "p@6" && c.change_type == ChangeType::Deleted),
            "expected p@6 to be Deleted: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type)).collect::<Vec<_>>()
        );
        assert!(
            result.changes.iter().any(|c| c.entity_name == "span@6" && c.change_type == ChangeType::Added),
            "expected span@6 to be Added: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type)).collect::<Vec<_>>()
        );
        // div@5 and script should be modified (content changed)
        assert!(
            result.changes.iter().any(|c| c.entity_name == "div@5" && c.change_type == ChangeType::Modified),
            "expected div@5 to be Modified: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type)).collect::<Vec<_>>()
        );
        assert!(
            result.changes.iter().any(|c| c.entity_name == "a" && c.change_type == ChangeType::Modified),
            "expected variable a to be Modified: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_svelte_component_swap() {
        // Both components on line 1
        let before = r#"<Button>Click</Button>"#;
        let after = r#"<Link>Click</Link>"#;

        let result = svelte_diff(Some(before), Some(after));

        assert!(
            result.changes.iter().any(|c| c.entity_name == "Button@1" && c.change_type == ChangeType::Deleted),
            "expected Button@1 to be Deleted: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type)).collect::<Vec<_>>()
        );
        assert!(
            result.changes.iter().any(|c| c.entity_name == "Link@1" && c.change_type == ChangeType::Added),
            "expected Link@1 to be Added: {:?}",
            result.changes.iter().map(|c| (&c.entity_name, &c.change_type)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_svelte_change_content_includes_before_and_after() {
        // p is on line 1 in both
        let before = r#"<p>old text</p>"#;
        let after = r#"<p>new text</p>"#;

        let result = svelte_diff(Some(before), Some(after));

        let p_change = result.changes.iter()
            .find(|c| c.entity_name == "p@1" && c.entity_type == "svelte_element" && c.change_type == ChangeType::Modified)
            .expect("expected p@1 to be Modified");

        assert!(p_change.before_content.is_some(), "before_content should be set");
        assert!(p_change.after_content.is_some(), "after_content should be set");
        assert_ne!(p_change.before_content, p_change.after_content, "before and after should differ");
    }

    #[test]
    fn test_svelte_tag_comment_diff_is_non_structural() {
        let registry = create_default_registry();
        let before = r#"<div class="app"></div>"#;
        let after = r#"<div // Svelte 5 tag comment
class="app"></div>"#;

        let result = compute_semantic_diff(
            &[FileChange {
                file_path: "src/routes/+page.svelte".to_string(),
                status: FileStatus::Modified,
                old_file_path: None,
                before_content: Some(before.to_string()),
                after_content: Some(after.to_string()),
            }],
            &registry,
            None,
            None,
        );

        assert!(
            result
                .changes
                .iter()
                .any(|change| change.entity_type == "svelte_element"
                    && change.structural_change == Some(false)),
            "expected regular element tag comment change to be treated as non-structural: {:?}",
            result.changes
        );

        assert!(
            result
                .changes
                .iter()
                .any(|change| change.entity_type == "svelte_fragment"
                    && change.structural_change == Some(false)),
            "expected fragment tag comment change to be treated as non-structural: {:?}",
            result.changes
        );
    }

    #[test]
    fn test_svelte_block_tag_comment_diff_is_non_structural() {
        let registry = create_default_registry();
        let before = r#"<div class="app"></div>"#;
        let after = r#"<div /* Svelte 5 tag comment */
class="app"></div>"#;

        let result = compute_semantic_diff(
            &[FileChange {
                file_path: "src/routes/+page.svelte".to_string(),
                status: FileStatus::Modified,
                old_file_path: None,
                before_content: Some(before.to_string()),
                after_content: Some(after.to_string()),
            }],
            &registry,
            None,
            None,
        );

        assert!(
            result
                .changes
                .iter()
                .any(|change| change.entity_type == "svelte_element"
                    && change.structural_change == Some(false)),
            "expected regular element block comment change to be treated as non-structural: {:?}",
            result.changes
        );

        assert!(
            result
                .changes
                .iter()
                .any(|change| change.entity_type == "svelte_fragment"
                    && change.structural_change == Some(false)),
            "expected fragment block comment change to be treated as non-structural: {:?}",
            result.changes
        );
    }
}
