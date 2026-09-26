# Issue #2164 — clear website npm advisories and CodeQL key alerts

- **Issue:** https://github.com/phaedrus1992/llmenv/issues/2164
- **Milestone:** `v3.11.2`
- **Branches:** npm part on `release/3.x` (forward-merges up); CodeQL part on `release/4.x` (forward-merges to `main`)
- **Type:** security fix
- **Blocked by:** #2166 for the npm part to reach `release/4.x` and `main` through the forward-merge

This is a spec, not a plan.

## Findings (2026-09-26)

### Website npm

`npm audit --package-lock-only` on each branch's `website/package-lock.json`:

| Package | Branches | Severity | Advisory | Path | Fixed in |
| --- | --- | --- | --- | --- | --- |
| `lodash-es` 4.17.23 | 3.x, 4.x, main | high, medium | GHSA-r5fr-rjxr-66jc, GHSA-f23m-r3pf-42rh (Dependabot #48, #47) | `mermaid` → `chevrotain` / `@chevrotain/*` (pins `lodash-es` `4.17.23` exactly), and `dagre-d3-es` (`^4.17.21`) | 4.18.0; current 4.18.1 (published 2026-04-01) |
| `serialize-javascript` ≤ 7.0.4 | 3.x only | high | npm audit | Docusaurus (pinned) | 4.x override `7.1.1` |
| `uuid` < 11.1.1 (and `sockjs`, `webpack-dev-server`, `copy-webpack-plugin`, `css-minimizer-webpack-plugin`, `@docusaurus/*` moderates that clear with it) | 3.x only | moderate | npm audit | Docusaurus (pinned) | 4.x override `14.0.2` |

`release/4.x` fixed `serialize-javascript` and `uuid` with `overrides` in commit `012a9573` (2026-08-18); `uuid` moved to 14 later in `63207518`.
The change never reached `release/3.x`, whose `website/package.json` has no `overrides` key.
`npm audit fix` cannot move any of these: each is a transitive dependency pinned by its parent (the `012a9573` message records the same for Docusaurus).

### CodeQL (release/4.x and main only)

Five open critical alerts, rule `rust/hard-coded-cryptographic-value`, message "This hard-coded value is used as a key", raised on commit `8997c93cb`:

| Alert | Location at `8997c93cb` | Flagged value |
| --- | --- | --- |
| #29 | `crates/llmenv-paths/src/lib.rs:257` | `".local/state/llmenv"` |
| #32 | `src/launch/socket.rs:299` | `"llmenv"` |
| #30 | `src/hook_run/launch_client.rs:245` | `"wrong-token"` (test) |
| #31 | `src/hook_run/launch_client.rs:305` | `"real-token"` (test) |
| #33 | `tests/hook_run_launch_notice.rs:35` | `TEST_TOKEN` (test) |

The production key is random: `LaunchToken::generate` fills 32 bytes from `getrandom` (`src/launch/socket.rs` lines 83 to 93).
#29 and #32 are path strings. They reach the HMAC key only in CodeQL's model: `socket::bind(pid)` returns the 4-tuple `(UnixListener, NoticeSlot, PathBuf, LaunchToken)` (line 319), and CodeQL does not keep tuple elements apart, so the path taints the token.
#30, #31 and #33 are literal tokens in tests.

None of this code exists on `release/3.x` (`src/launch/socket.rs`, `src/hook_run/launch_client.rs` and `tests/hook_run_launch_notice.rs` are absent).

`cargo deny check advisories` is clean on `release/3.x` and `release/4.x`.

## Changes

### A. npm overrides (`release/3.x`)

In `website/package.json`, add:

```json
"overrides": {
  "lodash-es": "4.18.1",
  "serialize-javascript": "7.1.1",
  "uuid": "14.0.2"
}
```

- Exact versions, no `^` or `~` (project rule).
- `uuid` 14.0.2 matches 4.x. Before committing, confirm the site builds and `sockjs` loads (commit `012a9573` chose 11.1.1 at first because `sockjs` needs CJS; 4.x has since run on 14.0.2). If the 3.x build fails with 14.0.2, use `11.1.1` and say so in the commit body.
- Regenerate the lockfile with `npm install --package-lock-only --ignore-scripts` inside `website/`. Do not run install scripts.
- Then run `npm audit --package-lock-only` inside `website/`; it must report no high or critical findings and none of the packages in the table above.
- Build the site (`npm run build` inside `website/`, the repo's docs workflow command) to confirm it still builds.

On `release/4.x` and `main`, the forward-merge carries the `lodash-es` override. The `serialize-javascript` and `uuid` keys already exist there with the same values, so they merge cleanly. If #2166 is not fixed yet, apply the `lodash-es` override on `release/4.x` by hand in the manual forward-merge resolution.

### B. CodeQL (`release/4.x`)

1. In `src/launch/socket.rs`, replace the tuple return of `bind` with a named struct:

   ```rust
   /// What `bind` hands back: the listener, the notice mailbox, the socket
   /// path (for LLMENV_LAUNCH_SOCKET and cleanup) and the shared secret.
   pub(crate) struct BoundSocket {
       pub listener: UnixListener,
       pub notices: NoticeSlot,
       pub path: PathBuf,
       pub token: LaunchToken,
   }
   pub(crate) fn bind(pid: u32) -> anyhow::Result<BoundSocket>
   ```

   Update every caller: `src/launch/mod.rs` near line 267 and the tests in `src/hook_run/launch_client.rs` near lines 218, 241 and 339, and in `src/launch/socket.rs`.
   `src/launch/proxy.rs` line 201 also returns a tuple that holds a token; convert it the same way so the pattern does not return in the next scan.
2. In `src/hook_run/launch_client.rs` tests, replace `"wrong-token"` and `"real-token"` with tokens from `LaunchToken::generate()`; the wrong-token test uses a second, separately generated token.
3. In `tests/hook_run_launch_notice.rs`, replace `const TEST_TOKEN` with a function that returns a fresh 64-character hex token: 32 bytes from `getrandom::fill`, encoded with `hex::encode` (both are already package dependencies). Generate it once per test and pass it to both sides.

Do not dismiss the alerts in the GitHub UI. They must close because a new scan no longer finds them.

## Acceptance criteria

1. Dependabot alerts #47 and #48 close after the change reaches `main`.
2. `npm audit --package-lock-only` in `website/` on `release/3.x` shows no high or critical findings.
3. CodeQL alerts #29 to #33 close on `release/4.x` and `main` after the next scan.
4. All tests pass on both branches; the website builds on `release/3.x`.
5. Changelog `Security` entry in the 3.x changelog for the website dependency fixes. The CodeQL change is a refactor with no user-visible effect; no changelog entry.

## Out of scope

- Upgrading Docusaurus or mermaid.
- Detection for release branches; that is #2165.
