// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Playbooks: how the assistant is taught to do a task the Temps way.
//!
//! A playbook is a markdown file in `crates/temps-ai-chat/harness/`, embedded
//! in the binary. Its frontmatter says what it is for and when it applies:
//!
//! ```text
//! ---
//! name: show-a-running-app
//! description: Show the user an app or server that is running in the sandbox
//! requires_tool: sandbox_preview     (optional: only offered with this tool)
//! contexts: assistant, project       (optional: every chat type when absent)
//! ---
//! # Markdown body…
//! ```
//!
//! The system prompt carries only a one-line index of the playbooks that apply
//! to the chat. The assistant reads one in full by calling `temps_playbook`
//! when the task matches, so guidance costs tokens only when it is used and
//! adding one is adding a file, not editing prompt text in Rust.

use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::json;
use temps_ai::ChatTool;
use thiserror::Error;
use tracing::error;

/// Tool the assistant calls to read a playbook in full.
pub const PLAYBOOK_TOOL: &str = "temps_playbook";

const SOURCES: &[(&str, &str)] = &[
    (
        "show-a-running-app.md",
        include_str!("../harness/show-a-running-app.md"),
    ),
    (
        "build-an-app.md",
        include_str!("../harness/build-an-app.md"),
    ),
    (
        "connect-a-service.md",
        include_str!("../harness/connect-a-service.md"),
    ),
    (
        "host-with-temps.md",
        include_str!("../harness/host-with-temps.md"),
    ),
];

#[derive(Error, Debug, PartialEq, Eq)]
pub enum PlaybookError {
    #[error("Playbook file {file} has no frontmatter: it must start with a `---` line")]
    MissingFrontmatter { file: String },

    #[error("Playbook file {file} has an unterminated frontmatter block")]
    UnterminatedFrontmatter { file: String },

    #[error("Playbook file {file} is missing the required `{key}` field")]
    MissingField { file: String, key: &'static str },

    #[error("Playbook file {file} has an unknown frontmatter line `{line}`")]
    UnknownField { file: String, line: String },

    #[error("Playbook file {file} has an empty body")]
    EmptyBody { file: String },
}

/// One parsed playbook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Playbook {
    pub name: String,
    pub description: String,
    /// The playbook is only offered when the chat has this tool.
    pub requires_tool: Option<String>,
    /// Chat types the playbook applies to; every type when empty.
    pub contexts: Vec<String>,
    pub body: String,
}

/// Parse one playbook file.
pub fn parse(file: &str, source: &str) -> Result<Playbook, PlaybookError> {
    let mut lines = source.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err(PlaybookError::MissingFrontmatter {
            file: file.to_string(),
        });
    }
    let mut name = None;
    let mut description = None;
    let mut requires_tool = None;
    let mut contexts = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let unknown = || PlaybookError::UnknownField {
            file: file.to_string(),
            line: line.to_string(),
        };
        let (key, value) = line.split_once(':').ok_or_else(unknown)?;
        let value = value.trim();
        match key.trim() {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            "requires_tool" => requires_tool = Some(value.to_string()),
            "contexts" => {
                contexts = value
                    .split(',')
                    .map(|context| context.trim().to_string())
                    .filter(|context| !context.is_empty())
                    .collect()
            }
            _ => return Err(unknown()),
        }
    }
    if !closed {
        return Err(PlaybookError::UnterminatedFrontmatter {
            file: file.to_string(),
        });
    }
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    if body.is_empty() {
        return Err(PlaybookError::EmptyBody {
            file: file.to_string(),
        });
    }
    let field = |value: Option<String>, key: &'static str| {
        value
            .filter(|value| !value.is_empty())
            .ok_or_else(|| PlaybookError::MissingField {
                file: file.to_string(),
                key,
            })
    };
    Ok(Playbook {
        name: field(name, "name")?,
        description: field(description, "description")?,
        requires_tool,
        contexts,
        body,
    })
}

/// Every embedded playbook. A file that does not parse is logged and left
/// out, so a bad playbook never takes a chat down; the tests fail instead.
pub fn all() -> &'static [Playbook] {
    static PARSED: OnceLock<Vec<Playbook>> = OnceLock::new();
    PARSED.get_or_init(|| {
        SOURCES
            .iter()
            .filter_map(|(file, source)| match parse(file, source) {
                Ok(playbook) => Some(playbook),
                Err(error) => {
                    error!("skipping an embedded playbook: {error}");
                    None
                }
            })
            .collect()
    })
}

/// The playbooks that apply to a chat of `context_type` holding `tool_names`.
pub fn applicable<'a>(context_type: &str, tool_names: &[&str]) -> Vec<&'a Playbook> {
    all()
        .iter()
        .filter(|playbook| {
            (playbook.contexts.is_empty()
                || playbook
                    .contexts
                    .iter()
                    .any(|context| context == context_type))
                && playbook
                    .requires_tool
                    .as_deref()
                    .map(|tool| tool_names.contains(&tool))
                    .unwrap_or(true)
        })
        .collect()
}

/// The system-prompt section naming the playbooks; `None` when none apply.
pub fn index_section(playbooks: &[&Playbook]) -> Option<String> {
    if playbooks.is_empty() {
        return None;
    }
    let mut section = format!(
        "## Playbooks\nWhen the task matches one of these, call `{PLAYBOOK_TOOL}` with its name \
         and follow it before you act:\n"
    );
    for playbook in playbooks {
        section.push_str(&format!(
            "- `{}`: {}\n",
            playbook.name, playbook.description
        ));
    }
    Some(section.trim_end().to_string())
}

pub fn tool() -> ChatTool {
    ChatTool {
        name: PLAYBOOK_TOOL.to_string(),
        description: "Read a playbook: step-by-step guidance for a kind of task, the Temps way. \
            The playbooks and what each is for are listed in the system prompt."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Playbook name." }
            },
            "required": ["name"]
        }),
    }
}

#[derive(Deserialize)]
struct PlaybookArgs {
    name: String,
}

/// The tool result for `temps_playbook`.
pub fn dispatch(arguments: &str) -> String {
    let args: PlaybookArgs = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(error) => {
            return format!("`{PLAYBOOK_TOOL}` needs {{\"name\": \"…\"}}: {error}.");
        }
    };
    match all().iter().find(|playbook| playbook.name == args.name) {
        Some(playbook) => playbook.body.clone(),
        None => {
            let names: Vec<&str> = all()
                .iter()
                .map(|playbook| playbook.name.as_str())
                .collect();
            format!(
                "There is no playbook named `{}`. Available: {}.",
                args.name,
                names.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox_link::{
        SANDBOX_CREATE_TOOL, SANDBOX_EXEC_TOOL, SANDBOX_OFFER_TOOL, SANDBOX_PREVIEW_TOOL,
        SANDBOX_READ_TOOL, SANDBOX_WRITE_TOOL,
    };

    /// Tools a playbook may name. A playbook that mentions another one has
    /// drifted from the code.
    const KNOWN_TOOLS: &[&str] = &[
        SANDBOX_EXEC_TOOL,
        SANDBOX_READ_TOOL,
        SANDBOX_WRITE_TOOL,
        SANDBOX_PREVIEW_TOOL,
        SANDBOX_CREATE_TOOL,
        SANDBOX_OFFER_TOOL,
        PLAYBOOK_TOOL,
        "temps_write",
        "temps",
    ];

    #[test]
    fn every_embedded_playbook_parses() {
        for (file, source) in SOURCES {
            let playbook = parse(file, source).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(format!("{}.md", playbook.name), *file, "file name matches");
        }
        assert_eq!(all().len(), SOURCES.len());
    }

    #[test]
    fn playbook_names_are_unique() {
        let mut names: Vec<&str> = all()
            .iter()
            .map(|playbook| playbook.name.as_str())
            .collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before);
    }

    #[test]
    fn playbooks_only_name_tools_that_exist() {
        for playbook in all() {
            if let Some(tool) = &playbook.requires_tool {
                assert!(
                    KNOWN_TOOLS.contains(&tool.as_str()),
                    "{}: {tool}",
                    playbook.name
                );
            }
            for token in playbook.body.split('`').skip(1).step_by(2) {
                let looks_like_a_tool = (token.starts_with("sandbox_")
                    || token.starts_with("temps_"))
                    && token.chars().all(|c| c.is_ascii_lowercase() || c == '_');
                if looks_like_a_tool {
                    assert!(
                        KNOWN_TOOLS.contains(&token),
                        "{} names `{token}`, which is not a tool",
                        playbook.name
                    );
                }
            }
        }
    }

    #[test]
    fn playbooks_that_point_at_each_other_point_at_real_ones() {
        let names: Vec<&str> = all()
            .iter()
            .map(|playbook| playbook.name.as_str())
            .collect();
        for playbook in all() {
            // "the `build-an-app` playbook" names a playbook; check it exists.
            for (before, _) in playbook.body.match_indices("` playbook") {
                let start = playbook.body[..before]
                    .rfind('`')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let named = &playbook.body[start..before];
                assert!(
                    names.contains(&named),
                    "{} points at a playbook `{named}` that does not exist",
                    playbook.name
                );
            }
        }
    }

    /// The variable names the connect playbook promises are the ones the
    /// service providers really hand to a deployment.
    #[test]
    fn connect_playbook_names_variables_the_providers_set() {
        let providers: &[(&str, &str)] = &[
            (
                include_str!("../../temps-providers/src/externalsvc/postgres.rs"),
                "POSTGRES_URL POSTGRES_HOST POSTGRES_PORT POSTGRES_DB POSTGRES_USER POSTGRES_PASSWORD",
            ),
            (
                include_str!("../../temps-providers/src/externalsvc/redis.rs"),
                "REDIS_URL REDIS_HOST REDIS_PORT REDIS_PASSWORD",
            ),
            (
                include_str!("../../temps-providers/src/externalsvc/mongodb.rs"),
                "MONGODB_HOST MONGODB_PORT MONGODB_DATABASE MONGODB_USERNAME MONGODB_PASSWORD",
            ),
            (
                include_str!("../../temps-providers/src/externalsvc/s3.rs"),
                "S3_ENDPOINT S3_BUCKET S3_ACCESS_KEY S3_SECRET_KEY S3_REGION",
            ),
        ];
        let playbook = all()
            .iter()
            .find(|playbook| playbook.name == "connect-a-service")
            .expect("connect-a-service is embedded");
        for (source, variables) in providers {
            for variable in variables.split_whitespace() {
                assert!(
                    playbook.body.contains(variable),
                    "the playbook does not mention {variable}"
                );
                assert!(
                    source.contains(&format!("\"{variable}\"")),
                    "{variable} is promised but no provider sets it"
                );
            }
        }
    }

    #[test]
    fn the_index_lists_only_what_applies() {
        let with_sandbox = applicable("assistant", &[SANDBOX_EXEC_TOOL, SANDBOX_PREVIEW_TOOL]);
        let names: Vec<&str> = with_sandbox.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"show-a-running-app"));
        assert!(names.contains(&"build-an-app"));
        assert!(names.contains(&"host-with-temps"));
        assert!(names.contains(&"connect-a-service"));

        let without = applicable("assistant", &[]);
        let names: Vec<&str> = without.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["connect-a-service", "host-with-temps"]);

        // A chat type a playbook does not list never sees it.
        assert!(applicable("deployment", &[SANDBOX_EXEC_TOOL, SANDBOX_PREVIEW_TOOL]).is_empty());
    }

    #[test]
    fn the_index_names_the_tool_and_every_playbook() {
        let applicable = applicable("project", &[SANDBOX_EXEC_TOOL, SANDBOX_PREVIEW_TOOL]);
        let section = index_section(&applicable).expect("an index");
        assert!(section.contains(PLAYBOOK_TOOL));
        for playbook in &applicable {
            assert!(section.contains(&format!("`{}`", playbook.name)));
        }
        assert_eq!(index_section(&[]), None);
    }

    #[test]
    fn dispatch_returns_the_body_and_lists_names_for_a_typo() {
        let body = dispatch(r#"{"name":"show-a-running-app"}"#);
        assert!(body.contains("sandbox_preview"));
        assert!(
            !body.starts_with("---"),
            "frontmatter is not shown to the model"
        );
        let missing = dispatch(r#"{"name":"show-me"}"#);
        assert!(missing.contains("no playbook named `show-me`"));
        assert!(missing.contains("build-an-app"));
        assert!(dispatch("{}").contains("needs"));
    }

    #[test]
    fn parse_rejects_each_malformed_shape() {
        let file = "x.md";
        assert_eq!(
            parse(file, "# no frontmatter"),
            Err(PlaybookError::MissingFrontmatter { file: file.into() })
        );
        assert_eq!(
            parse(file, "---\nname: a\ndescription: b\n"),
            Err(PlaybookError::UnterminatedFrontmatter { file: file.into() })
        );
        assert_eq!(
            parse(file, "---\nname: a\n---\nbody"),
            Err(PlaybookError::MissingField {
                file: file.into(),
                key: "description"
            })
        );
        assert_eq!(
            parse(file, "---\nname: a\ndescription: b\n---\n  \n"),
            Err(PlaybookError::EmptyBody { file: file.into() })
        );
        assert!(matches!(
            parse(file, "---\nname: a\ndescription: b\nwhen: now\n---\nbody"),
            Err(PlaybookError::UnknownField { .. })
        ));
        let ok = parse(
            file,
            "---\nname: a\ndescription: b\ncontexts: x, y\n---\nbody",
        )
        .unwrap();
        assert_eq!(ok.contexts, ["x", "y"]);
        assert_eq!(ok.requires_tool, None);
    }
}
