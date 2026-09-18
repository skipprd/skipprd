import pathlib
import re

ROOT = pathlib.Path("/Users/huders2000/Documents/sites/skippr/skipprd")
STRUCT = re.compile(
    r"^pub struct (OtlpConfigRaw|\w+(?:PluginConfig|HttpAuthConfig)|IcebergCatalogConfig)\b"
)
# Also pub(crate) OtlpConfigRaw and Upfoundry*Config
STRUCT2 = re.compile(
    r"^(?:pub(?:\([^)]+\))? )?struct (OtlpConfigRaw|IcebergCatalogConfig|\w+PluginConfig|Upfoundry\w+Config|DataSourceHttpAuthConfig)\b"
)

changed = 0
for path in ROOT.glob("plugins/**/*.rs"):
    text = path.read_text()
    lines = text.splitlines(keepends=True)
    out = []
    i = 0
    dirty = False
    while i < len(lines):
        line = lines[i]
        if STRUCT2.search(line) and "SkipprConfig" not in "".join(lines[max(0, i - 8) : i + 1]):
            # insert derive on previous derive line if present
            j = len(out) - 1
            while j >= 0 and (out[j].strip().startswith("#[") or out[j].strip().startswith("//") or not out[j].strip()):
                if "derive(" in out[j] and "SkipprConfig" not in out[j]:
                    out[j] = out[j].replace("Deserialize", "Deserialize, SkipprConfig")
                    if "SkipprConfig" not in out[j]:
                        out[j] = out[j].replace(")]", ", SkipprConfig)]")
                    dirty = True
                    break
                j -= 1
            else:
                out.append("#[derive(SkipprConfig)]\n")
                dirty = True
            if "use skippr_runtime_sdk::SkipprConfig" not in text and "skippr_plugin_macros::SkipprConfig" not in text:
                # add use after first use block later
                pass
        out.append(line)
        i += 1
    if dirty:
        new = "".join(out)
        if "SkipprConfig" in new and "use skippr_runtime_sdk::SkipprConfig" not in new and "skippr_plugin_macros::SkipprConfig" not in new:
            new = "use skippr_runtime_sdk::SkipprConfig;\n" + new
        path.write_text(new)
        changed += 1
        print(path.relative_to(ROOT))
print(f"derive on {changed} files")
