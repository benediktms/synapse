use api::{ContextResponse, DigestEntryDto, HitDto, MemoryDto, Origin};

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

fn provenance(origin: &Origin, memory: &MemoryDto) -> String {
    let date = date(&memory.updated_at);
    match origin {
        Origin::Preference => format!("preference, {date}"),
        Origin::Workspace(workspace) if memory.scope == "workspace" => {
            format!("{workspace}, {date}")
        }
        Origin::Workspace(workspace) => format!("{workspace} · {}, {date}", memory.scope),
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
        .chain(&context.shared_user)
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
    fn preference_hits_never_name_a_workspace() {
        let hit = HitDto {
            origin: Origin::Preference,
            score: 0.4,
            memory: memory("m_31bc", "workspace", "Prefers Datadog links."),
        };
        assert_eq!(
            hit_line(&hit),
            "[m_31bc] (preference, 2026-07-14) Prefers Datadog links."
        );
    }

    #[test]
    fn digest_dedups_and_collapses_multiline_content() {
        let context = ContextResponse {
            pinned: vec![entry("m_1", "pinned  fact\nsecond line")],
            shared_user: vec![DigestEntryDto {
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
            shared_user: vec![],
            recent_project: vec![],
        };
        assert_eq!(digest(&context), None);
    }
}
