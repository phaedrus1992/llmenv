LLM Environment
===============

A universal environment for LLMs. Like direnv for your agents.

Features
========

* define `AGENTS.md`-compatible rules, skills, and plugins and assign them
  to tags
* configure tags to apply to the network, host, user, or project scopes
* integrated memory with scope- and tag-awareness using the `icm` MCP
* configures hooks for automatic integration with your coding agents
* automatic sync with github

Supported LLM Systems
=====================

* Claude Code
* Codex
* Crush
* OpenCode

How Does It Work?
=================

LLMe runs a small, lightweight Rust coordinator that tracks your
configuration and automatically applies it, direnv-like, to your current
working environment.

Create bundles of config just like you would for a local project. Rules,
plugins, skills, documentation, etc. Then configure them to apply to various
scopes.

As you move around in your systems and projects, it applies a custom
`CLAUDE_CONFIG_DIR` for the current scope based on your configuration.

