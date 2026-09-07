import { defineConfig } from "astro/config";
import { unified } from "@astrojs/markdown-remark";
import starlight from "@astrojs/starlight";
import { resourceSidebar, resourceRedirects } from "./src/lib/resources.mjs";

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
  redirects: Object.fromEntries(Object.entries(resourceRedirects).map(([from, to]) => [from, `${basePath}${to}`])),
  markdown: {
    processor: unified({ remarkPlugins: [rewriteNylLinks] }),
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
                "commands/create",
                "commands/project-resources",
                "commands/validate",
                "commands/render",
                "commands/diff",
                "commands/apply",
                "commands/release",
                "commands/schema",
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
            "reference/resources/gitops",
            { label: "Resource usage guides", collapsed: true, items: [{ autogenerate: { directory: "deployment-workflows/rendered-manifests/resource-guides" } }] },
          ],
        },
        {
          label: "Reference",
          items: [
            { label: "Resources", link: "/reference/resources/" },
            ...resourceSidebar(),
            { label: "Rendering usage guides", collapsed: true, items: [{ autogenerate: { directory: "components/resource-guides" } }] },
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
