#!/usr/bin/env python3
"""Replay sanitized cache evidence into production CLI/TUI renderers, offline."""
import html
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import textwrap

ROOT = Path(__file__).resolve().parent.parent
PROVENANCE = (
    "Captured from the installed Agyva’s existing account registry and quota cache; "
    "identifying fields replaced with samples. Quotas and timestamps preserved. "
    "No refresh or live login verification performed."
)


def replay(root):
    fixture = json.loads((ROOT / "docs/capture-data/accounts.json").read_text())
    for name in ("home", "cfg", "state", "cache", "agyhome", "fakeagy"):
        (root / name).mkdir(mode=0o700)
    shutil.copyfile(ROOT / "tests/fixtures/fake-agy.sh", root / "fakeagy/agy")
    (root / "fakeagy/agy").chmod(0o700)
    registry = {
        "schema_version": 1, "active": fixture["list"]["active"],
        "previous": next((a["id"] for a in fixture["list"]["accounts"] if a["previous"]), None),
        "accounts": [{key: a[key] for key in ("id", "alias", "email", "auth_method", "added_at", "last_used_at")}
                     | {"updated_at": a["added_at"]} for a in fixture["list"]["accounts"]],
    }
    cache = {"schema_version": 1, "accounts": {}}
    for account in fixture["usage"]["accounts"]:
        groups = [{key: group[key] for key in ("group", "model", "disabled", "five_hour", "weekly")}
                  for group in account["groups"]]
        health = {key: account["health"][key] for key in ("state", "evidence", "observed_at", "source")}
        cache["accounts"][account["id"]] = {
            "quota": {"observed_at": account["observed_at"], "groups": groups},
            "last_successful_update": account["last_successful_update"],
            "last_attempt": account["last_attempt"], "last_error": None, "health": health,
        }
    for path, value in ((root / "cfg/registry.json", registry), (root / "cache/quota.json", cache)):
        path.write_text(json.dumps(value, indent=2) + "\n")
        path.chmod(0o600)
    return {
        "HOME": str(root / "home"), "NO_COLOR": "1", "AGYVA_BACKEND": "file",
        "AGYVA_CONFIG_DIR": str(root / "cfg"), "AGYVA_STATE_DIR": str(root / "state"),
        "AGYVA_CACHE_DIR": str(root / "cache"), "AGYVA_AGY_HOME": str(root / "agyhome"),
        "AGYVA_AGY_BIN": str(root / "fakeagy/agy"),
    }, fixture


def svg_frame(frame, caption):
    cell_w, line_h, pad = 8.5, 21, 24
    title_h = 42
    notes = textwrap.wrap(caption, 105)
    width = int(frame["width"] * cell_w + 2 * pad)
    height = title_h + frame["height"] * line_h + len(notes) * 18 + pad * 2
    parts = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-labelledby="title desc">',
             '<title id="title">Agyva: actual terminal rendering of sanitized cached accounts</title>',
             f'<desc id="desc">{html.escape(caption)}</desc>',
             '<style>text{font-family:"DejaVu Sans Mono",monospace;font-size:14px;fill:#e1e6eb;white-space:pre}.label{font-family:system-ui,sans-serif;font-size:12px;fill:#c0cbd5}.selected{fill:#101418}</style>',
             f'<rect width="{width}" height="{height}" fill="#101418"/>',
             f'<text class="label" x="{pad}" y="25">{html.escape(frame["name"])} · cache-only replay</text>']
    for y, row in enumerate(frame["rows"]):
        for cell in row:
            x = pad + cell["x"] * cell_w
            top = title_h + y * line_h
            if cell["reverse"]:
                parts.append(f'<rect x="{x}" y="{top}" width="{cell["columns"] * cell_w}" height="{line_h}" fill="#e1e6eb"/>')
            if cell["text"].strip():
                attrs = (' class="selected"' if cell["reverse"] else '') + (' font-weight="bold"' if cell["bold"] else '')
                parts.append(f'<text x="{x}" y="{top + 16}"{attrs} xml:space="preserve">{html.escape(cell["text"])}</text>')
    for index, note in enumerate(notes):
        y = title_h + frame["height"] * line_h + pad + index * 18
        parts.append(f'<text class="label" x="{pad}" y="{y}">{html.escape(note)}</text>')
    return "\n".join(parts + ["</svg>", ""])


def main():
    mode, output = sys.argv[1:3]
    out = Path(output)
    svg = out.with_name(out.stem.removesuffix("-capture") + ".svg")
    binary = ROOT / "target/debug/agyva"
    with tempfile.TemporaryDirectory(prefix="agyva-capture-") as temp:
        root = Path(temp)
        env = {"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "TERM": "xterm-256color"}
        fixture = None
        if mode == "snapshot":
            additions, fixture = replay(root)
            env.update(additions)
            caption = PROVENANCE
        else:
            setup = subprocess.check_output([str(ROOT / "scripts/demo-sandbox.sh"), str(root / "demo")],
                env=env | {"AGYVA_BIN": str(binary)}, text=True)
            for line in setup.splitlines():
                if line.startswith("export "):
                    name, value = shlex.split(line)[1].split("=", 1)
                    env[name] = value
            caption = "SYNTHETIC: fake accounts, fake agy, and made-up quota values. No real account data."
        before = (Path(env["AGYVA_CACHE_DIR"]) / "quota.json").read_bytes()
        usage = subprocess.check_output([str(binary), "usage", "--all", "--no-refresh", "--json"], env=env, text=True)
        if fixture is not None:
            actual = json.loads(usage)
            for expected, account in zip(fixture["usage"]["accounts"], actual["accounts"], strict=True):
                for name in ("id", "alias", "email", "plan", "observed_at", "last_successful_update", "last_attempt", "groups", "health", "unavailable"):
                    assert account[name] == expected[name], f"replay changed {name}"
        env["AGYVA_CAPTURE"] = "1"
        rendered = subprocess.check_output([str(ROOT / "target/debug/examples/capture-tui")], env=env, text=True)
        frames = json.loads(rendered)
        assert before == (Path(env["AGYVA_CACHE_DIR"]) / "quota.json").read_bytes(), "capture mutated quota cache"
        if fixture is not None:
            assert not (Path(env["AGYVA_AGY_BIN"]).parent / "usage.calls").exists()
        blocks = [caption, "", "Production views rendered with Ratatui TestBackend in an isolated scratch registry/cache.",
                  "Snapshot replay needs no credentials or vault; synthetic mode uses fake credentials. Capture jobs cannot switch or refresh.",
                  "Relative ages reflect generation time; original observation and reset timestamps remain in capture-data/accounts.json.",
                  "", "--- agyva usage --all --no-refresh --json ---", usage.rstrip()]
        for frame in frames:
            blocks += ["", f'--- {frame["name"]} ---']
            blocks += ["".join(cell["text"] for cell in row).rstrip() for row in frame["rows"]]
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text("\n".join(blocks) + "\n")
        # Two production frames retain both the compact overview and per-group values.
        overview = frames[0]
        svg.write_text(svg_frame(overview, caption))
        usage_svg = svg.with_name(svg.stem + "-usage.svg")
        usage_svg.write_text(svg_frame(frames[1], caption))
        print(f"wrote {out}, {svg} and {usage_svg}; replay fields match; quota cache unchanged")


if __name__ == "__main__":
    main()
