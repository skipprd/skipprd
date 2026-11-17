"""Hermetic as-of site lookup. No network."""

REVISIONS = [
    {"device_id": "dev-1", "valid_from": 0, "site": "north"},
    {"device_id": "dev-1", "valid_from": 100, "site": "south"},
    {"asset_id": "asset-9", "valid_from": 0, "site": "east"},
]


def lookup(device_id=None, asset_id=None, at=None):
    t = 0 if at is None else int(at)
    best = None
    best_from = None
    for row in REVISIONS:
        vf = int(row["valid_from"])
        if vf > t:
            continue
        hit = False
        if device_id is not None and row.get("device_id") == device_id:
            hit = True
        elif asset_id is not None and row.get("asset_id") == asset_id:
            hit = True
        if not hit:
            continue
        if best_from is None or vf >= best_from:
            best = row["site"]
            best_from = vf
    return best
