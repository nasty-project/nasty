#!/usr/bin/env python3
"""
Regenerate webui/src/lib/smart_attribute_metadata.ts from Scrutiny's
upstream ATA SMART attribute metadata table.

Usage:
    ./scripts/extract_smart_attribute_metadata.py

Fetches the metadata from github.com/AnalogJ/scrutiny (MIT license),
extracts the {name, critical, ideal, description} fields from each
entry, and writes a TypeScript constant + lookup helper to
webui/src/lib/smart_attribute_metadata.ts.

Scrutiny's `critical` flag derives from Backblaze drive-stats failure
rate analysis (CC-BY 4.0). When upstream updates their table (e.g.
based on a fresh Backblaze data dump) re-run this script to pick up
the changes; review the diff before committing.

Why not RPC the data from the engine? Pure static lookup table, ~20KB
JSON. Putting it on the frontend avoids a per-page-load RPC and keeps
the engine surface narrow. If a future use case wants the metadata
in alert rules, copy the same file into nasty-common as a Rust const.
"""
import json
import re
import sys
import urllib.request
from pathlib import Path

UPSTREAM_URL = (
    "https://raw.githubusercontent.com/AnalogJ/scrutiny/master/"
    "webapp/backend/pkg/thresholds/ata_attribute_metadata.go"
)
OUTPUT_PATH = Path(__file__).resolve().parent.parent / (
    "webui/src/lib/smart_attribute_metadata.ts"
)

# Map Scrutiny's Go constants → string values we use in TS.
GO_CONSTANTS = {
    "ObservedThresholdIdealLow": "low",
    "ObservedThresholdIdealHigh": "high",
    "AtaSmartAttributeDisplayTypeRaw": "raw",
    "AtaSmartAttributeDisplayTypeNormalized": "normalized",
    "AtaSmartAttributeDisplayTypeTransformed": "transformed",
}


def fetch_source() -> str:
    print(f"Fetching {UPSTREAM_URL}…")
    with urllib.request.urlopen(UPSTREAM_URL) as resp:
        return resp.read().decode("utf-8")


def parse_entries(src: str) -> list[tuple[int, str]]:
    """Walk the Go source and pull out (id, body) for each map entry."""
    start = src.find("AtaMetadata = map[int]AtaAttributeMetadata{")
    if start < 0:
        raise SystemExit("AtaMetadata map literal not found in upstream source")
    i = src.find("{", start) + 1

    # Capture the map body (balanced-brace).
    depth = 0
    body_chars = []
    while i < len(src):
        c = src[i]
        if c == "{":
            depth += 1
        elif c == "}":
            if depth == 0:
                break
            depth -= 1
        body_chars.append(c)
        i += 1
    body = "".join(body_chars)

    # Split into per-entry chunks. Each is `<int>: { … },` at top depth.
    entries = []
    j = 0
    while j < len(body):
        while j < len(body) and body[j] in " \t\n,":
            j += 1
        if j >= len(body):
            break
        m = re.match(r"(\d+):\s*\{", body[j:])
        if not m:
            j += 1
            continue
        aid = int(m.group(1))
        j += m.end()
        depth = 1
        estart = j
        while j < len(body) and depth > 0:
            if body[j] == "{":
                depth += 1
            elif body[j] == "}":
                depth -= 1
            j += 1
        entries.append((aid, body[estart:j - 1]))
    return entries


def field_string(entry: str, key: str) -> str | None:
    m = re.search(rf'{key}:\s*"((?:[^"\\]|\\.)*)"', entry)
    return m.group(1).encode().decode("unicode_escape") if m else None


def field_bool(entry: str, key: str) -> bool:
    m = re.search(rf"{key}:\s*(true|false)", entry)
    return m.group(1) == "true" if m else False


def field_ident(entry: str, key: str) -> str:
    m = re.search(rf"{key}:\s*(\w+(?:\.\w+)*)", entry)
    if not m:
        return ""
    return GO_CONSTANTS.get(m.group(1), "")


def escape_ts(s: str) -> str:
    return s.replace("\\", "\\\\").replace("'", "\\'")


def emit_ts(entries: list[tuple[int, dict]]) -> str:
    lines = [
        "/**",
        " * Per-attribute metadata for ATA SMART attributes.",
        " *",
        " * Adapted from Scrutiny's curated metadata table",
        " * (github.com/AnalogJ/scrutiny, MIT license). Scrutiny's `critical`",
        " * flag derives from Backblaze drive-stats failure-rate analysis",
        " * (CC-BY 4.0), so the eight attributes flagged here are the ones",
        " * Backblaze's empirical data shows most reliably predict drive",
        " * failure — vs. vendor-supplied SMART thresholds which are",
        " * notoriously stingy.",
        " *",
        " * `ideal` describes which raw-value direction is healthy:",
        " *   - 'low'  — higher values are worse (e.g. reallocated sector count)",
        " *   - 'high' — lower values are worse (e.g. helium level)",
        " *   - ''     — no clear direction (vendor-specific encoded values)",
        " *",
        " * To refresh: re-run scripts/extract_smart_attribute_metadata.py",
        " * against the upstream ata_attribute_metadata.go.",
        " */",
        "",
        "export interface AtaAttributeMetadata {",
        "\tname: string;",
        "\tcritical: boolean;",
        "\tideal: 'low' | 'high' | '';",
        "\tdescription: string;",
        "}",
        "",
        "export const ATA_ATTRIBUTE_METADATA: Record<number, AtaAttributeMetadata> = {",
    ]
    for aid, e in sorted(entries, key=lambda kv: kv[0]):
        name = escape_ts(e["name"])
        desc = escape_ts(e.get("description") or "")
        ideal = e["ideal"]
        crit = "true" if e["critical"] else "false"
        lines.append(
            f"\t{aid}: {{ name: '{name}', critical: {crit}, "
            f"ideal: '{ideal}', description: '{desc}' }},"
        )
    lines.extend(
        [
            "};",
            "",
            "/**",
            " * Look up metadata for an ATA attribute ID, with a sensible fallback",
            " * for vendor-specific attributes Scrutiny's table doesn't cover.",
            " * Vendor-only IDs (typically 200+) appear on individual drives but",
            " * have no portable interpretation — we fall through to the drive's",
            " * own attribute name + a generic description.",
            " */",
            "export function ataAttributeMetadata(id: number, fallbackName: string): AtaAttributeMetadata {",
            "\treturn ATA_ATTRIBUTE_METADATA[id] ?? {",
            "\t\tname: fallbackName,",
            "\t\tcritical: false,",
            "\t\tideal: '',",
            "\t\tdescription: 'Vendor-specific attribute — interpretation varies by drive manufacturer.',",
            "\t};",
            "}",
            "",
        ]
    )
    return "\n".join(lines)


def main() -> int:
    src = fetch_source()
    raw_entries = parse_entries(src)
    print(f"Parsed {len(raw_entries)} entries from upstream")

    parsed = []
    for aid, body in raw_entries:
        parsed.append(
            (
                aid,
                {
                    "name": field_string(body, "DisplayName") or "",
                    "critical": field_bool(body, "Critical"),
                    "ideal": field_ident(body, "Ideal"),
                    "description": field_string(body, "Description") or "",
                },
            )
        )

    OUTPUT_PATH.write_text(emit_ts(parsed))
    print(f"Wrote {OUTPUT_PATH} ({OUTPUT_PATH.stat().st_size} bytes)")

    # Sanity: list critical attributes for the reviewer's eyeball check.
    crit = [(aid, e["name"]) for aid, e in parsed if e["critical"]]
    print(f"\nCritical attributes ({len(crit)}):")
    for aid, name in sorted(crit):
        print(f"  {aid:3d}  {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
