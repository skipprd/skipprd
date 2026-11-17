"""skipprd python compute worker. Hosted by skipprd; not a user import."""

import importlib.util
import json
import os
import sys
import traceback

_STDIN_MAX = 64 * 1024 * 1024


def main() -> None:
    raw = sys.stdin.buffer.read(_STDIN_MAX + 1)
    if len(raw) > _STDIN_MAX:
        sys.stderr.write("python compute stdin exceeds 64MiB\n")
        sys.exit(2)
    try:
        msg = json.loads(raw)
    except json.JSONDecodeError as err:
        sys.stderr.write("python compute stdin is not JSON: %s\n" % err)
        sys.exit(2)
    path = msg.get("path")
    handler = msg.get("handler")
    records = msg.get("records")
    if (
        not isinstance(path, str)
        or not path.endswith(".py")
        or not os.path.isabs(path)
        or not os.path.isfile(path)
    ):
        sys.stderr.write("python compute path must be an absolute .py file\n")
        sys.exit(2)
    if (
        not isinstance(handler, str)
        or not handler.isascii()
        or not handler.isidentifier()
    ):
        sys.stderr.write("python compute handler must be a Python identifier\n")
        sys.exit(2)
    if not isinstance(records, list):
        sys.stderr.write("python compute records must be a JSON array\n")
        sys.exit(2)
    directory = os.path.dirname(path)
    sys.path.insert(0, directory)
    spec = importlib.util.spec_from_file_location("skippr_compute", path)
    if spec is None or spec.loader is None:
        sys.stderr.write("python compute could not load %s\n" % path)
        sys.exit(2)
    mod = importlib.util.module_from_spec(spec)
    try:
        spec.loader.exec_module(mod)
    except Exception:
        sys.stderr.write(traceback.format_exc())
        sys.exit(2)
    fn = getattr(mod, handler, None)
    if not callable(fn):
        sys.stderr.write("python compute handler %s is not callable\n" % handler)
        sys.exit(2)
    results = []
    for i, record in enumerate(records):
        if not isinstance(record, dict):
            results.append({"error": "record %d is not a JSON object" % i})
            continue
        try:
            out = fn(record)
        except Exception as err:
            results.append({"error": "%s: %s" % (type(err).__name__, err)})
            continue
        if not isinstance(out, dict):
            results.append({"error": "handler must return a JSON object"})
            continue
        results.append({"record": out})
    json.dump({"results": results}, sys.stdout, separators=(",", ":"))
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
