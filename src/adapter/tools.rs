//! The neutral tool vocabulary and its per-engine permission mappings (#1371).
//!
//! `capabilities.permissions[].tool` names a tool in a single neutral
//! vocabulary — Claude Code's PascalCase tool names — which each adapter then
//! renders in its own engine's grammar. Claude Code uses the neutral name
//! verbatim (see `claude_code::normalize_deprecated_tool` for the one rewrite it
//! applies), so only the engines with a *closed* key set need translation:
//!
//! - **opencode** decodes `permission` against a fixed schema and discards the
//!   entire config file if any one key fails, so an untranslated PascalCase name
//!   can't be passed through.
//! - **crush** matches `permissions.allowed_tools` by exact string equality
//!   against its own lowercase tool ids, so an untranslated name silently
//!   matches nothing.
//!
//! Both used to carry their own hand-written `match` arms, which drifted: crush
//! treated `MultiEdit` as its own tool while opencode folded it into `edit`, and
//! neither recorded whether an absent entry meant "this engine has no analog" or
//! "nobody got around to it". [`NEUTRAL_TOOLS`] is the single table both read
//! from, and [`ToolMapping`] makes that distinction explicit so the docs can
//! state it (`website/docs/engines.md`).

/// How one engine expresses a neutral tool in its permission config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolMapping {
    /// The engine's own identifier for the same tool.
    Renamed(&'static str),
    /// No exact analog. `key` is the closest the engine has and `note` says how
    /// it differs — reproduced verbatim in the docs table, so it has to read as
    /// an explanation to a user rather than a code comment.
    Closest {
        key: &'static str,
        note: &'static str,
    },
    /// No analog at all. A neutral permission rule naming this tool is dropped
    /// for this engine; `note` says why.
    Unsupported { note: &'static str },
}

impl ToolMapping {
    /// The engine's permission key, or `None` when the engine has no analog and
    /// the rule must be dropped.
    fn key(self) -> Option<&'static str> {
        match self {
            Self::Renamed(key) | Self::Closest { key, .. } => Some(key),
            Self::Unsupported { .. } => None,
        }
    }

    /// The user-facing explanation for a mapping that isn't one-to-one.
    pub(crate) fn note(self) -> Option<&'static str> {
        match self {
            Self::Renamed(_) => None,
            Self::Closest { note, .. } | Self::Unsupported { note } => Some(note),
        }
    }
}

/// One neutral tool and where it lands on each engine that needs translation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NeutralTool {
    /// The name written in `capabilities.permissions[].tool`.
    name: &'static str,
    pub(crate) opencode: ToolMapping,
    pub(crate) crush: ToolMapping,
}

/// Note text for a tool absent from the crush tool set llmenv's mapping was
/// source-verified against.
///
/// Deliberately phrased as what llmenv knows rather than as a claim about what
/// crush will never have: the mappings below are source-verified (#1306, #1321)
/// against `allToolNames()` in crush's `internal/config/config.go`, and an entry
/// is only promoted out of `Unsupported` by verifying the same way — not by
/// assuming a lowercase pass-through works.
const CRUSH_ABSENT: &str = "not present in the crush tool set this mapping was verified against, so rules naming it \
     are dropped for crush. Use `native_permissions.crush` if crush gains an equivalent.";

/// Note text for a tool absent from the opencode permission key set llmenv's
/// mapping was source-verified against (`packages/core/src/v1/config/permission.ts`,
/// #1326) — same reasoning as [`CRUSH_ABSENT`].
const OPENCODE_ABSENT: &str = "not present in the opencode permission key set this mapping was verified against, so \
     rules naming it are dropped for opencode. Rendering an unverified key is not an option: \
     opencode discards its entire config file when one key fails schema decode. Use \
     `native_permissions.opencode` if opencode gains an equivalent.";

/// crush's `edit`/`multiedit` are broader than Claude Code's: they create a
/// missing file and its parent directories on an empty `old_string`
/// (`internal/agent/tools/edit.go`'s `createNewFile`), where Claude Code's
/// `Edit` errors on a nonexistent path and requires `Write` to create one.
const CRUSH_EDIT_CREATES: &str = "crush's edit tools also create missing files and parent directories, which the neutral \
     name alone doesn't imply — allowing this on crush also allows file creation.";

/// The canonical neutral tool vocabulary.
///
/// A name absent from this table is not rejected: Claude Code gets new tools
/// llmenv has no reason to know about, and its adapter passes an unrecognized
/// name straight through, so a rule naming one still works there. It *is*
/// reported once by `cli::warn_dead_config`, because opencode and crush can only
/// drop it.
const NEUTRAL_TOOLS: &[NeutralTool] = &[
    NeutralTool {
        name: "Bash",
        opencode: ToolMapping::Renamed("bash"),
        crush: ToolMapping::Renamed("bash"),
    },
    NeutralTool {
        name: "Read",
        opencode: ToolMapping::Renamed("read"),
        crush: ToolMapping::Renamed("view"),
    },
    NeutralTool {
        name: "Edit",
        opencode: ToolMapping::Renamed("edit"),
        crush: ToolMapping::Closest {
            key: "edit",
            note: CRUSH_EDIT_CREATES,
        },
    },
    NeutralTool {
        name: "Write",
        // opencode has no `write` key separate from `edit`: one key gates every
        // file mutation, so a Write rule reaches opencode as an edit rule.
        opencode: ToolMapping::Closest {
            key: "edit",
            note: "opencode gates every file mutation through the single `edit` key, so an \
                   allow/ask/deny of `Write` also covers `Edit`.",
        },
        crush: ToolMapping::Renamed("write"),
    },
    NeutralTool {
        name: "MultiEdit",
        opencode: ToolMapping::Closest {
            key: "edit",
            note: "opencode has no separate multi-edit key; the rule covers `edit` as a whole.",
        },
        crush: ToolMapping::Closest {
            key: "multiedit",
            note: CRUSH_EDIT_CREATES,
        },
    },
    NeutralTool {
        name: "Glob",
        opencode: ToolMapping::Renamed("glob"),
        crush: ToolMapping::Renamed("glob"),
    },
    NeutralTool {
        name: "Grep",
        opencode: ToolMapping::Renamed("grep"),
        crush: ToolMapping::Renamed("grep"),
    },
    NeutralTool {
        name: "LS",
        opencode: ToolMapping::Renamed("list"),
        crush: ToolMapping::Renamed("ls"),
    },
    NeutralTool {
        name: "WebFetch",
        opencode: ToolMapping::Renamed("webfetch"),
        // crush also has a more specialized `agentic_fetch`; the base `fetch`
        // tool is the direct equivalent (#1306).
        crush: ToolMapping::Renamed("fetch"),
    },
    NeutralTool {
        name: "WebSearch",
        opencode: ToolMapping::Renamed("websearch"),
        crush: ToolMapping::Unsupported {
            note: "crush's `web_search` is a more specialized tool than Claude Code's \
                   `WebSearch` and was judged not a direct equivalent when this mapping was \
                   source-verified, so rules naming `WebSearch` are dropped for crush. Target \
                   `web_search` explicitly via `native_permissions.crush` if that's what you \
                   want.",
        },
    },
    NeutralTool {
        name: "TodoWrite",
        opencode: ToolMapping::Renamed("todowrite"),
        crush: ToolMapping::Renamed("todos"),
    },
    NeutralTool {
        name: "Task",
        opencode: ToolMapping::Renamed("task"),
        crush: ToolMapping::Unsupported { note: CRUSH_ABSENT },
    },
    NeutralTool {
        // In the vocabulary specifically so it reads as "no engine analog"
        // rather than "unrecognized name" — it's a real Claude Code tool, and
        // both adapters already cited it as the example of one neither can
        // express (#1326, #1321).
        name: "NotebookEdit",
        opencode: ToolMapping::Unsupported {
            note: OPENCODE_ABSENT,
        },
        crush: ToolMapping::Unsupported { note: CRUSH_ABSENT },
    },
    NeutralTool {
        name: "Skill",
        opencode: ToolMapping::Renamed("skill"),
        crush: ToolMapping::Unsupported { note: CRUSH_ABSENT },
    },
];

/// The table entry for `neutral`, or `None` when the name isn't in the
/// vocabulary at all.
pub(crate) fn lookup(neutral: &str) -> Option<&'static NeutralTool> {
    NEUTRAL_TOOLS.iter().find(|t| t.name == neutral)
}

/// Whether `neutral` is a name llmenv knows how to translate.
pub(crate) fn is_known(neutral: &str) -> bool {
    lookup(neutral).is_some()
}

/// Every neutral tool name, for the "valid names are ..." half of a diagnostic.
pub(crate) fn known_names() -> Vec<&'static str> {
    NEUTRAL_TOOLS.iter().map(|t| t.name).collect()
}

/// opencode's permission key for `neutral`, or `None` when opencode has no
/// analog (or the name isn't in the vocabulary).
pub(crate) fn opencode_key(neutral: &str) -> Option<&'static str> {
    lookup(neutral).and_then(|t| t.opencode.key())
}

/// crush's tool identifier for `neutral`, or `None` when crush has no analog
/// (or the name isn't in the vocabulary).
pub(crate) fn crush_key(neutral: &str) -> Option<&'static str> {
    lookup(neutral).and_then(|t| t.crush.key())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn every_name_is_unique() {
        let mut names = known_names();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "a duplicate name would shadow an entry");
    }

    #[test]
    fn unknown_name_maps_nowhere() {
        // The reported case: `Create` is not a tool in the vocabulary, so it has
        // no key on either engine and `is_known` says so.
        assert!(!is_known("Create"));
        assert_eq!(opencode_key("Create"), None);
        assert_eq!(crush_key("Create"), None);
    }

    #[test]
    fn mapping_notes_exist_exactly_where_the_mapping_is_not_one_to_one() {
        for tool in NEUTRAL_TOOLS {
            for (engine, mapping) in [("opencode", tool.opencode), ("crush", tool.crush)] {
                match mapping {
                    ToolMapping::Renamed(_) => assert!(
                        mapping.note().is_none(),
                        "{} on {engine}: a one-to-one rename needs no note",
                        tool.name
                    ),
                    ToolMapping::Closest { .. } | ToolMapping::Unsupported { .. } => {
                        let note = mapping.note().unwrap_or_default();
                        assert!(
                            !note.trim().is_empty(),
                            "{} on {engine}: a non-exact mapping must explain itself — \
                             that note is what the docs table reproduces",
                            tool.name
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn unsupported_tools_have_no_key() {
        // The invariant the renderers rely on: `Unsupported` means "drop the
        // rule", so it must never hand back a key to render.
        for tool in NEUTRAL_TOOLS {
            if matches!(tool.crush, ToolMapping::Unsupported { .. }) {
                assert_eq!(crush_key(tool.name), None, "{}", tool.name);
            }
            if matches!(tool.opencode, ToolMapping::Unsupported { .. }) {
                assert_eq!(opencode_key(tool.name), None, "{}", tool.name);
            }
        }
    }

    /// Guards the drift that made this table necessary: a tool added to the
    /// vocabulary without a row in the engines doc leaves users with no way to
    /// find out how it maps.
    #[test]
    fn every_tool_is_documented_in_engines_md() {
        let doc = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/website/docs/engines.md"
        ))
        .unwrap();
        for tool in NEUTRAL_TOOLS {
            assert!(
                doc.contains(&format!("`{}`", tool.name)),
                "neutral tool `{}` is missing from website/docs/engines.md — add it to the \
                 tool mapping table",
                tool.name
            );
        }
    }
}
