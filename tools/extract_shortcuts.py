"""Extract Trilium default keyboard actions (non-Mac branch) from keyboard_actions.ts to JSON."""
import json, re, sys
src = open(sys.argv[1], encoding="utf-8").read()
start = src.index("const DEFAULT_KEYBOARD_ACTIONS")
body = src[start:src.index("return DEFAULT_KEYBOARD_ACTIONS", start)]
out, section = [], None
for blk in re.split(r"\n        \{", body)[1:]:
    blk = blk.split("\n        }")[0]
    m = re.search(r't\("keyboard_actions\.([\w-]+)"\)', blk)
    if "separator:" in blk:
        section = m.group(1) if m else None
        continue
    name = re.search(r'actionName:\s*"(\w+)"', blk)
    if not name:
        continue
    ds = re.search(r"defaultShortcuts:\s*(.*?),\n\s+\w+:", blk + "\n x:", re.S)
    raw = ds.group(1) if ds else "[]"
    tern = re.match(r"isMac\(\)\s*\?\s*(\[.*?\])\s*:\s*(\[.*\])", raw, re.S)
    arr = tern.group(2) if tern else raw
    mac = tern.group(1) if tern else None
    keys = re.findall(r'"([^"]+)"', arr)
    scope = re.search(r'scope:\s*"(\w+)"', blk)
    out.append({"action": name.group(1), "section": section, "keys": keys,
                "mac_keys": re.findall(r'"([^"]+)"', mac) if mac else None,
                "scope": scope.group(1) if scope else None})
json.dump(out, open(sys.argv[2], "w"), indent=1)
print(len(out), "actions;", sum(1 for a in out if a["keys"]), "with default keys")
