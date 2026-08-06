use api::{ContextResponse, DigestEntryDto, HitDto, MemoryDto, Origin};

use crate::args::SCOPE_EVERYWHERE;

pub fn hit_line(hit: &HitDto) -> String {
    memory_line(&hit.origin, &hit.memory)
}

pub fn memory_line(origin: &Origin, memory: &MemoryDto) -> String {
    format!(
        "[{}] ({}) {}",
        memory.id,
        provenance(origin, memory),
        memory.content
    )
}

/// `syn list` line — `memory_line` plus the importance tier column.
pub fn list_line(origin: &Origin, memory: &MemoryDto) -> String {
    format!("{} [{}]", memory_line(origin, memory), memory.importance)
}

fn provenance(origin: &Origin, memory: &MemoryDto) -> String {
    format!(
        "{}, {}",
        place(origin, &memory.scope),
        date(&memory.updated_at)
    )
}

pub fn store_label(origin: &Origin) -> String {
    place(origin, "workspace")
}

pub fn place(origin: &Origin, scope: &str) -> String {
    match origin {
        Origin::Preference => SCOPE_EVERYWHERE.to_string(),
        Origin::Workspace(workspace) if scope == "workspace" => workspace.clone(),
        Origin::Workspace(workspace) => format!("{workspace} · {scope}"),
    }
}

fn date(timestamp: &str) -> &str {
    timestamp.get(..10).unwrap_or(timestamp)
}

pub fn digest(context: &ContextResponse) -> Option<String> {
    let mut seen = Vec::new();
    let mut lines = Vec::new();
    let ordered = context
        .pinned
        .iter()
        .chain(&context.preferences)
        .chain(&context.recent_project);
    for entry in ordered {
        if seen.contains(&entry.memory.id) {
            continue;
        }
        seen.push(entry.memory.id.clone());
        lines.push(format!("- {}", digest_line(entry)));
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "## Memory (syn context)\n{}\n- (recall more with: syn recall \"<query>\")",
        lines.join("\n")
    ))
}

fn digest_line(entry: &DigestEntryDto) -> String {
    format!(
        "[{}] {}",
        entry.memory.id,
        single_line(&entry.memory.content)
    )
}

fn single_line(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(id: &str, scope: &str, content: &str) -> MemoryDto {
        MemoryDto {
            id: id.into(),
            content: content.into(),
            kind: "project".into(),
            scope: scope.into(),
            tags: vec![],
            pinned: false,
            importance: "medium".into(),
            created_at: "2026-06-02T09:00:00Z".into(),
            updated_at: "2026-07-14T11:22:33Z".into(),
        }
    }

    fn entry(id: &str, content: &str) -> DigestEntryDto {
        DigestEntryDto {
            origin: Origin::Workspace("work".into()),
            memory: memory(id, "workspace", content),
        }
    }

    #[test]
    fn hits_carry_workspace_scope_and_date() {
        let hit = HitDto {
            origin: Origin::Workspace("work".into()),
            score: 0.9,
            memory: memory("m_7f2a", "fresha/offers", "Staging deploys via ArgoCD."),
        };
        assert_eq!(
            hit_line(&hit),
            "[m_7f2a] (work · fresha/offers, 2026-07-14) Staging deploys via ArgoCD."
        );
    }

    #[test]
    fn workspace_scoped_hits_omit_the_redundant_scope_segment() {
        let hit = HitDto {
            origin: Origin::Workspace("work".into()),
            score: 0.5,
            memory: memory("m_31bc", "workspace", "Team uses trunk-based development."),
        };
        assert_eq!(
            hit_line(&hit),
            "[m_31bc] (work, 2026-07-14) Team uses trunk-based development."
        );
    }

    #[test]
    fn everywhere_hits_never_name_a_workspace() {
        let hit = HitDto {
            origin: Origin::Preference,
            score: 0.4,
            memory: memory("m_31bc", "workspace", "Prefers Datadog links."),
        };
        assert_eq!(
            hit_line(&hit),
            "[m_31bc] (everywhere, 2026-07-14) Prefers Datadog links."
        );
    }

    #[test]
    fn list_line_renders_the_importance_tier() {
        let mut fact = memory("m_9a1c", "workspace", "Runbook for deploy lanes.");
        fact.importance = "high".into();
        let line = list_line(&Origin::Workspace("work".into()), &fact);
        assert!(line.starts_with("[m_9a1c]"), "{line}");
        assert!(line.ends_with("[high]"), "{line}");
    }

    #[test]
    fn digest_dedups_and_collapses_multiline_content() {
        let context = ContextResponse {
            pinned: vec![entry("m_1", "pinned  fact\nsecond line")],
            preferences: vec![DigestEntryDto {
                origin: Origin::Preference,
                memory: memory("m_2", "workspace", "a preference"),
            }],
            recent_project: vec![entry("m_1", "pinned fact"), entry("m_3", "c")],
        };
        assert_eq!(
            digest(&context).unwrap(),
            "## Memory (syn context)\n\
             - [m_1] pinned fact second line\n\
             - [m_2] a preference\n\
             - [m_3] c\n\
             - (recall more with: syn recall \"<query>\")"
        );
    }

    #[test]
    fn an_empty_digest_prints_nothing() {
        let context = ContextResponse {
            pinned: vec![],
            preferences: vec![],
            recent_project: vec![],
        };
        assert_eq!(digest(&context), None);
    }
}
