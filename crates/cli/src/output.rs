use api::{ContextResponse, DigestEntryDto, HitDto, MemoryDto, Origin};

use crate::args::{Detail, SCOPE_EVERYWHERE};

pub fn hit_line(hit: &HitDto, detail: Detail) -> String {
    if hit.neighbors.is_empty() {
        return memory_line(&hit.origin, &hit.memory, detail);
    }
    let mut links = hit
        .neighbors
        .iter()
        .map(|n| format!("{} {}", n.phrase, n.id))
        .collect::<Vec<_>>();
    if hit.neighbors_truncated {
        links.push("more via syn links".to_string());
    }
    format!(
        "{} ({})",
        memory_line(&hit.origin, &hit.memory, detail),
        links.join(", ")
    )
}

pub fn memory_line(origin: &Origin, memory: &MemoryDto, detail: Detail) -> String {
    format!(
        "[{}] ({}) {}",
        memory.id,
        provenance(origin, memory),
        body(memory, detail)
    )
}

/// `syn list` line — `memory_line` plus the importance tier column.
pub fn list_line(origin: &Origin, memory: &MemoryDto, detail: Detail) -> String {
    format!(
        "{} [{}]",
        memory_line(origin, memory, detail),
        memory.importance
    )
}

fn body(memory: &MemoryDto, detail: Detail) -> String {
    match detail {
        Detail::Short => short(memory),
        Detail::Full => memory.content.clone(),
    }
}

fn short(memory: &MemoryDto) -> String {
    domain::short_form(&memory.title, &memory.content)
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

/// The digest is the tightest surface there is, so it only ever prints the short form.
fn digest_line(entry: &DigestEntryDto) -> String {
    format!("[{}] {}", entry.memory.id, short(&entry.memory))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(id: &str, scope: &str, content: &str) -> MemoryDto {
        MemoryDto {
            id: id.into(),
            content: content.into(),
            title: String::new(),
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

    fn neighbor(id: &str, phrase: &str, scope: &str) -> api::NeighborDto {
        api::NeighborDto {
            id: id.into(),
            phrase: phrase.into(),
            scope: scope.into(),
        }
    }

    #[test]
    fn hit_line_appends_bracketed_neighbors() {
        let hit = HitDto {
            origin: Origin::Workspace("work".into()),
            score: 0.9,
            memory: memory("m_7f2a", "workspace", "New deploy process."),
            neighbors: vec![
                neighbor("m_31bc", "supersedes", "workspace"),
                neighbor("m_00B1", "is superseded by", "work · fresha/offers"),
            ],
            neighbors_truncated: false,
        };
        assert_eq!(
            hit_line(&hit, Detail::Full),
            "[m_7f2a] (work, 2026-07-14) New deploy process. \
             (supersedes m_31bc, is superseded by m_00B1)"
        );
    }

    #[test]
    fn hits_carry_workspace_scope_and_date() {
        let hit = HitDto {
            origin: Origin::Workspace("work".into()),
            score: 0.9,
            memory: memory("m_7f2a", "fresha/offers", "Staging deploys via ArgoCD."),
            neighbors: vec![],
            neighbors_truncated: false,
        };
        assert_eq!(
            hit_line(&hit, Detail::Full),
            "[m_7f2a] (work · fresha/offers, 2026-07-14) Staging deploys via ArgoCD."
        );
    }

    #[test]
    fn workspace_scoped_hits_omit_the_redundant_scope_segment() {
        let hit = HitDto {
            origin: Origin::Workspace("work".into()),
            score: 0.5,
            memory: memory("m_31bc", "workspace", "Team uses trunk-based development."),
            neighbors: vec![],
            neighbors_truncated: false,
        };
        assert_eq!(
            hit_line(&hit, Detail::Full),
            "[m_31bc] (work, 2026-07-14) Team uses trunk-based development."
        );
    }

    #[test]
    fn everywhere_hits_never_name_a_workspace() {
        let hit = HitDto {
            origin: Origin::Preference,
            score: 0.4,
            memory: memory("m_31bc", "workspace", "Prefers Datadog links."),
            neighbors: vec![],
            neighbors_truncated: false,
        };
        assert_eq!(
            hit_line(&hit, Detail::Full),
            "[m_31bc] (everywhere, 2026-07-14) Prefers Datadog links."
        );
    }

    #[test]
    fn list_line_renders_the_importance_tier() {
        let mut fact = memory("m_9a1c", "workspace", "Runbook for deploy lanes.");
        fact.importance = "high".into();
        let line = list_line(&Origin::Workspace("work".into()), &fact, Detail::Full);
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
    fn the_digest_prefers_a_title_and_falls_back_to_the_first_sentence() {
        let mut titled = memory("m_1", "workspace", "A long fact with plenty of detail.");
        titled.title = "Deploys use ArgoCD".into();
        let context = ContextResponse {
            pinned: vec![DigestEntryDto {
                origin: Origin::Workspace("work".into()),
                memory: titled,
            }],
            preferences: vec![],
            recent_project: vec![entry(
                "m_2",
                "The queue drains on flush. Everything after this is dropped from the digest.",
            )],
        };
        assert_eq!(
            digest(&context).unwrap(),
            "## Memory (syn context)\n\
             - [m_1] Deploys use ArgoCD\n\
             - [m_2] The queue drains on flush…\n\
             - (recall more with: syn recall \"<query>\")"
        );
    }

    #[test]
    fn short_detail_replaces_the_body_on_a_hit_line() {
        let mut fact = memory(
            "m_7f2a",
            "workspace",
            "Deploys go through ArgoCD. The rest is detail.",
        );
        fact.title = "ArgoCD owns deploys".into();
        let hit = HitDto {
            origin: Origin::Workspace("work".into()),
            score: 0.9,
            memory: fact,
            neighbors: vec![],
            neighbors_truncated: false,
        };
        assert_eq!(
            hit_line(&hit, Detail::Short),
            "[m_7f2a] (work, 2026-07-14) ArgoCD owns deploys"
        );
        assert_eq!(
            hit_line(&hit, Detail::Full),
            "[m_7f2a] (work, 2026-07-14) Deploys go through ArgoCD. The rest is detail."
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
