import { defineConfig } from "astro/config";
import { unified } from "@astrojs/markdown-remark";
import starlight from "@astrojs/starlight";
import starlightSidebarTopics from "starlight-sidebar-topics";
import { consoleOutput } from "./src/lib/console-output.mjs";
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
      expressiveCode: { plugins: [consoleOutput()] },
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
                label: "Concepts",
                items: [
                  {
                    label: "Components",
                    items: [
                      "kubernetes/components/overview",
                      "kubernetes/components/authoring-local-components",
                      "kubernetes/components/value-imports",
                      "kubernetes/components/resolution-and-lookup",
                      "kubernetes/components/remote-shortcuts-and-aliases",
                      "kubernetes/components/troubleshooting",
                    ],
                  },
                  "kubernetes/git-integration",
                ],
              },
              {
                label: "Workflow",
                items: [
                  "kubernetes/cli-workflows",
                  "kubernetes/validation/manifest-validation",
                  "kubernetes/validation/kyverno-policies",
                  "kubernetes/renovate",
                ],
              },
              { label: "Resources", items: [{ autogenerate: { directory: "kubernetes/resource-guides" } }] },
            ],
          },
          {
            id: "rendered-gitops",
            label: "Rendered GitOps",
            link: "/rendered-gitops/",
            icon: "code-branch",
            items: [
              "rendered-gitops",
              {
                label: "Concepts",
                items: [
                  "rendered-gitops/project-structure",
                  "rendered-gitops/targets-and-clusters",
                  "rendered-gitops/release-inputs",
                  "rendered-gitops/rendered-layout",
                ],
              },
              {
                label: "Workflow",
                items: [
                  "rendered-gitops/workflow/render",
                  "rendered-gitops/workflow/diff",
                  "rendered-gitops/workflow/publish",
                  "rendered-gitops/workflow/move-publication",
                ],
              },
              {
                label: "Resources",
                items: [
                  { slug: "rendered-gitops/resources", label: "Overview" },
                  { autogenerate: { directory: "rendered-gitops/resource-guides" } },
                ],
              },
              "rendered-gitops/security",
            ],
          },
          {
            id: "orchestration",
            label: "Orchestration",
            link: "/orchestration/",
            icon: "setting",
            badge: { text: "M3", variant: "caution" },
            items: [
              "orchestration",
              "orchestration/environments-and-state",
              "orchestration/runs",
              "orchestration/recovery",
              "orchestration/deletion",
              { label: "Resources", items: [{ autogenerate: { directory: "orchestration/resource-guides" } }] },
              "orchestration/roadmap",
            ],
          },
          {
            id: "domain-resources",
            label: "Domain resources",
            link: "/domain-resources/",
            icon: "list-format",
            badge: { text: "Planned", variant: "note" },
            items: ["domain-resources", "domain-resources/example", "domain-resources/roadmap"],
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
                  "reference/cli/validate",
                  "reference/cli/schema",
                  "reference/cli/project-resources",
                  "reference/cli/vendor",
                  "reference/cli/render",
                  "reference/cli/diff",
                  "reference/cli/apply",
                  "reference/cli/release",
                  "reference/cli/render-tree",
                  "reference/cli/diff-tree",
                  "reference/cli/publish-tree",
                  "reference/cli/comment",
                  "reference/cli/plan",
                  "reference/cli/reconcile",
                  "reference/cli/status",
                  "reference/cli/verify",
                  "reference/cli/teardown",
                  "reference/cli/hold",
                  "reference/cli/recover",
                  "reference/cli/state",
                ],
              },
              {
                label: "YAML resources",
                items: [{ label: "All resources", link: "/reference/resources/" }, ...resourceSidebar()],
              },
              { label: "Configuration", items: ["reference/configuration"] },
              "reference/git-storage",
              "reference/editor-schemas",
              "reference/deprecations",
            ],
          },
        ]),
      ],
    }),
  ],
});
