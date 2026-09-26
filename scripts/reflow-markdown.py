#!/usr/bin/env python3
"""Reflow prose paragraphs in Markdown while preserving block structure.

Joins prose lines within paragraphs to single lines, while preserving:
- Blank lines (paragraph separators)
- Headings (lines starting with #)
- List items (lines starting with -, *, +)
- Code blocks (lines within ``` fences)
- HTML/raw blocks

Usage: reflow-markdown.py < input.md > output.md
"""

import sys

def reflow_markdown(text: str) -> str:
    lines = text.rstrip('\n').split('\n')
    result = []
    i = 0
    in_code_block = False

    while i < len(lines):
        line = lines[i]

        # Toggle code block state
        if line.startswith('```'):
            in_code_block = not in_code_block
            result.append(line)
            i += 1
            continue

        # Inside code block: preserve as-is
        if in_code_block:
            result.append(line)
            i += 1
            continue

        # Blank line: preserve
        if not line.strip():
            result.append(line)
            i += 1
            continue

        # Block element (heading, table, blockquote, etc.): preserve as-is
        if (line.lstrip().startswith('#') or  # heading
            line.lstrip().startswith('|') or  # table
            line.lstrip().startswith('>') or  # blockquote
            line.startswith('    ') or line.startswith('\t')):  # indented (code)
            result.append(line)
            i += 1
            continue

        # List item or paragraph: collect lines and join
        paragraph = [line]
        i += 1
        while i < len(lines):
            next_line = lines[i]
            # Stop if blank
            if not next_line.strip():
                break
            # Stop if a new list item or block element
            if (next_line.lstrip().startswith('#') or
                next_line.lstrip().startswith('|') or
                next_line.lstrip().startswith('>') or
                next_line.startswith('```')):
                break
            # Stop if non-indented line that starts a new list/hr
            if not next_line.startswith((' ', '\t')):
                if (next_line.lstrip().startswith('-') or
                    next_line.lstrip().startswith('*') or
                    next_line.lstrip().startswith('+')):
                    break
            # Include indented continuation or prose
            paragraph.append(next_line)
            i += 1

        # Join paragraph/list lines
        joined = ' '.join(p.strip() for p in paragraph)
        result.append(joined)

    if in_code_block:
        print("reflow-markdown: unclosed code fence in input — aborting", file=sys.stderr)
        sys.exit(1)

    return '\n'.join(result)

if __name__ == '__main__':
    try:
        text = sys.stdin.read()
    except OSError as exc:
        print(f"reflow-markdown: failed to read stdin: {exc}", file=sys.stderr)
        sys.exit(1)
    try:
        print(reflow_markdown(text), end='')
        sys.stdout.flush()
    except BrokenPipeError:
        pass  # stdout closed, exit silently
