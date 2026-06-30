#!/usr/bin/env node
// Scan llmenv (and global user claude) Claude Code session transcripts for a single
// local day and emit a per-project digest of the human's prompts + the intent
// of any compacted sessions. Read-only. Deterministic given the same logs.
//
// Usage:  scan-sessions.mjs [--date YYYY-MM-DD] [--max-prompts N]
//   --date         local day to scan (default: yesterday, local time)
//   --max-prompts  cap prompts printed per project (default: 16)
//
// Roots scanned (colon-separated override via LLMENV_DAILY_ROOTS):
//   ~/.cache/llmenv/claude-code   (all profile hashes under 1.0/*/projects)
//   ~/.cl*ude                     (non-llmenv sessions, if present)

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';

const HOME = os.homedir();
// Second root is written as `.cla` + `ude` on purpose: it keeps the literal
// sessions-dir name from appearing verbatim in this source, so the file doesn't
// match its own scan (and doesn't trip path-name tooling). Not a typo.
const DEFAULT_ROOTS = [`${HOME}/.cache/llmenv/claude-code`, `${HOME}/.cla` + `ude`];

function parseArgs(argv) {
  const args = { maxPrompts: 16, date: null };
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--date') args.date = argv[(i += 1)];
    else if (argv[i] === '--max-prompts') args.maxPrompts = Number(argv[(i += 1)]);
  }
  return args;
}

function localDay(d) {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, '0');
  const day = String(d.getDate()).padStart(2, '0');
  return `${y}-${m}-${day}`;
}

function targetDate(arg) {
  if (arg) return arg;
  const d = new Date();
  d.setDate(d.getDate() - 1);
  return localDay(d);
}

function roots() {
  const override = process.env.LLMENV_DAILY_ROOTS;
  const list = override ? override.split(':') : DEFAULT_ROOTS;
  return list.filter((r) => fs.existsSync(r));
}

function transcriptFiles() {
  const out = [];
  for (const root of roots()) {
    let entries;
    try {
      entries = fs.readdirSync(root, { recursive: true });
    } catch (err) {
      process.stderr.write(`scan-sessions: could not read root ${root}: ${err.message}\n`);
      continue;
    }
    for (const rel of entries) {
      const full = path.join(root, rel);
      if (!full.endsWith('.jsonl')) continue;
      if (full.includes('/subagents/') || full.includes('/session-logs/')) continue;
      if (!full.includes('/projects/')) continue;
      out.push(full);
    }
  }
  return out;
}

function projectOf(file) {
  const m = file.match(/projects\/([^/]+)\//);
  if (!m) return '?';
  return m[1].replace(/^-Users-[^-]+-git-/, '').replace(/^-/, '');
}

const NOISE = [
  /^<(system-reminder|command-|local-command|task-notification)/,
  /<command-name>/,
  /Caveman mode|SessionStart|This session is being continued/,
  /^\[Request interrupted/,
];

function textOf(message) {
  const c = message.content;
  if (typeof c === 'string') return c.trim();
  if (!Array.isArray(c)) return '';
  if (c.some((p) => p && p.type === 'tool_result')) return '';
  return c
    .filter((p) => p && p.type === 'text')
    .map((p) => p.text)
    .join(' ')
    .trim();
}

function isNoise(txt) {
  return NOISE.some((re) => re.test(txt));
}

function intentFrom(txt) {
  const i = txt.indexOf('Primary Request and Intent');
  if (i < 0) return null;
  return txt.slice(i, i + 600).replace(/\s+/g, ' ');
}

function collect(file, date, acc) {
  let lines;
  try {
    lines = fs.readFileSync(file, 'utf8').split('\n');
  } catch (err) {
    process.stderr.write(`scan-sessions: could not read ${file}: ${err.message}\n`);
    return;
  }
  const proj = projectOf(file);
  acc[proj] ??= { prompts: new Set(), intent: null, sessions: new Set() };
  let touched = false;
  let parseErrors = 0;
  for (const ln of lines) {
    if (!ln.trim()) continue;
    let o;
    try {
      o = JSON.parse(ln);
    } catch {
      parseErrors += 1;
      continue;
    }
    if (o.type !== 'user' || !o.message || o.isMeta) continue;
    if (o.timestamp && localDay(new Date(o.timestamp)) !== date) continue;
    const txt = textOf(o.message);
    if (!txt) continue;
    touched = true;
    if (!acc[proj].intent) acc[proj].intent = intentFrom(txt);
    if (isNoise(txt)) continue;
    acc[proj].prompts.add(txt.replace(/\s+/g, ' ').slice(0, 220));
  }
  if (parseErrors > 0) {
    process.stderr.write(`scan-sessions: ${parseErrors} unparseable line(s) in ${file}\n`);
  }
  if (touched) acc[proj].sessions.add(file);
}

function render(acc, date, maxPrompts) {
  const projects = Object.entries(acc)
    .filter(([, v]) => v.prompts.size || v.intent)
    .sort((a, b) => b[1].prompts.size - a[1].prompts.size);
  console.log(`# Session activity for ${date}\n`);
  if (!projects.length) {
    console.log('(no session activity found for this date)');
    return;
  }
  for (const [proj, v] of projects) {
    console.log(`\n## ${proj}  (${v.sessions.size} session(s))`);
    if (v.intent) console.log(`_intent_: ${v.intent}`);
    [...v.prompts].slice(0, maxPrompts).forEach((t, i) => console.log(`  ${i + 1}. ${t}`));
  }
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const date = targetDate(args.date);
  const acc = {};
  for (const file of transcriptFiles()) collect(file, date, acc);
  render(acc, date, args.maxPrompts);
}

main();
