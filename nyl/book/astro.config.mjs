import { defineConfig } from "astro/config";
import { unified } from "@astrojs/markdown-remark";
import starlight from "@astrojs/starlight";
import starlightSidebarTopics from "starlight-sidebar-topics";
import { movedPages } from "./src/lib/moved-pages.mjs";
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
  redirects: Object.fromEntries(
    Object.entries({ ...resourceRedirects, ...movedPages }).map(([from, to]) => [from, `${basePath}${to}`]),
  ),
  markdown: {
    processor: unified({ remarkPlugins: [rewriteNylLinks] }),
  },
  integrations: [
    starlight({
      title: "Nyl",
      description: "A fast Kubernetes manifest generator for rendered GitOps.",
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/NiklasRosenstein/nyl",
        },
      ],
      customCss: ["./src/styles/custom.css"],
      components: {
        Header: "./src/components/Header.astro",
        Sidebar: "./src/components/Sidebar.astro",
      },
      plugins: [
        starlightSidebarTopics([
          {
            id: "start",
            label: "Get started",
            link: "/",
            icon: "rocket",
            items: ["index", "start/introduction", "start/getting-started"],
          },
          {
            id: "kubernetes",
            label: "Kubernetes",
            link: "/kubernetes/pipeline/",
            icon: "puzzle",
            items: [
              "kubernetes/pipeline",
              {
                label: "Components",
                items: [
                  "kubernetes/components/overview",
                  "kubernetes/components/authoring-local-components",
                  "kubernetes/components/resolution-and-lookup",
                  "kubernetes/components/remote-shortcuts-and-aliases",
                  "kubernetes/components/troubleshooting",
                ],
              },
              "kubernetes/git-integration",
              "kubernetes/cli-workflows",
              {
                label: "Validation",
                items: ["kubernetes/validation/manifest-validation", "kubernetes/validation/kyverno-policies"],
              },
              { label: "Integrations", items: ["kubernetes/renovate"] },
              { label: "Resource guides", collapsed: true, items: [{ autogenerate: { directory: "kubernetes/resource-guides" } }] },
            ],
          },
          {
            id: "rendered-gitops",
            label: "Rendered GitOps",
            link: "/rendered-gitops/",
            icon: "code-branch",
            items: [
              "rendered-gitops",
              "rendered-gitops/project-structure",
              "rendered-gitops/resources",
              "rendered-gitops/targets-and-clusters",
              "rendered-gitops/release-inputs",
              "rendered-gitops/rendering-and-publishing",
              "rendered-gitops/security",
              { label: "Resource guides", collapsed: true, items: [{ autogenerate: { directory: "rendered-gitops/resource-guides" } }] },
            ],
          },
          {
            id: "reference",
            label: "Reference",
            link: "/reference/cli/",
            icon: "open-book",
            items: [
              {
                label: "CLI",
                items: [
                  "reference/cli",
                  "reference/cli/init",
                  "reference/cli/create",
                  "reference/cli/project-resources",
                  "reference/cli/validate",
                  "reference/cli/render",
                  "reference/cli/diff",
                  "reference/cli/apply",
                  "reference/cli/release",
                  "reference/cli/gitops",
                  "reference/cli/vendor",
                  "reference/cli/schema",
                  "reference/cli/comment",
                ],
              },
              {
                label: "YAML resources",
                items: [
                  { label: "All resources", link: "/reference/resources/" },
                  ...resourceSidebar(),
                ],
              },
              { label: "Configuration", items: ["reference/configuration"] },
              "reference/editor-schemas",
            ],
          },
        ]),
      ],
    }),
  ],
});
