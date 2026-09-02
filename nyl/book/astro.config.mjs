import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

const basePath = process.env.BASE_PATH ?? "/nyl";
const authoredDocsBase = "/nyl";

function rewriteNylLinks() {
  return (tree) => {
    function visit(node) {
      if (!node || typeof node !== "object") {
        return;
      }

      // Markdown sources should use production-style `/nyl/...` links. This
      // rewrites them for PR preview deployments with a deeper BASE_PATH.
      if (
        typeof node.url === "string" &&
        (node.url === authoredDocsBase || node.url.startsWith(`${authoredDocsBase}/`))
      ) {
        node.url = `${basePath}${node.url.slice(authoredDocsBase.length)}`;
      }

      if (Array.isArray(node.children)) {
        node.children.forEach(visit);
      }
    }

    visit(tree);
  };
}

export default defineConfig({
  site: "https://niklasrosenstein.github.io",
  base: basePath,
  markdown: {
    remarkPlugins: [rewriteNylLinks],
  },
  integrations: [
    starlight({
      title: "Nyl",
      description: "A fast Kubernetes manifest generator for rendered manifest GitOps and CLI workflows.",
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/NiklasRosenstein/nyl",
        },
      ],
      customCss: ["./src/styles/custom.css"],
      sidebar: [
        {
          label: "Start Here",
          items: [
            "index",
            "getting-started",
            "deployment-workflows/rendered-manifests",
            "deployment-workflows/cli-workflows",
          ],
        },
        {
          label: "User Guide",
          items: [
            "configuration",
            {
              label: "Component System",
              items: [
                "components/overview",
                "components/authoring-local-components",
                "components/resolution-and-lookup",
                "components/remote-shortcuts-and-aliases",
                "components/troubleshooting",
              ],
            },
            "git-integration",
            {
              label: "Commands",
              items: [
                "commands",
                "commands/rendering-pipeline",
                "commands/gitops",
                "commands/init",
                "commands/new",
                "commands/validate",
                "commands/render",
                "commands/diff",
                "commands/apply",
                "commands/release",
                "commands/generate",
                "commands/cluster",
                "commands/vendor",
              ],
            },
          ],
        },
        {
          label: "Rendered Manifest Pattern",
          items: [
            "deployment-workflows/rendered-manifests/project-structure",
            "deployment-workflows/rendered-manifests/targets-and-clusters",
            "deployment-workflows/rendered-manifests/rendering-and-publishing",
            "deployment-workflows/rendered-manifests/security",
            {
              label: "Reference",
              items: [
                "reference/resources/gitops",
                "reference/resources/gitops/git-repository",
                "reference/resources/gitops/cluster",
                "reference/resources/gitops/deployment-target",
                "reference/resources/gitops/app-project-definition",
                "reference/resources/gitops/application-group",
                "reference/resources/gitops/release",
              ],
            },
          ],
        },
        {
          label: "Reference",
          items: [
            "reference/resources",
            "reference/resources/component",
            "reference/resources/helmchart",
            "reference/resources/remote-manifest",
            "reference/kyverno-policies",
          ],
        },
        {
          label: "Extras",
          items: ["extras/renovate", "extras/nyl-resource-schemas"],
        },
      ],
    }),
  ],
});
