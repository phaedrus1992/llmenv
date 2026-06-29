---
name: plex
description: >
  Interact with the local Plex Media Server using plexctl. Use for: browsing and
  querying the media archive (library stats, genre breakdowns, recently added,
  playback history, collections); and curating music playlists from abstract
  descriptions of mood, energy, atmosphere, or style. Triggers: "what's in plex",
  "show me my music", "create a playlist for", "plex query", "what have I been
  watching", "find me something to listen to", "make a playlist that feels like".
---

# Plex Skill

Interact with Plex Media Server via `plexctl`. Two modes:

1. **Media queries** — explore archive, report stats
2. **Playlist curation** — pick tracks for mood/energy/style

Use `-o json-pretty` for machine-readable output. Use `--all` for full library (slow for large — filter first).

---

## Step 0 — Discover libraries

List libraries to get key IDs:

```bash
plexctl library list -o json-pretty
```

Libraries (cached; re-run if uncertain):
| key | title | type |
|-----|-------|------|
| 1   | Anime | show |
| 2   | Movies | movie |
| 3   | TV Shows | show |
| 8   | Music | artist |
| 10  | Miscellaneous | movie |
| 14  | Audiobooks | artist |
| 22  | Music Videos | artist |
| 23  | Hockey | show |
| 27  | Whisky | movie |

---

## Mode A: Media Queries

### Browsing a library

```bash
# First page of items (default 50)
plexctl library show 8 -o json-pretty

# All items (use sparingly for large libraries)
plexctl library show 8 -o json-pretty --all

# Paginate for large libraries
plexctl library show 8 -o json-pretty --count 100 --page 2

# Sort by a field
plexctl library show 8 -o json-pretty --all --sort title
```

### Collections

```bash
# List collections in a library
plexctl collection list 8 -o json-pretty

# Show what's in a collection
plexctl collection show <ratingKey> -o json-pretty
```

### Playlists

```bash
plexctl playlist list -o json-pretty
plexctl playlist show <ratingKey> -o json-pretty
```

### Playback history

```bash
# Default: last 1 week
plexctl history -o json-pretty

# Custom window
plexctl history -o json-pretty --since 30d
plexctl history -o json-pretty --since 3h
```

### Query patterns

Parse JSON with `uv run python3` inline:

**Genre breakdown for music library:**
```bash
plexctl library show 8 -o json-pretty --all | uv run python3 -c "
import json, sys
from collections import Counter
data = json.load(sys.stdin)
genres = Counter()
for item in data:
    for g in item.get('Genre', []):
        genres[g['tag']] += 1
for genre, count in genres.most_common(20):
    print(f'{count:5d}  {genre}')
"
```

**Artists by country:**
```bash
plexctl library show 8 -o json-pretty --all | uv run python3 -c "
import json, sys
from collections import Counter
data = json.load(sys.stdin)
countries = Counter()
for item in data:
    for c in item.get('Country', []):
        countries[c['tag']] += 1
for country, count in countries.most_common(20):
    print(f'{count:5d}  {country}')
"
```

**Recently added (last N days):**
```bash
plexctl library show 8 -o json-pretty --all | uv run python3 -c "
import json, sys, time
data = json.load(sys.stdin)
cutoff = time.time() - (30 * 86400)
recent = [i for i in data if i.get('addedAt', 0) > cutoff]
recent.sort(key=lambda x: x['addedAt'], reverse=True)
for i in recent:
    import datetime
    dt = datetime.datetime.fromtimestamp(i['addedAt']).strftime('%Y-%m-%d')
    print(f'{dt}  {i[\"title\"]}')
"
```

**History summary (most-played artists this week):**
```bash
plexctl history -o json-pretty --since 1w | uv run python3 -c "
import json, sys
from collections import Counter
data = json.load(sys.stdin)
artists = Counter(
    i['grandparentTitle'] for i in data
    if i.get('type') == 'track'
)
for artist, count in artists.most_common(15):
    print(f'{count:4d}  {artist}')
"
```

---

## Mode B: Music Playlist Curation

When user describes mood/energy/atmosphere/style, produce curated track list from library only — no invented tracks.

### Step 1 — Map mood to genre signals

Map description to genre tags, keywords, artist archetypes. Be specific:

| Abstract | Genre signals to look for |
|----------|--------------------------|
| Late-night driving | Synthwave, Dark Ambient, Post-Rock, Shoegaze |
| High energy / workout | Metal, Industrial, Punk, Drum & Bass, Hardcore |
| Focus / concentration | Ambient, Drone, Minimalist, Classical, Lo-Fi |
| Melancholy / introspective | Slowcore, Post-Punk, Folk, Singer-Songwriter |
| Party / celebratory | Pop/Rock, Dance, Electronic, Funk, Soul |
| Eerie / unsettling | Dark Ambient, Black Metal, Noise, Industrial |
| Euphoric | Trance, House, Rave, New Age |
| Aggressive | Death Metal, Grindcore, Hardcore, Industrial |
| Romantic | Jazz, Classical, Soft Rock, R&B |
| Nostalgic | Classic Rock, 80s Pop, New Wave, Oldies |

### Step 2 — Fetch music library

```bash
plexctl library show 8 -o json-pretty --all
```

### Step 3 — Filter and score artists

Filter JSON by genre signal match. Score artists by signal count:

```bash
# (pipe the library show output into this)
uv run python3 -c "
import json, sys
data = json.load(sys.stdin)
signals = ['Electronic', 'Industrial', 'Dark Ambient']  # from Step 1

results = []
for item in data:
    genres = [g['tag'] for g in item.get('Genre', [])]
    score = sum(1 for s in signals if any(s.lower() in g.lower() for g in genres))
    if score > 0:
        results.append((score, item['title'], genres))

results.sort(reverse=True)
for score, title, genres in results[:30]:
    print(f'{score}  {title:40s}  {\" / \".join(genres[:3])}')
"
```

### Step 4 — Present curation

Output structured recommendation:

1. **Playlist name** — evocative, mood-matched (not just genre)
2. **Artist list** — grouped by intensity or arc
3. **Rationale** — one line per artist: why they fit
4. **Listening order** — gentle → peak → cool-down
5. **Collections** — surface matching collections from `plexctl collection list 8`

### Step 5 — Create playlist via API

`plexctl playlist` supports `list`/`show` only. To create in Plex, use HTTP API directly (needs server URL + token from `~/.plexctl.yaml`):

```bash
# Read server config
cat ~/.plexctl.yaml
```

The Plex API endpoint for creating a playlist:
```
POST /playlists?type=audio&title=<name>&smart=0&uri=<track-uris>&X-Plex-Token=<token>
```

Track URIs are in the form: `server://<machine-id>/com.plexapp.plugins.library/library/metadata/<ratingKey>`

Only call API if user explicitly asks. Do NOT pass token as CLI arg (visible in process list) — use env var or stdin.

---

## Output style

- Markdown tables for structured data (stats, genre breakdowns)
- Numbered lists for tracks (order preserved)
- Lead with most interesting finding, skip preamble
- Brief explanations — data first, no tutorial
- Excess data → summarize counts, show top N only
