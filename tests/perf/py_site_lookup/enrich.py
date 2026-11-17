from lookup import lookup


def transform(record):
    detail = record.get("detail")
    if not isinstance(detail, dict):
        detail = {}
        record["detail"] = detail
    if detail.get("site"):
        record["site"] = detail["site"]
        return record
    truck = record.get("truck")
    if isinstance(truck, dict) and truck.get("site"):
        detail["site"] = truck["site"]
        record["site"] = truck["site"]
        return record
    device_id = record.get("device_id")
    asset_id = truck.get("id") if isinstance(truck, dict) else None
    site = lookup(
        device_id=device_id,
        asset_id=asset_id,
        at=record.get("event_time"),
    )
    if site:
        detail["site"] = site
        record["site"] = site
    return record
