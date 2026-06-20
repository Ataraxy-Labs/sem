use regex::Regex;
use std::collections::HashMap;

use crate::model::entity::{build_entity_id, build_entity_id_disambiguated, SemanticEntity};
use crate::parser::plugin::SemanticParserPlugin;
use crate::utils::hash::content_hash;

pub struct LatexParserPlugin;

const SIGNIFICANT_ENVIRONMENTS: &[&str] = &[
    "theorem",
    "lemma",
    "corollary",
    "proposition",
    "definition",
    "proof",
    "example",
    "remark",
    "figure",
    "table",
    "listing",
    "algorithm",
    "abstract",
    "appendix",
];

/// Map LaTeX sectioning commands to hierarchy levels.
fn section_level(cmd: &str) -> Option<usize> {
    match cmd {
        "part" => Some(0),
        "chapter" => Some(1),
        "section" => Some(2),
        "subsection" => Some(3),
        "subsubsection" => Some(4),
        "paragraph" => Some(5),
        _ => None,
    }
}

/// Extract content inside balanced braces starting at byte position `pos` (the `{`).
/// Uses char iteration to handle UTF-8 correctly.
fn extract_braced(s: &str, pos: usize) -> Option<String> {
    let substr = &s[pos..];
    let mut chars = substr.chars();

    if chars.next() != Some('{') {
        return None;
    }

    let mut depth = 1i32;
    let mut result = String::new();

    for ch in chars {
        match ch {
            '{' => {
                depth += 1;
                result.push(ch);
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(result);
                }
                result.push(ch);
            }
            _ => result.push(ch),
        }
    }
    None
}

/// A line is a comment if the first non-whitespace character is `%`.
fn is_comment_line(line: &str) -> bool {
    line.trim_start().starts_with('%')
}

impl SemanticParserPlugin for LatexParserPlugin {
    fn id(&self) -> &str {
        "latex"
    }

    fn extensions(&self) -> &[&str] {
        &[".tex", ".latex", ".cls", ".sty"]
    }

    fn extract_entities(&self, content: &str, file_path: &str) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            return entities;
        }

        let section_re =
            Regex::new(r"\\(part|chapter|section|subsection|subsubsection|paragraph)\*?\{")
                .unwrap();
        let begin_env_re = Regex::new(r"\\begin\{(\w+)\}").unwrap();
        let end_env_re = Regex::new(r"\\end\{(\w+)\}").unwrap();
        let label_re = Regex::new(r"\\label\{([^}]+)\}").unwrap();
        let cmd_def_re =
            Regex::new(r"\\(newcommand|renewcommand|DeclareMathOperator)\*?\{?\\(\w+)").unwrap();

        // --- Locate \begin{document} and \end{document} ---
        let mut doc_start: Option<usize> = None;
        let mut doc_end: Option<usize> = None;
        for (i, &line) in lines.iter().enumerate() {
            if !is_comment_line(line) {
                if doc_start.is_none() && line.contains(r"\begin{document}") {
                    doc_start = Some(i);
                } else if doc_start.is_some()
                    && doc_end.is_none()
                    && line.contains(r"\end{document}")
                {
                    doc_end = Some(i);
                }
            }
        }

        let body_start = doc_start.map_or(0, |s| s + 1);
        let body_end = doc_end.unwrap_or(lines.len());

        // --- Preamble ---
        // For files with \begin{document}: everything before it is preamble.
        // For .sty/.cls without \begin{document}: entire file is preamble.
        let preamble_range: Option<(usize, usize)> = if let Some(ds) = doc_start {
            if ds > 0 {
                Some((0, ds))
            } else {
                None
            }
        } else if file_path.ends_with(".sty") || file_path.ends_with(".cls") {
            Some((0, lines.len()))
        } else {
            None
        };

        if let Some((p_start, p_end)) = preamble_range {
            let preamble_content = lines[p_start..p_end].join("\n").trim().to_string();
            if !preamble_content.is_empty() {
                let pid = build_entity_id(file_path, "preamble", "(preamble)", None);
                entities.push(SemanticEntity {
                    id: pid.clone(),
                    file_path: file_path.to_string(),
                    entity_type: "preamble".to_string(),
                    name: "(preamble)".to_string(),
                    parent_id: None,
                    content_hash: content_hash(&preamble_content),
                    structural_hash: None,
                    content: preamble_content,
                    start_line: p_start + 1,
                    end_line: p_end,
                    metadata: None,
                });

                // Extract command definitions from preamble
                let preamble_lines = &lines[p_start..p_end];
                let mut i = 0;
                while i < preamble_lines.len() {
                    let line = preamble_lines[i];
                    if !is_comment_line(line) {
                        if let Some(caps) = cmd_def_re.captures(line) {
                            let cmd_name = format!("\\{}", &caps[2]);
                            let def_start = i;
                            let mut def_end = i;

                            // Count braces to find multi-line definitions
                            let mut depth: i32 = 0;
                            for j in i..preamble_lines.len() {
                                for ch in preamble_lines[j].chars() {
                                    if ch == '{' {
                                        depth += 1;
                                    } else if ch == '}' {
                                        depth -= 1;
                                    }
                                }
                                def_end = j;
                                if depth <= 0 {
                                    break;
                                }
                            }

                            let def_content = preamble_lines[def_start..=def_end]
                                .join("\n")
                                .trim()
                                .to_string();
                            let cmd_id = build_entity_id(
                                file_path,
                                "command_definition",
                                &cmd_name,
                                Some(&pid),
                            );

                            entities.push(SemanticEntity {
                                id: cmd_id,
                                file_path: file_path.to_string(),
                                entity_type: "command_definition".to_string(),
                                name: cmd_name,
                                parent_id: Some(pid.clone()),
                                content_hash: content_hash(&def_content),
                                structural_hash: None,
                                content: def_content,
                                start_line: p_start + def_start + 1,
                                end_line: p_start + def_end + 1,
                                metadata: None,
                            });

                            i = def_end + 1;
                            continue;
                        }
                    }
                    i += 1;
                }
            }
        }

        // If entire file was treated as preamble (.sty/.cls), skip body parsing
        if preamble_range.map_or(false, |(_, end)| end == lines.len()) {
            return entities;
        }

        // --- Body: Pass 1 – Parse sections (like markdown headings) ---
        struct Section {
            level: usize,
            name: String,
            start_line: usize, // 1-based
            lines: Vec<String>,
            base_id: String,
            parent_index: Option<usize>,
        }

        let mut sections: Vec<Section> = Vec::new();
        let mut current_section: Option<usize> = None;
        let mut section_stack: Vec<(usize, usize)> = Vec::new(); // (level, section index)

        for i in body_start..body_end {
            let line = lines[i];
            let line_num = i + 1; // 1-based

            if is_comment_line(line) {
                if let Some(idx) = current_section {
                    sections[idx].lines.push(line.to_string());
                }
                continue;
            }

            if let Some(m) = section_re.find(line) {
                if let Some(caps) = section_re.captures(line) {
                    let cmd = &caps[1];
                    if let Some(level) = section_level(cmd) {
                        // Use brace-counting to extract title (handles nested braces)
                        let brace_pos = m.end() - 1; // byte offset of the `{`
                        let name =
                            extract_braced(line, brace_pos).unwrap_or_else(|| cmd.to_string());

                        // Pop stack until we find an ancestor with a strictly lower level
                        while section_stack.last().map_or(false, |(l, _)| *l >= level) {
                            section_stack.pop();
                        }
                        let parent_index = section_stack.last().map(|(_, idx)| *idx);

                        sections.push(Section {
                            level,
                            name: name.clone(),
                            start_line: line_num,
                            lines: vec![line.to_string()],
                            base_id: build_entity_id(file_path, "section", &name, None),
                            parent_index,
                        });
                        let section_index = sections.len() - 1;
                        current_section = Some(section_index);
                        section_stack.push((level, section_index));
                        continue;
                    }
                }
            }

            // Regular line: append to current section
            if let Some(idx) = current_section {
                sections[idx].lines.push(line.to_string());
            }
        }

        // Disambiguate section IDs
        let mut id_counts: HashMap<&str, usize> = HashMap::new();
        for section in &sections {
            *id_counts.entry(section.base_id.as_str()).or_default() += 1;
        }

        let section_ids: Vec<String> = sections
            .iter()
            .map(|section| {
                if id_counts[section.base_id.as_str()] > 1 {
                    build_entity_id_disambiguated(
                        file_path,
                        "section",
                        &section.name,
                        None,
                        section.start_line,
                    )
                } else {
                    section.base_id.clone()
                }
            })
            .collect();

        // Store section ranges for environment parent lookup
        let section_ranges: Vec<(usize, usize, usize)> = sections
            .iter()
            .map(|s| (s.start_line, s.start_line + s.lines.len() - 1, s.level))
            .collect();

        for (index, section) in sections.iter().enumerate() {
            let section_content = section.lines.join("\n").trim().to_string();
            if section_content.is_empty() {
                continue;
            }

            entities.push(SemanticEntity {
                id: section_ids[index].clone(),
                file_path: file_path.to_string(),
                entity_type: "section".to_string(),
                name: section.name.clone(),
                parent_id: section.parent_index.map(|pi| section_ids[pi].clone()),
                content_hash: content_hash(&section_content),
                structural_hash: None,
                content: section_content,
                start_line: section.start_line,
                end_line: section.start_line + section.lines.len() - 1,
                metadata: None,
            });
        }

        // --- Body: Pass 2 – Parse significant environments ---
        struct EnvInfo {
            env_type: String,
            name: String,
            start_line: usize, // 1-based
            end_line: usize,   // 1-based
            content: String,
            base_id: String,
        }

        let mut env_entities: Vec<EnvInfo> = Vec::new();
        // Stack: (env_type, start_line_1based, accumulated_lines)
        let mut env_stack: Vec<(String, usize, Vec<String>)> = Vec::new();

        for i in body_start..body_end {
            let line = lines[i];
            let line_num = i + 1;

            if is_comment_line(line) {
                if let Some((_, _, ref mut env_lines)) = env_stack.last_mut() {
                    env_lines.push(line.to_string());
                }
                continue;
            }

            // Check for \begin{env}
            if let Some(caps) = begin_env_re.captures(line) {
                let env_name = caps[1].to_string();
                if env_name != "document" && SIGNIFICANT_ENVIRONMENTS.contains(&env_name.as_str()) {
                    env_stack.push((env_name, line_num, vec![line.to_string()]));
                    continue;
                }
            }

            // Check for \end{env}
            if let Some(caps) = end_env_re.captures(line) {
                let env_name = caps[1].to_string();
                if let Some(pos) = env_stack.iter().rposition(|(name, _, _)| *name == env_name) {
                    let (env_type, start_line, mut env_lines) = env_stack.remove(pos);
                    env_lines.push(line.to_string());

                    // Try to find a \label inside the environment
                    let label = env_lines
                        .iter()
                        .find_map(|l| label_re.captures(l).map(|c| c[1].to_string()));

                    let name = label.unwrap_or_else(|| env_type.clone());
                    let env_content = env_lines.join("\n").trim().to_string();

                    env_entities.push(EnvInfo {
                        env_type,
                        name: name.clone(),
                        start_line,
                        end_line: line_num,
                        content: env_content,
                        base_id: build_entity_id(file_path, "environment", &name, None),
                    });
                    continue;
                }
            }

            // Accumulate lines inside open environments
            if let Some((_, _, ref mut env_lines)) = env_stack.last_mut() {
                env_lines.push(line.to_string());
            }
        }

        // Disambiguate environment IDs
        let mut env_id_counts: HashMap<&str, usize> = HashMap::new();
        for env in &env_entities {
            *env_id_counts.entry(env.base_id.as_str()).or_default() += 1;
        }

        let env_ids: Vec<String> = env_entities
            .iter()
            .map(|env| {
                if env_id_counts[env.base_id.as_str()] > 1 {
                    build_entity_id_disambiguated(
                        file_path,
                        "environment",
                        &env.name,
                        None,
                        env.start_line,
                    )
                } else {
                    env.base_id.clone()
                }
            })
            .collect();

        for (index, env) in env_entities.iter().enumerate() {
            // Find the deepest (highest-level number) section containing this environment
            let parent_id = find_parent_section_id(env.start_line, &section_ranges, &section_ids);

            let mut metadata = HashMap::new();
            metadata.insert("environment_type".to_string(), env.env_type.clone());

            entities.push(SemanticEntity {
                id: env_ids[index].clone(),
                file_path: file_path.to_string(),
                entity_type: "environment".to_string(),
                name: env.name.clone(),
                parent_id,
                content_hash: content_hash(&env.content),
                structural_hash: None,
                content: env.content.clone(),
                start_line: env.start_line,
                end_line: env.end_line,
                metadata: Some(metadata),
            });
        }

        entities
    }
}

/// Find the ID of the deepest section that contains the given line.
fn find_parent_section_id(
    line: usize,
    section_ranges: &[(usize, usize, usize)], // (start, end, level)
    section_ids: &[String],
) -> Option<String> {
    let mut best: Option<(usize, usize)> = None; // (index, level)
    for (i, &(start, end, level)) in section_ranges.iter().enumerate() {
        if start <= line && line <= end {
            if best.map_or(true, |(_, best_level)| level > best_level) {
                best = Some((i, level));
            }
        }
    }
    best.map(|(idx, _)| section_ids[idx].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(content: &str) -> Vec<SemanticEntity> {
        let plugin = LatexParserPlugin;
        plugin.extract_entities(content, "paper.tex")
    }

    #[test]
    fn basic_section_hierarchy() {
        let content = r"\begin{document}
\section{Introduction}
Some intro text.
\subsection{Background}
Background material.
\section{Methods}
Method details.
\end{document}
";
        let entities = extract(content);

        let sections: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "section")
            .collect();

        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].name, "Introduction");
        assert_eq!(sections[0].parent_id, None);
        assert_eq!(sections[1].name, "Background");
        assert_eq!(
            sections[1].parent_id.as_deref(),
            Some("paper.tex::section::Introduction")
        );
        assert_eq!(sections[2].name, "Methods");
        assert_eq!(sections[2].parent_id, None);
    }

    #[test]
    fn preamble_with_command_definitions() {
        let content = r"\documentclass{article}
\usepackage{amsmath}
\newcommand{\R}{\mathbb{R}}
\renewcommand{\vec}[1]{\mathbf{#1}}
\begin{document}
\section{Body}
Text.
\end{document}
";
        let entities = extract(content);

        let preamble: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "preamble")
            .collect();
        assert_eq!(preamble.len(), 1);
        assert!(preamble[0].content.contains(r"\documentclass{article}"));
        assert!(preamble[0].content.contains(r"\usepackage{amsmath}"));

        let cmds: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "command_definition")
            .collect();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, r"\R");
        assert_eq!(cmds[1].name, r"\vec");
        assert_eq!(
            cmds[0].parent_id.as_deref(),
            Some("paper.tex::preamble::(preamble)")
        );
    }

    #[test]
    fn environment_with_label() {
        let content = r"\begin{document}
\section{Results}
\begin{theorem}\label{thm:main}
Every even number greater than 2 is the sum of two primes.
\end{theorem}
\end{document}
";
        let entities = extract(content);

        let envs: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "environment")
            .collect();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].name, "thm:main");
        assert_eq!(
            envs[0].metadata.as_ref().unwrap().get("environment_type"),
            Some(&"theorem".to_string())
        );
        assert_eq!(
            envs[0].parent_id.as_deref(),
            Some("paper.tex::section::Results")
        );
    }

    #[test]
    fn environment_without_label() {
        let content = r"\begin{document}
\section{Proofs}
\begin{proof}
Trivial.
\end{proof}
\end{document}
";
        let entities = extract(content);

        let envs: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "environment")
            .collect();
        assert_eq!(envs.len(), 1);
        // Without a label, the name is the environment type
        assert_eq!(envs[0].name, "proof");
        assert_eq!(envs[0].id, "paper.tex::environment::proof");
    }

    #[test]
    fn starred_sections() {
        let content = r"\begin{document}
\section*{Acknowledgments}
Thanks to everyone.
\end{document}
";
        let entities = extract(content);

        let sections: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "section")
            .collect();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "Acknowledgments");
    }

    #[test]
    fn nested_braces_in_title() {
        let content = r"\begin{document}
\section{The $O(n^{2})$ Algorithm}
Details here.
\end{document}
";
        let entities = extract(content);

        let sections: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "section")
            .collect();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "The $O(n^{2})$ Algorithm");
    }

    #[test]
    fn comments_skipped_for_sections() {
        let content = r"\begin{document}
% \section{Commented Out}
\section{Real Section}
Content here.
\end{document}
";
        let entities = extract(content);

        let sections: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "section")
            .collect();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "Real Section");
    }

    #[test]
    fn empty_document_only_preamble() {
        let content = r"\documentclass{article}
\usepackage{amsmath}
\newcommand{\N}{\mathbb{N}}
\begin{document}
\end{document}
";
        let entities = extract(content);

        let preamble: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "preamble")
            .collect();
        assert_eq!(preamble.len(), 1);

        let cmds: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "command_definition")
            .collect();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, r"\N");

        let sections: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "section")
            .collect();
        assert_eq!(sections.len(), 0);
    }

    #[test]
    fn duplicate_section_names_disambiguated() {
        let content = r"\begin{document}
\section{Results}
First results.
\section{Results}
Second results.
\end{document}
";
        let entities = extract(content);

        let sections: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "section")
            .collect();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].id, "paper.tex::section::Results@L2");
        assert_eq!(sections[1].id, "paper.tex::section::Results@L4");
    }

    #[test]
    fn figure_environment() {
        let content = r"\begin{document}
\section{Experiments}
\begin{figure}
\centering
\includegraphics{plot.png}
\caption{Results}
\label{fig:results}
\end{figure}
\end{document}
";
        let entities = extract(content);

        let envs: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "environment")
            .collect();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].name, "fig:results");
        assert_eq!(
            envs[0].metadata.as_ref().unwrap().get("environment_type"),
            Some(&"figure".to_string())
        );
    }

    #[test]
    fn nonsignificant_environments_not_extracted() {
        let content = r"\begin{document}
\section{List}
\begin{itemize}
\item One
\item Two
\end{itemize}
\end{document}
";
        let entities = extract(content);

        let envs: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "environment")
            .collect();
        assert_eq!(envs.len(), 0);
    }

    #[test]
    fn sty_file_treated_as_preamble() {
        let content = r"\NeedsTeXFormat{LaTeX2e}
\ProvidesPackage{mypackage}
\newcommand{\foo}{bar}
";
        let plugin = LatexParserPlugin;
        let entities = plugin.extract_entities(content, "mypackage.sty");

        let preamble: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "preamble")
            .collect();
        assert_eq!(preamble.len(), 1);

        let cmds: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "command_definition")
            .collect();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, r"\foo");
    }

    #[test]
    fn multiline_command_definition() {
        let content = r"\newcommand{\mybox}[1]{%
  \fbox{%
    \parbox{0.9\textwidth}{#1}%
  }%
}
\begin{document}
\section{Body}
Text.
\end{document}
";
        let entities = extract(content);

        let cmds: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "command_definition")
            .collect();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, r"\mybox");
        assert!(cmds[0].content.contains(r"\parbox"));
        assert_eq!(cmds[0].start_line, 1);
        assert_eq!(cmds[0].end_line, 5);
    }

    #[test]
    fn multiple_environments_disambiguated() {
        let content = r"\begin{document}
\section{Theorems}
\begin{theorem}
First theorem.
\end{theorem}
\begin{theorem}
Second theorem.
\end{theorem}
\end{document}
";
        let entities = extract(content);

        let envs: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "environment")
            .collect();
        assert_eq!(envs.len(), 2);
        // Both have name "theorem" (no labels), so they get disambiguated
        assert_eq!(envs[0].id, "paper.tex::environment::theorem@L3");
        assert_eq!(envs[1].id, "paper.tex::environment::theorem@L6");
    }

    #[test]
    fn abstract_before_sections() {
        let content = r"\begin{document}
\begin{abstract}
This paper presents results.
\end{abstract}
\section{Introduction}
Intro text.
\end{document}
";
        let entities = extract(content);

        let envs: Vec<&SemanticEntity> = entities
            .iter()
            .filter(|e| e.entity_type == "environment")
            .collect();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].name, "abstract");
        // abstract is before any section, so no parent
        assert_eq!(envs[0].parent_id, None);
    }
}
