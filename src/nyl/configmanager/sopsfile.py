from dataclasses import dataclass, field
import json
from pathlib import Path
import subprocess
from typing import Any, Literal, Mapping


@dataclass
class SopsFile:
    name: str
    path: Path
    kind: Literal["SopsFile"] = "SopsFile"
    _cache: dict[str, Any] | None = field(default=None, init=False)

    def _load(self) -> None:
        if self._cache is None:
            self._cache = json.loads(
                subprocess.run(
                    ["sops", "--output-type", "json", "--decrypt", str(self.path)],
                    capture_output=True,
                    text=True,
                    check=True,
                ).stdout
            )

    def get_lookup_resolver(self) -> Mapping[str, Any]:
        self._load()
        assert self._cache is not None
        return self._cache
