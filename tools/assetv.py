"""Content-hashed version tags for the site's assets.

Stylesheets and scripts keep stable URLs, so browsers cache them across
deploys and visitors keep seeing last week's layout. Generators append
`?v=<8 hex of sha256>` to every asset reference: content changes -> URL
changes -> one cache miss; unchanged content keeps its cached URL.
"""

import hashlib
import pathlib

_ASSETS = pathlib.Path(__file__).resolve().parent.parent / "site/assets"


def v(name: str) -> str:
    """`kevy.css` -> `kevy.css?v=1a2b3c4d`. A missing asset raises: a typo
    here is a 404 in production the link gate cannot see."""
    digest = hashlib.sha256((_ASSETS / name).read_bytes()).hexdigest()[:8]
    return f"{name}?v={digest}"
