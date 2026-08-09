// SPDX-License-Identifier: Apache-2.0

//! Shared project-memory helpers for CLI, HTTP, and Desktop surfaces.

use serde::{Deserialize, Serialize};

use crate::EncodingContext;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProjectMemoryDescriptor {
    pub name: String,
    pub id: String,
    pub root: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl ProjectMemoryDescriptor {
    pub fn normalized(mut self) -> Result<Self, String> {
        self.name = self.name.trim().to_string();
        self.id = self.id.trim().to_string();
        self.root = self.root.trim().to_string();
        self.tags = self
            .tags
            .into_iter()
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .collect();
        if self.name.is_empty() {
            return Err("project.name must not be empty".to_string());
        }
        if self.id.is_empty() {
            return Err("project.id must not be empty".to_string());
        }
        if self.root.is_empty() {
            return Err("project.root must not be empty".to_string());
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectPolicyClient {
    #[default]
    Generic,
    Codex,
    Claude,
    Cursor,
}

impl ProjectPolicyClient {
    pub fn label(self) -> &'static str {
        match self {
            Self::Generic => "Generic Coding Agent",
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Cursor => "Cursor",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
        }
    }
}

pub fn project_decision_content(project: &ProjectMemoryDescriptor, decision: &str) -> String {
    format!(
        "Project decision for {} (id: {}, root: {}): {}",
        project.name,
        project.id,
        project.root,
        decision.trim()
    )
}

pub fn project_decision_source_id(project_id: &str, now_ms: impl std::fmt::Display) -> String {
    format!("project:{project_id}:decision:{now_ms}")
}

pub fn project_decision_encoding_context(project: &ProjectMemoryDescriptor) -> EncodingContext {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "project_name".to_string(),
        serde_json::Value::String(project.name.clone()),
    );
    extra.insert(
        "project_id".to_string(),
        serde_json::Value::String(project.id.clone()),
    );
    extra.insert(
        "project_root".to_string(),
        serde_json::Value::String(project.root.clone()),
    );
    EncodingContext {
        task: Some("codebase_memory".to_string()),
        extra,
        ..EncodingContext::default()
    }
}

pub fn project_decision_scope_matches(
    source_id: Option<&str>,
    encoding_context_json: &str,
    content: &str,
    project_id: &str,
) -> bool {
    let mut saw_structured_project_scope = false;
    let source_id_prefix = format!("project:{project_id}:decision:");
    if let Some(source_id) = source_id {
        if source_id.starts_with(&source_id_prefix) {
            return true;
        }
        if source_id.starts_with("project:") && source_id.contains(":decision:") {
            saw_structured_project_scope = true;
        }
    }
    if let Some(candidate) = encoding_context_project_id(encoding_context_json) {
        if candidate == project_id {
            return true;
        }
        saw_structured_project_scope = true;
    }
    if saw_structured_project_scope {
        return false;
    }
    project_decision_content_matches(content, project_id)
}

fn encoding_context_project_id(encoding_context_json: &str) -> Option<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(encoding_context_json) else {
        return None;
    };
    value
        .pointer("/extra/project_id")
        .or_else(|| value.get("project_id"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn project_decision_content_matches(content: &str, project_id: &str) -> bool {
    if !content.starts_with("Project decision for ") {
        return false;
    }
    let id_with_root = format!("(id: {project_id},");
    let id_without_root = format!("(id: {project_id}):");
    content.contains(&id_with_root) || content.contains(&id_without_root)
}

pub fn render_project_policy(
    client: ProjectPolicyClient,
    project: &ProjectMemoryDescriptor,
) -> String {
    let client_name = client.label();
    let tags = if project.tags.is_empty() {
        "(none)".to_string()
    } else {
        project.tags.join(", ")
    };
    format!(
        r#"# Solo Project Memory Policy - {client_name}

Use Solo as durable project memory for this repository only.

Project name: {name}
Project id: {project_id}
Project root: {root}
Project tags: {tags}

Before coding:
- Retrieve context when prior project decisions, release constraints, architecture, debugging history, or user preferences may matter.
- Include the project name and project id in memory queries.
- Read the current workspace files before trusting memory about code behavior.

When writing memory:
- Store durable implementation decisions, root causes, release procedures, and project-specific constraints.
- Use project-scoped wording such as `Project decision for {name} (id: {project_id}): ...`.
- Prefer Solo Desktop's Project Decisions panel, `solo project decisions --add "..."`, or the MCP `memory_remember_batch` tool with the project id in each memory.

Safety:
- Do not store secrets, tokens, private keys, raw proprietary logs, or transient command output.
- Do not mix this project with another Solo profile or project id unless the user explicitly asks for cross-project context.
"#,
        name = project.name.as_str(),
        project_id = project.id.as_str(),
        root = project.root.as_str(),
        tags = tags,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> ProjectMemoryDescriptor {
        ProjectMemoryDescriptor {
            name: "Solo".to_string(),
            id: "solo".to_string(),
            root: "/work/solo".to_string(),
            tags: vec!["memory".to_string()],
        }
    }

    #[test]
    fn project_decision_scope_prefers_structured_metadata() {
        let content = "Project decision for Other (id: other): not this project";
        assert!(project_decision_scope_matches(
            Some("project:solo:decision:1"),
            "{}",
            content,
            "solo"
        ));
        assert!(!project_decision_scope_matches(
            Some("project:other:decision:1"),
            "{}",
            "Project decision for Solo (id: solo): legacy text",
            "solo"
        ));
        assert!(project_decision_scope_matches(
            None,
            r#"{"extra":{"project_id":"solo"}}"#,
            content,
            "solo"
        ));
    }

    #[test]
    fn project_decision_scope_accepts_legacy_content_when_unstructured() {
        assert!(project_decision_scope_matches(
            None,
            "{}",
            "Project decision for Solo (id: solo, root: /work/solo): Use HTTP.",
            "solo"
        ));
    }

    #[test]
    fn render_project_policy_names_client_and_scope() {
        let policy = render_project_policy(ProjectPolicyClient::Codex, &descriptor());
        assert!(policy.contains("Solo Project Memory Policy - Codex"));
        assert!(policy.contains("Project id: solo"));
        assert!(policy.contains("Project root: /work/solo"));
        assert!(policy.contains("Do not store secrets"));
    }
}
