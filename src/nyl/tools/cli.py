import builtins
import sys
from typing import Any


def print_info(*msg: Any) -> None:
    builtins.print("[nyl]", *msg, file=sys.stderr)
