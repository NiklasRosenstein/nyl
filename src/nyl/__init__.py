from pathlib import Path
from types import ModuleType
from nyl.pipeline import Pipeline


class Nyl:
    PIPELINE_FILE = "nyl-pipeline.py"

    def __init__(self, working_dir: Path | None = None) -> None:
        self.working_dir = working_dir or Path.cwd()

    def load_pipeline(self) -> Pipeline:
        pipeline_file = self.working_dir / self.PIPELINE_FILE
        if not pipeline_file.is_file():
            raise FileNotFoundError(f"Pipeline file not found: {pipeline_file}")
        with pipeline_file.open() as f:
            module = ModuleType("nyl-pipeline")
            module.__file__ = str(pipeline_file)
            exec(compile(f.read(), pipeline_file, "exec"), vars(module))
        if not hasattr(module, "pipeline"):
            raise AttributeError(f"Pipeline not found in {pipeline_file}")
        if callable(module.pipeline):
            return module.pipeline()
        return module.pipeline
