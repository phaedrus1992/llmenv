# Model and providers

Codex lets you pick the model and define **custom model providers** — base URL,
wire API, auth, headers. Claude Code has no equivalent (the model is fixed by the
Anthropic backend), so this is entirely new territory for llmenv.

## Selecting a model

```toml
model = "gpt-5.4"
model_provider = "proxy"   # defaults to "openai"
```

Other model keys: `model_instructions_file`, `model_reasoning_effort`,
`review_model`, `service_tier`, `show_raw_agent_reasoning`.

## Custom providers

Reserved built-in provider IDs (`openai`, `ollama`, `lmstudio`) **cannot** be
redefined. Define your own:

```toml
[model_providers.proxy]
name = "OpenAI using LLM proxy"
base_url = "http://proxy.example.com"
env_key = "OPENAI_API_KEY"
wire_api = "responses"          # responses | chat

[model_providers.local_ollama]
name = "Ollama"
base_url = "http://localhost:11434/v1"
```

Headers:

```toml
[model_providers.example]
http_headers = { "X-Example-Header" = "example-value" }
env_http_headers = { "X-Example-Features" = "EXAMPLE_FEATURES" }  # value from env
```

Command-backed auth (fetch bearer tokens from an external helper):

```toml
[model_providers.proxy.auth]
command = "/usr/local/bin/fetch-codex-token"
args = ["--audience", "codex"]
timeout_ms = 5000
refresh_interval_ms = 300000
```

The auth command gets no stdin, must print the token to stdout (trimmed; empty =
error), and is refreshed proactively at `refresh_interval_ms`.

Per-provider tuning (Azure example):

```toml
[model_providers.azure]
name = "Azure"
base_url = "https://YOUR_PROJECT.openai.azure.com/openai"
env_key = "AZURE_OPENAI_API_KEY"
query_params = { api-version = "2025-04-01-preview" }
wire_api = "responses"
request_max_retries = 4
stream_max_retries = 10
stream_idle_timeout_ms = 300000
```

To change the built-in OpenAI base URL, set top-level `openai_base_url` — don't
create `[model_providers.openai]`.

## Gaps vs llmenv

llmenv's `Settings` is about cache/sync, not the agent model. A `CodexAdapter`
would need:

- New schema input for `model` / `model_provider`.
- An optional `[model_providers.*]` block if we want to generate custom-provider
  config (base URL, wire API, env key, headers, auth command). This is a
  meaningful surface — for self-hosted/proxied deployments it's the whole point.
- Recognition that the **selection model could drive this**: different scopes
  (e.g. a corp network) could select a different provider/model via tag
  intersection, the same mechanism that selects bundles. That's a natural fit but
  requires schema work.
