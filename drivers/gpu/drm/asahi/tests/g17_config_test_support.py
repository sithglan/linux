"""Use the production firmware configuration in standalone Rust harnesses."""
import json
from pathlib import Path


def with_g17_config(source):
    path = Path(__file__).resolve().parents[1] / "g17_config.rs"
    return source + f'\n#[path = {json.dumps(str(path))}] mod g17_config;\n'
