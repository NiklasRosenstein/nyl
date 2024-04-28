from dataclasses import dataclass, field
import json
import logging
from pathlib import Path
import subprocess
import tempfile
from textwrap import dedent
from typing import Any, Literal
from nyl.pipeline import PipelineStepImpl

logger = logging.getLogger(__name__)


@dataclass
class TerraformContext:
    instance_dir: Path
    state_file: Path
    plan_file: Path

    init_mode: Literal["install", "configure"] = "configure"
    """
    Returns the mode in which the Terraform module should be initialized. The "install" mode is used to install
    dependencies of the module, but skips any backend configuration and initialization.
    """

    offline: bool = False
    """
    If enabled, the Terraform module will be initialized in offline mode. This is achieved by passing the `-plugin-dir`
    option to `terraform init` pointing to the `.terraform/plugins` directory in the instance directory. The module
    must have been initialized before, for example using the "install" `init_mode`.
    """

    destroy: bool = False
    """
    Whether to generate a destroy plan instead of a regular plan.
    """


@dataclass
class Terraform(PipelineStepImpl[TerraformContext], context_cls=TerraformContext):
    """
    Represents the configuration of a step in the pipeline that applies a Terraform module.
    """

    source: str
    version: str | None = None
    vars: dict[str, None | bool | int | float | str | dict[str, Any] | list[Any]] = field(default_factory=dict)

    def init(self, ctx: TerraformContext) -> None:
        """
        Initialize the Terraform module.
        """

        init_args: list[str] = ["-lockfile=readonly", "-input=false"]

        if self.source.startswith("http") or self.source.startswith("git") or self.source.startswith("ssh"):
            # TODO: Support Git ref in the source URL.
            # Clone the source to the instance directory.
            if (ctx.instance_dir / ".git").is_dir():
                # TODO: Verify that the repository is the same.
                # Pull the latest changes.
                logger.info("Pulling the latest changes from the repository. (dest=%s)", ctx.instance_dir)
                subprocess.run(["git", "pull"], cwd=ctx.instance_dir, check=True)
            else:
                # Clone the repository.
                logger.info("Cloning the repository. (source=%s, dest=%s)", self.source, ctx.instance_dir)
                subprocess.run(["git", "clone", self.source, ctx.instance_dir], check=True)
        else:
            # TODO: Remove files that are not in the source directory / consider symlinking files (not dirs).
            # Copy the source to the instance directory.
            logger.info("Copying the source directory. (source=%s, dest=%s)", self.source, ctx.instance_dir)
            subprocess.run(["cp", "-r", self.source, ctx.instance_dir], check=True)

        if ctx.init_mode == "configure":
            # Create the override file for the backend.
            logger.info("Configuring the backend. (state_file=%s)", ctx.state_file)
            with open(ctx.instance_dir / "backend_override.tf", "w") as f:
                config = f"""
                    terraform {{
                        backend "local" {{
                            path = "{ctx.state_file.absolute()}"
                        }}
                    }}
                """
                f.write(dedent(config))

            init_args += ["-backend=true"]

        else:
            init_args += ["-backend=false"]

        if ctx.offline:
            init_args += ["-plugin-dir", ctx.instance_dir / ".terraform/plugins"]

        # Initialize the Terraform module.
        subprocess.run(["terraform", "init", *init_args], cwd=ctx.instance_dir, check=True)

    def plan(self, ctx: TerraformContext) -> None:
        """
        Plan the changes to the Terraform module.

        Args:
            _destroy: Internal. Run a destroy plan instead.
        """

        with tempfile.TemporaryDirectory() as tmpdir:
            vars_file = Path(tmpdir) / "terraform.tfvars"
            with vars_file.open("w") as vars_fp:
                for key, value in self.vars.items():
                    print(f"{key} = {json.dumps(value)}", file=vars_fp)

            ctx.plan_file.parent.mkdir(exist_ok=True, parents=True)
            subprocess.run(
                [
                    "terraform",
                    "plan",
                    "-input=false",
                    f"-out={ctx.plan_file.absolute()}",
                    "-compact-warnings",
                    f"-var-file={vars_file}",
                    *(["-destroy"] if ctx.destroy else []),
                ],
                cwd=ctx.get_instance_dir(),
                check=True,
            )

    def apply(self, ctx: TerraformContext) -> None:
        """
        Apply the changes to the Terraform module.
        """

        subprocess.run(
            ["terraform", "apply", "-input=false", ctx.plan_file.absolute(), "-auto-approve", "-compact-warnings"],
            cwd=ctx.instance_dir,
            check=True,
        )

    def get_outputs(self, ctx: TerraformContext) -> dict[str, Any]:
        """
        Get the outputs of the Terraform module.
        """

        outputs = subprocess.run(
            ["terraform", "output", "-json"],
            cwd=ctx.instance_dir,
            capture_output=True,
            check=True,
        )
        return json.loads(outputs.stdout)
