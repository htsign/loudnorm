"""Compare two SemVer versions: prints "up", "down" or "eq" for BASE -> HEAD.

Usage: python3 semver_cmp.py BASE HEAD
Pre-releases rank below their release (1.0.0-rc.1 < 1.0.0); numeric identifiers compare numerically
and rank below alphanumeric ones; build metadata (+...) is ignored, as in SemVer 2.0.0.
"""
import re
import sys

PATTERN = re.compile(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z.-]+)?")


def key(v):
    m = PATTERN.fullmatch(v)
    if not m:
        sys.exit(f"not a SemVer version: {v!r}")
    core = tuple(int(x) for x in m.group(1, 2, 3))
    pre = m.group(4)
    if pre is None:
        return (core, 1, ())
    ids = tuple((0, int(p), "") if p.isdigit() else (1, 0, p) for p in pre.split("."))
    return (core, 0, ids)


def compare(base, head):
    b, h = key(base), key(head)
    return "eq" if b == h else ("up" if h > b else "down")


if __name__ == "__main__":
    print(compare(sys.argv[1], sys.argv[2]))
