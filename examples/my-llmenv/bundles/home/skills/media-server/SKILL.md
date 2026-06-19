---
name: media-server
description: >
  Query and curate a local Plex or Jellyfin media server. Supports browsing
  libraries, fetching playback history, and building music playlists. Only
  available on the home network (home bundle, SSID-scoped).
---

<!--
  This skill is in bundles/home/skills/ so it only loads when the `home` tag
  is active (home Wi-Fi connected). On any other network it doesn't exist in
  the agent's context, which avoids confusing "connection refused" errors.

  HOW IT CONNECTS:
    - Activated by: home network SSID → `home` tag → home bundle → this skill
    - Uses: WebFetch to the local server's HTTP API (allowed by home/bundle.yaml)
    - No MCP server needed — the Plex/Jellyfin REST API is called directly via
      WebFetch. The server address is configured below.

  SETUP:
    1. Set SERVER_URL to your server's local address.
    2. Set TOKEN to your Plex token (Settings > Account > Plex token) or
       Jellyfin API key (Dashboard > API Keys).
    3. Run `llmenv regenerate` to pick up the change.

  SECURITY NOTE:
    The token is embedded in request URLs. Keep this SKILL.md out of version
    control (or use an env var substitution once llmenv supports it). The
    native_permissions in home/bundle.yaml already restricts WebFetch to the
    local domain, so the token can't be sent outside the LAN.
-->

# Media Server Skill

**Server:** `http://plex.local:32400` (Plex) or `http://jellyfin.local:8096` (Jellyfin)
**Token:** Set `PLEX_TOKEN` or `JELLYFIN_API_KEY` in your shell environment.

## Step 0 — Discover libraries

Before any query, list available libraries to find the correct library key.

**Plex:**
```
GET http://plex.local:32400/library/sections?X-Plex-Token=${PLEX_TOKEN}
```

**Jellyfin:**
```
GET http://jellyfin.local:8096/Library/MediaFolders
Headers: X-Emby-Token: ${JELLYFIN_API_KEY}
```

## Mode A: Media Queries

### Browse a library

```
# Plex — list items in library section (key from Step 0)
GET /library/sections/<key>/all?X-Plex-Token=...&sort=addedAt:desc&limit=50

# Jellyfin
GET /Items?ParentId=<libraryId>&SortBy=DateCreated&SortOrder=Descending&Limit=50
```

### Playback history

```
# Plex — recently played
GET /status/sessions/history/all?X-Plex-Token=...&limit=20

# Jellyfin
GET /Users/<userId>/Items?IsPlayed=true&SortBy=DatePlayed&Limit=20
```

## Mode B: Music Playlist Curation

Use when asked to build a playlist for a mood, occasion, or genre.

### Step 1 — Map mood to genre signals

| Mood | Signals |
|------|---------|
| Focus / work | Instrumental, ambient, lo-fi, jazz |
| Energy / workout | BPM > 140, rock, electronic, hip-hop |
| Relax | Acoustic, folk, soft jazz, classical |
| Social / party | Pop, dance, upbeat indie |

### Step 2 — Fetch music library

Retrieve artists and albums. Filter client-side to matching genres.

### Step 3 — Score and rank

Score each track:
- Genre match: +3
- Artist variety (don't cluster): +1
- Recent play penalty: −1 per play in last 7 days

Pick top 20-30 tracks by score. Interleave artists.

### Step 4 — Create playlist

**Plex:**
```
POST /playlists?type=audio&title=<name>&smart=0&uri=<trackURIs>&X-Plex-Token=...
```

**Jellyfin:**
```
POST /Playlists
Body: { "Name": "<name>", "Ids": ["<trackId1>", ...], "MediaType": "Audio" }
```

## Output style

- Markdown tables for stats and genre breakdowns
- Numbered lists for tracks (preserve order)
- Lead with the most interesting finding — skip preamble
- For large result sets: show top 10, summarize the rest as counts
