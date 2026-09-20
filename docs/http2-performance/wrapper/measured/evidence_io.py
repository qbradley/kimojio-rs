"""Read plain or losslessly compressed evidence."""
import gzip
from pathlib import Path


def read_text(path):
    path = Path(path)
    if not path.exists():
        path = Path(str(path) + ".gz")
    if path.suffix == ".gz":
        with gzip.open(path, "rt") as stream:
            return stream.read()
    return path.read_text()
