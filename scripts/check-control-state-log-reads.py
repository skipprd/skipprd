#!/usr/bin/env python3
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]

CHECK_PATHS = [
    ROOT / "react/suites/react-suites/src/data_engineer/control_flow.rs",
    ROOT / "react/suites/react-suites/src/data_engineer/progress_controller.rs",
    ROOT / "react/suites/react-suites/src/data_engineer/transition_dispatcher.rs",
]

FORBIDDEN_SNIPPETS = [
    "thread_store.get(",
    "store.get(thread_id)",
]

ALLOWLIST_LINE_FRAGMENTS = [
    '#[cfg(test)]',
    'mod tests',
    'expect("thread log',
]

def is_allowlisted(context: str) -> bool:
    return any(token in context for token in ALLOWLIST_LINE_FRAGMENTS)

def main() -> int:
    violations = []
    for path in CHECK_PATHS:
        if not path.exists():
            continue
        lines = path.read_text(encoding="utf-8").splitlines()
        for i, line in enumerate(lines, start=1):
            if any(snippet in line for snippet in FORBIDDEN_SNIPPETS):
                window_start = max(0, i - 6)
                window_end = min(len(lines), i + 5)
                context = "\n".join(lines[window_start:window_end])
                if is_allowlisted(context):
                    continue
                violations.append(f"{path.relative_to(ROOT)}:{i}: {line.strip()}")

    if violations:
        print("Control-state thread-log read guard failed. Violations:")
        for v in violations:
            print(f" - {v}")
        return 1

    print("Control-state thread-log read guard passed.")
    return 0

if __name__ == "__main__":
    sys.exit(main())

