"""Release notes in Portuguese for the in-app update log.

Collects the pull requests squash-merged between the previous release tag and the commit being
released, reads each one's "## Notas da versão" bullets (falling back to its title), and groups
them by Conventional Commit type into the `### Section` + `- item` shape the updater shows.

Usage: python tools/release-notes.py <repo> <previous-tag> <target-sha> > notes.md
Needs an authenticated `gh` (GH_TOKEN in Actions).
"""

import json
import re
import subprocess
import sys

SECTIONS = [
    ("feat", "Novidades"),
    ("fix", "Correções"),
    ("perf", "Desempenho"),
]
OTHER = "Outras mudanças"
SKIPPED = {"chore", "ci", "docs", "style", "test", "build", "refactor"}
MAX_ITEMS_PER_PR = 8
MAX_ITEM = 300


def gh(*args):
    return json.loads(subprocess.run(["gh", *args], check=True, capture_output=True, text=True, encoding="utf-8").stdout)


def notes_section(body):
    """Bullets under `## Notas da versão`, without HTML comments; empty when absent."""
    body = re.sub(r"<!--.*?-->", "", body or "", flags=re.S)
    match = re.search(r"^##\s+Notas da versão\s*$(.*?)(?=^##\s|\Z)", body, flags=re.M | re.S)
    if not match:
        return []
    items = []
    for line in match.group(1).splitlines():
        line = line.strip()
        if line[:2] in ("- ", "* ") and line[2:].strip():
            items.append(line[2:].strip()[:MAX_ITEM])
    return items[:MAX_ITEMS_PER_PR]


def kind(title):
    match = re.match(r"^(\w+)(\([^)]*\))?(!)?:", title)
    return match.group(1).lower() if match else ""


def main():
    repo, previous, target = sys.argv[1:4]
    compare = gh("api", f"repos/{repo}/compare/{previous}...{target}")
    numbers = []
    for commit in compare.get("commits", []):
        subject = commit["commit"]["message"].splitlines()[0]
        found = re.search(r"\(#(\d+)\)\s*$", subject)
        if found and found.group(1) not in numbers:
            numbers.append(found.group(1))

    grouped = {}
    for number in numbers:
        pr = gh("pr", "view", number, "-R", repo, "--json", "title,body")
        title = pr["title"].strip()
        category = kind(title)
        items = notes_section(pr["body"])
        if not items:
            if category in SKIPPED:
                continue
            # No user-facing notes: show the title without its type prefix.
            items = [re.sub(r"^\w+(\([^)]*\))?!?:\s*", "", title)[:MAX_ITEM]]
        heading = dict(SECTIONS).get(category, OTHER)
        grouped.setdefault(heading, []).extend(f"{item} (#{number})" for item in items)

    out = []
    for heading in [name for _, name in SECTIONS] + [OTHER]:
        if grouped.get(heading):
            out.append(f"### {heading}\n")
            out.extend(f"- {item}" for item in grouped[heading])
            out.append("")
    if not out:
        out = ["### Outras mudanças\n", "- Ajustes internos.", ""]
    out.append(f"**Todas as mudanças**: https://github.com/{repo}/compare/{previous}...{target[:12]}")
    sys.stdout.reconfigure(encoding="utf-8")
    print("\n".join(out))


if __name__ == "__main__":
    main()
