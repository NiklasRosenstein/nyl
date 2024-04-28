from nyl.pipeline import Pipeline
from nyl.contrib.terraform import Terraform

pipeline = Pipeline()
mod1 = pipeline.add("mod1", Terraform(source=f"{pipeline.dir}/mod1"))
mod2 = pipeline.add(
    "mod2", Terraform(source=f"{pipeline.dir}/mod2", vars={"private_key_pem": mod1.output("private_key_pem")})
)
