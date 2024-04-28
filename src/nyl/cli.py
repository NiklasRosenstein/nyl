"""
Nyl facilitates the orchestration of infrastructure and application deployment pipelines across different tools,
making them work together in a seamless manner.
"""

from pathlib import Path
from typer import Option, Typer
from nyl import Nyl
import nr.proxy

app = Typer(
    help=__doc__,
    no_args_is_help=True,
    pretty_exceptions_enable=False,
    rich_markup_mode=True,
)

nyl: Nyl = nr.proxy.proxy()

@app.callback()
def callback(
    working_dir: Path = Option(None, help="The working directory that contains the pipeline definition."),
) -> None:
    nr.proxy.set_value(nyl, Nyl(working_dir=working_dir))

@app.command("init")
def init(
    install_only: bool = Option(
        False, help="Only install dependencies of components (if any), do not configure backends."
    ),
) -> None:
    """Initalize all components of a Nyl pipeline, such as Terraform modules."""

    pipeline = nyl.load_pipeline()
    for step in pipeline.steps.values():
        # TODO: How to obtain the context of the step
        step.impl.init(step.context)


@app.command("run")
def run() -> None:
    pass
