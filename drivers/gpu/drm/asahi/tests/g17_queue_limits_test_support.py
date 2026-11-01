"""Compile the actual queue-limit contract with its production depth constant."""
from pathlib import Path
import re

def queue_limits_source():
    root = Path(__file__).resolve().parents[1]
    limits = (root / 'g17_queue_limits.rs').read_text().replace('//!', '//')
    resources = (root / 'g17_resources.rs').read_text()
    depth = re.search(r'pub\(crate\) const G17P_RENDER_MAX_IN_FLIGHT: [^;]+;', resources)[0]
    return depth + '\n' + limits.replace(
        'crate::g17_resources::G17P_RENDER_MAX_IN_FLIGHT', 'G17P_RENDER_MAX_IN_FLIGHT')
