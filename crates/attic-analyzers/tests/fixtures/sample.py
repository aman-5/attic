"""Sample Python fixture.

Code-like text in docstring: def fake(): pass / class AlsoFake: ...
"""
import os
import os.path as osp
from collections import OrderedDict
from ..shared import constants as C

MAX_ITEMS = 100


class Inventory(Base):
    """Inventory of parts."""

    kind = "inventory"

    def __init__(self, items):
        self.items = list(items)

    @property
    def size(self):
        return len(self.items)

    async def refresh(self, source):
        data = await fetch(source)
        self.items = data or []


def top_level(a, b=2, *args, **kwargs):
    """Top level function with nesting."""

    def nested():
        return a

    return nested()


helper = lambda q: q + 1
