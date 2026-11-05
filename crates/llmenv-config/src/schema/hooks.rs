//! The hook entries of a config or a bundle.

use serde::{Deserialize, Serialize};

/// A hook registration. `command` paths in `handler` are bundle-relative when
/// declared in a `bundle.yaml`, resolved at materialize time.
#[derive(Debug, Clone, Deserialize, Serialize, Eq)]
pub struct Hook {
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub handler: HookHandler,
    #[serde(skip)]
    pub bundle_origin: Option<std::path::PathBuf>,
}

impl PartialEq for Hook {
    fn eq(&self, other: &Self) -> bool {
        self.event == other.event && self.matcher == other.matcher && self.handler == other.handler
    }
}

/// A hook handler. `type` selects the mechanism; `command` is set for
/// `command`-type handlers, `tool` for `mcp_tool`-type.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HookHandler {
    #[serde(rename = "type")]
    pub kind: HookHandlerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// What the engine does when the hook cannot start, times out, or exits with an unexpected
    /// code. Unset means the engine default, which lets the action through (#3101).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<HookOnFailure>,
}

/// The engine action when a hook fails to run. Only Claude Code 2.1.295 and later read it (#3101).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookOnFailure {
    /// Block the action, so a guard that cannot run fails closed.
    Block,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookHandlerKind {
    Command,
    McpTool,
}

/// Drop repeated hooks and keep the first, in order. Two hooks that differ only by `on_failure`
/// are one guard, so the stricter setting wins: when any copy blocks, the kept hook blocks (#3101).
pub fn dedup_hooks(hooks: &mut Vec<Hook>) {
    let mut kept: Vec<Hook> = Vec::with_capacity(hooks.len());
    for hook in hooks.drain(..) {
        match kept.iter_mut().find(|k| k.same_guard(&hook)) {
            Some(k) => k.handler.on_failure = k.handler.on_failure.or(hook.handler.on_failure),
            None => kept.push(hook),
        }
    }
    *hooks = kept;
}

impl Hook {
    /// Whether `other` runs the same handler on the same event and matcher, whatever its
    /// `on_failure` is.
    fn same_guard(&self, other: &Self) -> bool {
        self.event == other.event
            && self.matcher == other.matcher
            && self.handler.kind == other.handler.kind
            && self.handler.command == other.handler.command
            && self.handler.tool == other.handler.tool
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn hook(command: &str, on_failure: Option<HookOnFailure>) -> Hook {
        Hook {
            event: "PreToolUse".into(),
            matcher: None,
            handler: HookHandler {
                kind: HookHandlerKind::Command,
                command: Some(command.into()),
                tool: None,
                on_failure,
            },
            bundle_origin: None,
        }
    }

    #[test]
    fn a_guard_set_twice_is_kept_once_and_blocks_when_any_copy_blocks() {
        let mut hooks = vec![
            hook("a.sh", None),
            hook("b.sh", None),
            hook("a.sh", Some(HookOnFailure::Block)),
            hook("a.sh", None),
        ];
        dedup_hooks(&mut hooks);
        assert_eq!(
            hooks,
            vec![hook("a.sh", Some(HookOnFailure::Block)), hook("b.sh", None)]
        );
    }

    proptest! {
        /// After the dedup each guard appears once, in first-seen order, and it blocks exactly
        /// when one of its copies blocked.
        #[test]
        fn each_guard_is_kept_once_with_the_strictest_setting(
            items in proptest::collection::vec((0usize..4, any::<bool>()), 0..12),
        ) {
            let mut hooks: Vec<Hook> = items
                .iter()
                .map(|(i, block)| hook(&format!("g{i}.sh"), block.then_some(HookOnFailure::Block)))
                .collect();
            dedup_hooks(&mut hooks);
            let mut order: Vec<usize> = Vec::new();
            for (i, _) in &items {
                if !order.contains(i) {
                    order.push(*i);
                }
            }
            prop_assert_eq!(hooks.len(), order.len());
            for (h, i) in hooks.iter().zip(&order) {
                let want = format!("g{i}.sh");
                prop_assert_eq!(h.handler.command.as_deref(), Some(want.as_str()));
                let any_block = items.iter().any(|(j, b)| j == i && *b);
                prop_assert_eq!(h.handler.on_failure.is_some(), any_block);
            }
        }

        /// A hook with any `on_failure` survives a YAML round trip.
        #[test]
        fn a_hook_round_trips_through_yaml(
            command in "[a-z]{1,8}\\.sh",
            matcher in proptest::option::of("[A-Za-z|]{1,8}"),
            block in any::<bool>(),
        ) {
            let mut original = hook(&command, block.then_some(HookOnFailure::Block));
            original.matcher = matcher;
            let text = serde_yaml::to_string(&original).unwrap();
            let back: Hook = serde_yaml::from_str(&text).unwrap();
            prop_assert_eq!(back, original);
        }
    }
}
