#!/usr/bin/env python3
"""Deterministic smart-merge for the living fyi.

Folds a fresh raw scan (list of {id,tier,title,note,refs}) into the persisted
living list, keyed by stable `id`. The LLM scan never sees check state, so a
re-scan can never silently uncheck your work.

Rules:
- scan item not in prior data  -> status "new", unchecked
- scan item in prior data       -> keep checked/manual/firstSeen, refresh fields
- prior item gone from scan      -> status "done", checked (auto-completed),
                                    kept visible until the date rolls over
- new date                       -> fresh list (done items pruned, checks reset)

Usage:
    merge.py <scan.json> <data.json>      merge scan into data (writes data.json)
    merge.py --selftest                   run the built-in checks
"""
import json
import os
import re
import sys
from datetime import datetime, timezone

TIER_ORDER = {"urgent": 0, "in_progress": 1, "pending": 2}


def _now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def merge(scan, prior, now=None, today=None):
    """Merge a raw scan list into the prior data dict. Pure, no I/O."""
    now = now or _now()
    today = today or now[:10]
    # Date rollover (or no prior) starts a clean day: no carried-over done
    # items, check state reset, everything re-flagged new.
    if not prior or prior.get("date") != today:
        prior = {"date": today, "items": []}
    prior_items = {it["id"]: it for it in prior["items"]}

    scan_ids = set()
    out = []
    for s in scan:
        sid = s["id"]
        scan_ids.add(sid)
        base = {
            "id": sid,
            "tier": s.get("tier", "pending"),
            "title": s["title"],
            "note": s.get("note", ""),
            "refs": s.get("refs", []),
        }
        old = prior_items.get(sid)
        if old:
            out.append({**base, "checked": old.get("checked", False),
                        "manual": old.get("manual", False), "status": "open",
                        "firstSeen": old.get("firstSeen", now)})
        else:
            out.append({**base, "checked": False, "manual": False,
                        "status": "new", "firstSeen": now})

    # Work that dropped out of the scan cleared today -> auto-complete it.
    for old in prior["items"]:
        if old["id"] not in scan_ids:
            out.append({**old, "status": "done", "checked": True})

    # Stable sort keeps scan order within a tier; done items sink to the bottom.
    out.sort(key=lambda it: (TIER_ORDER.get(it["tier"], 3),
                             1 if it["status"] == "done" else 0))
    return {"date": today, "lastScan": now, "items": out}


def _load_scan(path):
    """Load the raw scan, tolerating stray prose / markdown fences around it."""
    with open(path, encoding="utf-8") as f:
        raw = f.read().strip()
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        m = re.search(r"(\[.*\]|\{.*\})", raw, re.S)
        if not m:
            raise SystemExit(f"scan: no JSON found in {path}")
        print(f"merge: {path} wasn't clean JSON; extracting embedded JSON "
              "via regex fallback", file=sys.stderr)
        try:
            data = json.loads(m.group(1))
        except json.JSONDecodeError as e:
            raise SystemExit(f"scan: could not parse extracted JSON from {path}: {e}")
    if isinstance(data, dict):
        data = data.get("items", [])
    return data


def _load_data(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def _selftest():
    prior = {"date": "2026-06-22", "items": [
        {"id": "a", "tier": "urgent", "title": "A", "note": "", "refs": [],
         "checked": True, "manual": True, "status": "open", "firstSeen": "x"},
        {"id": "b", "tier": "in_progress", "title": "B", "note": "", "refs": [],
         "checked": False, "manual": False, "status": "open", "firstSeen": "x"},
    ]}
    scan = [
        {"id": "a", "tier": "urgent", "title": "A", "refs": []},   # still open, was checked
        {"id": "c", "tier": "urgent", "title": "C", "refs": []},   # brand new
    ]  # b dropped -> done
    r = merge(scan, prior, now="2026-06-22T14:00:00Z", today="2026-06-22")
    items = {it["id"]: it for it in r["items"]}
    assert items["a"]["checked"] is True and items["a"]["status"] == "open", items["a"]
    assert items["c"]["status"] == "new" and items["c"]["checked"] is False, items["c"]
    assert items["b"]["status"] == "done" and items["b"]["checked"] is True, items["b"]
    # done sinks below open within the urgent tier; b (in_progress) after urgents
    assert [it["id"] for it in r["items"]] == ["a", "c", "b"], r["items"]

    # Date rollover: fresh day wipes done + resets checks + re-flags new.
    r2 = merge(scan, r, now="2026-06-23T08:00:00Z", today="2026-06-23")
    ids = {it["id"] for it in r2["items"]}
    assert "b" not in ids, "done item should be pruned on date rollover"
    a2 = {it["id"]: it for it in r2["items"]}["a"]
    assert a2["checked"] is False and a2["status"] == "new", a2
    print("ok")


if __name__ == "__main__":
    if len(sys.argv) == 2 and sys.argv[1] == "--selftest":
        _selftest()
    elif len(sys.argv) == 3:
        scan = _load_scan(sys.argv[1])
        if not scan:
            raise SystemExit("scan: empty, refusing to overwrite data.json")
        result = merge(scan, _load_data(sys.argv[2]))
        out_path = sys.argv[2]
        tmp_path = out_path + ".tmp"
        with open(tmp_path, "w", encoding="utf-8") as f:
            json.dump(result, f, indent=2)
        os.replace(tmp_path, out_path)
        print(f"merged {len(result['items'])} items -> {out_path}")
    else:
        raise SystemExit(__doc__)
