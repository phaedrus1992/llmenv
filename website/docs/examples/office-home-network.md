# Office vs. Home Network

Use different MCP servers depending on which network you're on, with no manual toggling.

## Config

```yaml
# ~/.config/llmenv/config.yaml

scope:
  network:
    - id: office
      match: { gateway_mac: "aa:bb:cc:dd:ee:ff" }   # your office router's MAC
      tags: [office]
    - id: home
      match: { gateway_mac: "11:22:33:44:55:66" }   # your home router's MAC
      tags: [home]

mcp:
  - name: internal-docs
    when: [office]
    type: stdio              # default; shown for clarity
    command: npx
    args: ["-y", "my-company-docs-mcp"]

  - name: home-assistant
    when: [home]
    type: stdio
    command: uvx
    args: ["home-assistant-mcp"]
```

## How it works

1. On every shell prompt, llmenv runs `llmenv export`.
2. It detects the current gateway MAC and matches it to a `network` scope.
3. The matching scope adds `office` or `home` to the active tag set.
4. The MCP server whose tags intersect the active set is included; the other is not.
5. The adapter upserts the selected server into `mcpServers` in `.claude.json`. Claude Code
   picks it up on the next session.

If you're on an unknown network (traveling, coffee shop) neither scope matches and neither MCP loads.

## Verify

```bash
llmenv doctor            # confirm which scopes and MCPs are active
llmenv export --dry-run  # preview the manifest that would be written
```

## Tips

- Run `ip neigh show default` (Linux) or
  `arp -n $(route -n get default | grep gateway | awk '{print $2}')` (macOS) to find your
  router's MAC. <!-- markdownlint-disable-line MD013 -->
- You can stack multiple network scopes — if you have a VPN that changes the gateway, add it
  as a third entry with its own tags.
