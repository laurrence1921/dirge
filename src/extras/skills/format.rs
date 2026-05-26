//! Skill format validation and frontmatter parsing.
//!
//! Skills are stored as directories under `.dirge/skills/` with a
//! `SKILL.md` file. The file starts with YAML frontmatter (between
//! `---` delimiters) followed by Markdown body content.

/// A parsed skill specification — the in-memory representation of
/// a `SKILL.md` file with its frontmatter metadata extracted.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillSpec {
    /// Skill name (lowercase, hyphens, max 64 chars). From
    /// frontmatter `name:` field or the directory name.
    pub name: String,
    /// Human-readable description from frontmatter `description:`.
    pub description: String,
    /// The full file content (frontmatter + body).
    pub content: String,
    /// Tags extracted from `tags:` in frontmatter dirge metadata.
    pub tags: Vec<String>,
    /// Related skill names from `related_skills:` in metadata.
    pub related: Vec<String>,
    /// The body content (everything after the closing `---`).
    pub body: String,
}

// ── Validation constants ───────────────────────────────

/// Maximum length of a skill name.
const MAX_NAME_LEN: usize = 64;

/// Maximum total content size (100K chars ≈ 36K tokens).
const MAX_CONTENT_LEN: usize = 100_000;

/// Characters allowed in skill names.
fn is_valid_name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
}

// ── Public API ─────────────────────────────────────────

/// Parse a `SKILL.md` file's content into a [`SkillSpec`]. Uses
/// `dir_name` as the fallback name when frontmatter omits it.
pub fn parse_skill_spec(content: &str, dir_name: &str) -> Option<SkillSpec> {
    let (frontmatter, body) = split_frontmatter(content)?;
    let body = body.trim().to_string();
    if body.is_empty() {
        return None;
    }

    let name = extract_field(&frontmatter, "name:")
        .map(|s| s.to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| dir_name.to_string());

    let description = extract_field(&frontmatter, "description:")
        .map(|s| s.to_string())
        .unwrap_or_default();

    let tags = extract_yaml_list(&frontmatter, "tags:");
    let related = extract_yaml_list(&frontmatter, "related_skills:");

    Some(SkillSpec {
        name,
        description,
        content: content.to_string(),
        tags,
        related,
        body,
    })
}

/// Validate a skill name. Returns `Ok(())` if the name is valid,
/// `Err(reason)` otherwise.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Skill name must not be empty".to_string());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!(
            "Skill name too long ({} chars, max {})",
            name.len(),
            MAX_NAME_LEN
        ));
    }
    if !name.chars().all(is_valid_name_char) {
        return Err(
            "Skill name must contain only lowercase letters, digits, and hyphens".to_string(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("Skill name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        return Err("Skill name must not contain consecutive hyphens".to_string());
    }
    Ok(())
}

/// Validate total content size. Returns error if over the limit.
pub fn validate_content_size(content: &str) -> Result<(), String> {
    if content.len() > MAX_CONTENT_LEN {
        return Err(format!(
            "Skill content too large ({} chars, max {})",
            content.len(),
            MAX_CONTENT_LEN
        ));
    }
    Ok(())
}

/// Build the frontmatter header for a skill.
#[cfg_attr(not(test), allow(dead_code))]
pub fn build_frontmatter(name: &str, description: &str, tags: &[String]) -> String {
    let mut fm = String::from("---\n");
    fm.push_str(&format!("name: {}\n", name));
    if !description.is_empty() {
        fm.push_str(&format!("description: {}\n", description));
    }
    if !tags.is_empty() {
        fm.push_str("metadata:\n");
        fm.push_str("  dirge:\n");
        fm.push_str("    tags: [");
        fm.push_str(
            &tags
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
        fm.push_str("]\n");
    }
    fm.push_str("---\n\n");
    fm
}

// ── Internal helpers ───────────────────────────────────

/// Split frontmatter from body. Returns `None` if there's no
/// frontmatter or it's malformed. Returns `(frontmatter_text, body_text)`.
fn split_frontmatter(content: &str) -> Option<(String, String)> {
    let content = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;

    let (fm, body) = if let Some(pos) = content.find("\n---") {
        let (a, b) = content.split_at(pos);
        (a.to_string(), b[4..].to_string())
    } else if let Some(pos) = content.find("\r\n---") {
        let (a, b) = content.split_at(pos);
        (a.to_string(), b[5..].to_string())
    } else {
        return None;
    };

    Some((fm, body))
}

/// Extract a scalar field value from frontmatter YAML.
fn extract_field<'a>(frontmatter: &'a str, key: &str) -> Option<&'a str> {
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix(key) {
            return Some(value.trim());
        }
    }
    None
}

/// Extract a YAML list field from frontmatter (e.g., `tags: [a, b]`).
fn extract_yaml_list(frontmatter: &str, key: &str) -> Vec<String> {
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix(key) {
            let value = value.trim();
            // Handle `[a, b, c]` format.
            if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
                return inner
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            // Handle single value without brackets.
            if !value.is_empty() {
                return vec![value.to_string()];
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_name ─────────────────────────────────

    #[test]
    fn valid_name_passes() {
        assert!(validate_name("project-build").is_ok());
        assert!(validate_name("rust-best-practices").is_ok());
        assert!(validate_name("a").is_ok());
    }

    #[test]
    fn empty_name_rejected() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn uppercase_name_rejected() {
        assert!(validate_name("Project-Build").is_err());
    }

    #[test]
    fn special_chars_rejected() {
        assert!(validate_name("project_build").is_err());
        assert!(validate_name("project build").is_err());
    }

    #[test]
    fn leading_trailing_hyphen_rejected() {
        assert!(validate_name("-project").is_err());
        assert!(validate_name("project-").is_err());
    }

    #[test]
    fn double_hyphen_rejected() {
        assert!(validate_name("project--build").is_err());
    }

    #[test]
    fn too_long_name_rejected() {
        let long = "a".repeat(65);
        assert!(validate_name(&long).is_err());
    }

    // ── parse_skill_spec ──────────────────────────────

    #[test]
    fn parse_valid_skill() {
        let content = "---\nname: project-build\ndescription: Build commands\n---\n\nRun `cargo build` to compile.\n";
        let spec = parse_skill_spec(content, "fallback").unwrap();
        assert_eq!(spec.name, "project-build");
        assert_eq!(spec.description, "Build commands");
        assert!(spec.body.contains("cargo build"));
    }

    #[test]
    fn parse_falls_back_to_dir_name() {
        let content = "---\ndescription: no name field\n---\n\nbody here\n";
        let spec = parse_skill_spec(content, "dir-name").unwrap();
        assert_eq!(spec.name, "dir-name");
    }

    #[test]
    fn parse_rejects_empty_body() {
        let content = "---\nname: test\n---\n   \n";
        assert!(parse_skill_spec(content, "dir").is_none());
    }

    #[test]
    fn parse_no_frontmatter_returns_none() {
        assert!(parse_skill_spec("just body", "dir").is_none());
    }

    #[test]
    fn parse_extracts_tags() {
        let content =
            "---\nname: s\nmetadata:\n  dirge:\n    tags: [build, rust, cargo]\n---\n\nbody\n";
        let spec = parse_skill_spec(content, "s").unwrap();
        assert_eq!(spec.tags, vec!["build", "rust", "cargo"]);
    }

    #[test]
    fn frontmatter_with_empty_name_defaults_to_dir() {
        let content = "---\nname:\ndescription: desc\n---\n\nbody\n";
        let spec = parse_skill_spec(content, "dir-name").unwrap();
        assert_eq!(spec.name, "dir-name");
    }

    // ── validate_content_size ─────────────────────────

    #[test]
    fn content_size_under_limit() {
        assert!(validate_content_size("short").is_ok());
    }

    #[test]
    fn content_size_over_limit() {
        let big = "x".repeat(100_001);
        assert!(validate_content_size(&big).is_err());
    }

    // ── build_frontmatter ─────────────────────────────

    #[test]
    fn build_frontmatter_includes_name_and_description() {
        let fm = build_frontmatter("my-skill", "Does things", &[]);
        assert!(fm.contains("name: my-skill"));
        assert!(fm.contains("description: Does things"));
        assert!(fm.starts_with("---\n"));
        assert!(fm.ends_with("---\n\n"));
    }

    #[test]
    fn build_frontmatter_includes_tags() {
        let fm = build_frontmatter("s", "", &["rust".into(), "build".into()]);
        assert!(fm.contains("tags: [rust, build]"));
    }

    // ── extract_yaml_list ─────────────────────────────

    #[test]
    fn extract_empty_list_for_missing_key() {
        let list = extract_yaml_list("no tags here", "tags:");
        assert!(list.is_empty());
    }

    #[test]
    fn extract_single_tag_without_brackets() {
        let list = extract_yaml_list("tags: rust", "tags:");
        assert_eq!(list, vec!["rust"]);
    }
}
