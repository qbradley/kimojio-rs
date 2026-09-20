"""Run the immutable normal-binary profile driver for each affected backend."""
from pathlib import Path
import subprocess

HERE = Path(__file__).resolve().parent
path = "docs/http2-performance/wrapper/direct-read-458b2bcd/profile.py"
source = subprocess.check_output(["git", "show", "2a65d8e4a690975564104f3a78c697448eaee8e0:" + path], text=True)
for backend in ("native", "generic"):
    program = source.replace('folder = private / version', 'folder = private / (version + "-' + backend + '")')
    program = program.replace('"generic"', '"' + backend + '"')
    program = program.replace('"generic-duplex.data"', '"' + backend + '-duplex.data"')
    program = program.replace('HERE / "profiles.json"', 'HERE / "profiles-' + backend + '.json"')
    program = program.replace('HERE / "profile-analysis.json"', 'HERE / "profile-analysis-' + backend + '.json"')
    exec(compile(program, str(HERE / ("profile-" + backend + ".py")), "exec"),
         {"__file__": str(HERE / ("profile-" + backend + ".py")), "__name__": "profile_" + backend})
