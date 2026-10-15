#!/usr/bin/env python3
"""Parsimony gate: product code per crate stays within a checked-in budget, and the tree holds no
committed evidence. Growth is allowed only by editing tools/size-budget.txt in the same change, so
it is a decision a reviewer sees, not drift. The budget also ratchets down: one more than
SLACK above the measured size fails, so freed room is not silently refilled.

Product lines: non-blank lines of crates/<crate>/src/**/*.rs outside `#[cfg(test)]` items (a test
module, import or helper is skipped to its matching brace or `;`, with strings, raw strings, char
literals and comments masked), excluding tests.rs files, `tests` directories and files declared
only as `#[cfg(test)] mod` (including `#[path]` ones).
Run from the repository root: python3 -I tools/check-size.py [--write]"""

import pathlib
import re
import subprocess
import sys

BUDGET = pathlib.Path("tools/size-budget.txt")
SLACK = 0.05
DOC_MAX_LINES = 250
CFG_TEST = re.compile(r"#\[(cfg\((test|all\(test\b[^\]]*)\)|test)\]")  # cfg(test) items and #[test] fns
MOD_DECL = re.compile(r"\bmod\s+(\w+)\s*;")
PATH_ATTR = re.compile(r'#\[path\s*=\s*"([^"]+)"\]')


def code_mask(text: str) -> list[bool]:
    """Per character: True where it is code (not inside a comment, string or char literal)."""
    mask = [True] * len(text)
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
        elif (m := re.match(r'b?r(#*)"', text[i:])) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            close = '"' + m.group(1)
            j = text.find(close, i + m.end())
            j = n if j < 0 else j + len(close)
        elif c == '"' or text.startswith('b"', i) and (i == 0 or not text[i - 1].isalnum()):
            j = i + (2 if c == "b" else 1)
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            j += 1
        elif c == "'" and (m := re.match(r"'(\\.[^']*|[^\\'])'", text[i:])):
            j = i + m.end()  # a char literal; a lifetime has no closing quote and stays code
        else:
            i += 1
            continue
        for k in range(i, min(j, n)):
            mask[k] = False
        i = j
    return mask


def item_end(text: str, mask: list[bool], start: int) -> int:
    """End of the item, field or variant that begins at `start`: its matching brace, or its `;`/`,`."""
    depth, i = 0, start
    while i < len(text):
        if mask[i]:
            ch = text[i]
            if ch in "{([":
                depth += 1
            elif ch in "})]":
                if depth == 0:
                    return i  # the enclosing item closes: a field or variant ends here
                depth -= 1
                if depth == 0 and ch == "}":
                    return i + 1
            elif ch in ";," and depth == 0:
                return i + 1  # an item, field, variant or argument
        i += 1
    return len(text)


def strip_test_items(path: pathlib.Path, excluded: set[pathlib.Path]) -> str:
    """The file's text without `#[cfg(test)]` items; test-only module files go into `excluded`."""
    text = path.read_text(encoding="utf-8", errors="replace")
    mask = code_mask(text)
    out, pos = [], 0
    for m in CFG_TEST.finditer(text):
        if m.start() < pos or not mask[m.start()]:
            continue
        end = item_end(text, mask, m.end())
        item = text[m.end():end]
        if decl := MOD_DECL.search(item):
            attr = PATH_ATTR.search(item)
            base = path.parent if path.name in ("mod.rs", "lib.rs", "main.rs") else path.with_suffix("")
            if attr:
                excluded.add((path.parent / attr.group(1)).resolve())
            else:
                excluded.add((base / f"{decl.group(1)}.rs").resolve())
                excluded.add((base / decl.group(1) / "mod.rs").resolve())
        out.append(text[pos:m.start()])
        pos = end
    out.append(text[pos:])
    return "".join(out)


def product_lines(crate: pathlib.Path) -> int:
    files = sorted((crate / "src").rglob("*.rs"))
    excluded: set[pathlib.Path] = set()
    stripped = {path: strip_test_items(path, excluded) for path in files}
    total = 0
    for path in files:
        if path.name == "tests.rs" or "tests" in path.relative_to(crate / "src").parts[:-1]:
            continue
        if path.resolve() in excluded:
            continue
        total += sum(1 for line in stripped[path].splitlines() if line.strip())
    return total


def measure() -> dict[str, int]:
    return {
        crate.name: product_lines(crate)
        for crate in sorted(pathlib.Path("crates").iterdir())
        if (crate / "src").is_dir()
    }


def tracked(*patterns: str) -> list[str]:
    out = subprocess.run(["git", "ls-files", "--", *patterns], capture_output=True, text=True, check=True)
    return [line for line in out.stdout.splitlines() if line]


def main() -> int:
    sizes = measure()
    if "--write" in sys.argv:
        BUDGET.write_text(
            "# Product lines per crate (tools/check-size.py). Raise a number only as a reviewed decision.\n"
            + "".join(f"{name} {lines}\n" for name, lines in sizes.items())
        )
        print(f"wrote {BUDGET}")
        return 0

    failures = []
    budget = {}
    for line in BUDGET.read_text().splitlines():
        if line.strip() and not line.startswith("#"):
            name, lines = line.split()
            budget[name] = int(lines)
    for name, lines in sizes.items():
        limit = budget.get(name)
        if limit is None:
            failures.append(f"{name}: {lines} product lines and no budget (add it to {BUDGET})")
        elif lines > limit:
            failures.append(f"{name}: {lines} product lines exceed the budget of {limit} by {lines - limit}")
        elif lines < limit / (1 + SLACK):
            failures.append(f"{name}: budget {limit} is more than {SLACK:.0%} above the {lines} measured; lower it")
        print(f"{name:12} {lines:7} / {limit if limit is not None else '-'}")
    for name in budget.keys() - sizes.keys():
        failures.append(f"{name}: budgeted but no such crate; remove it from {BUDGET}")

    for path in tracked("docs/*.json", "docs/**/*.json", "tools/profiling/results/*"):
        failures.append(f"{path}: measurements belong in commit messages, not committed files")
    for path in tracked("docs/design/*.md"):
        n = len(pathlib.Path(path).read_text().splitlines())
        if n > DOC_MAX_LINES:
            failures.append(f"{path}: {n} lines; design docs stay under {DOC_MAX_LINES} (history lives in git)")

    for failure in failures:
        print(f"FAIL {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
