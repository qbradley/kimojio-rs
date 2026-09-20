"""Authorized 80-cell refresh on the same frozen candidate, without rebuilding."""
import importlib.util
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
BASE = HERE.parent / "measured"
OUT = HERE / "full-refresh"
OUT.mkdir(exist_ok=True)
action = sys.argv[1]
name = "summarize.py" if action == "summary" else "run.py"
sys.path.append(str(BASE))
spec = importlib.util.spec_from_file_location("full_" + name.replace(".", "_"), BASE / name)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.HERE = OUT
if action != "summary":
    assert not (OUT / (action + ".jsonl")).exists()
    assert not (OUT / (action + ".jsonl.gz")).exists()
    module.BINARY = Path(json.loads((HERE / "freeze.json").read_text())["binaries"]["runtime"]["path"])
module.main()
