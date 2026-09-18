import pathlib
import re

ROOT = pathlib.Path("/Users/huders2000/Documents/sites/skippr/skipprd")

SECRET = {
    "password",
    "sasl_password",
    "connection_string",
    "private_key_pem",
    "credentials_json",
    "api_key",
    "account_key",
    "sas_token",
    "consumer_key",
    "oauth_consumer_key",
    "token",
    "developer_token",
    "motherduck_token",
    "auth_token",
    "bearer_token",
    "access_token",
    "refresh_token",
    "oauth_token",
    "oauth_client_secret",
    "oauth_refresh_token",
    "oauth_consumer_secret",
    "oauth_token_secret",
    "staging_azure_sas_token",
    "staging_azure_account_key",
    "client_secret",
}
SECRET_PATH = {
    "private_key_path",
    "credentials_path",
    "service_account_json_path",
    "service_account_key_path",
    "staging_gcs_service_account_key_path",
}

STRUCT_START = re.compile(
    r"^(pub(?:\([^)]+\))? )?struct (OtlpConfigRaw|\w+(?:PluginConfig|Config|HttpAuthConfig)|IcebergCatalogConfig)\b"
)
FIELD = re.compile(r"^(\s+)pub(?:\([^)]+\))? ([a-zA-Z0-9_]+):")


def kind_for(ident: str):
    if ident.endswith("_token_url") or ident == "oauth_token_url":
        return "not_secret"
    if ident in SECRET_PATH:
        return "secret_path"
    if ident in SECRET or ident.endswith("_secret") or ident.endswith("_token"):
        return "secret"
    if ident.endswith("_path") and any(
        p in ident for p in ("private_key", "credential", "service_account", "json")
    ):
        return "secret_path"
    return None


def process(path: pathlib.Path) -> bool:
    text = path.read_text()
    lines = text.splitlines(keepends=True)
    out = []
    changed = False
    in_struct = False
    brace = 0
    i = 0
    while i < len(lines):
        line = lines[i]
        if not in_struct and STRUCT_START.search(line):
            in_struct = True
            brace = line.count("{") - line.count("}")
            out.append(line)
            i += 1
            continue
        if in_struct:
            brace += line.count("{") - line.count("}")
            m = FIELD.match(line)
            if m:
                ident = m.group(2)
                k = kind_for(ident)
                prev = out[-1] if out else ""
                if k and f"skippr({k})" not in prev and "skippr(" not in prev:
                    out.append(f"{m.group(1)}#[skippr({k})]\n")
                    changed = True
            out.append(line)
            if brace <= 0:
                in_struct = False
            i += 1
            continue
        out.append(line)
        i += 1
    if changed:
        path.write_text("".join(out))
    return changed


changed_files = []
for path in list(ROOT.glob("plugins/**/*.rs")) + [
    ROOT / "crates/skippr-iceberg-catalog/src/lib.rs"
]:
    if process(path):
        changed_files.append(path)
print(f"annotated {len(changed_files)} files")
for p in changed_files:
    print(p.relative_to(ROOT))
