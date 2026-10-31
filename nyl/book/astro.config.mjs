import { defineConfig } from "astro/config";
import { unified } from "@astrojs/markdown-remark";
import starlight from "@astrojs/starlight";
import starlightSidebarTopics from "starlight-sidebar-topics";
import { consoleOutput } from "./src/lib/console-output.mjs";
import { resourceSidebar } from "./src/lib/resources.mjs";
import { layers } from "./src/lib/layers.mjs";

const basePath = process.env.BASE_PATH ?? "/nyl";
const authoredDocsBase = "/nyl";

// A layer's tab label and status badge come from the one list of layers.
const layer = (id) => layers.find((candidate) => candidate.id === id);
const layerLabel = (id) => layer(id).name;
const layerBadge = (id) => {
  const { status, tone } = layer(id);
  return status ? { text: status, variant: tone === "amber" ? "caution" : "default" } : undefined;
};

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
    processor: unified({ remarkPlugins: [rewriteNylLinks] }),
  },
  integrations: [
    starlight({
      title: "Nyl",
      description: "Nyl turns configuration in Git into deployed systems.",
      head: [
        { tag: "link", attrs: { rel: "preconnect", href: "https://fonts.googleapis.com" } },
        { tag: "link", attrs: { rel: "preconnect", href: "https://fonts.gstatic.com", crossorigin: "" } },
        {
          tag: "link",
          attrs: {
            rel: "stylesheet",
            href: "https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500&display=swap",
          },
        },
      ],
      logo: { src: "./src/assets/logo.svg" },
      expressiveCode: {
        plugins: [consoleOutput()],
        styleOverrides: { borderRadius: "0.625rem", borderColor: "var(--sl-color-gray-5)" },
      },
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
        PageTitle: "./src/components/PageTitle.astro",
        Sidebar: "./src/components/Sidebar.astro",
      },
      plugins: [
        starlightSidebarTopics([
          {
            id: "start",
            label: "Get started",
            link: "/start/getting-started/",
            icon: "rocket",
            items: ["index", "start/introduction", "start/getting-started"],
          },
          {
            id: "kubernetes-rendering",
            label: layerLabel("kubernetes-rendering"),
            link: "/kubernetes-rendering/",
            icon: "puzzle",
            items: [
              "kubernetes-rendering",
              "kubernetes-rendering/pipeline",
              {
                label: "Concepts",
                items: [
                  {
                    label: "Components",
                    items: [
                      "kubernetes-rendering/components/overview",
                      "kubernetes-rendering/components/authoring-local-components",
                      "kubernetes-rendering/components/value-imports",
                      "kubernetes-rendering/components/resolution-and-lookup",
                      "kubernetes-rendering/components/remote-shortcuts-and-aliases",
                      "kubernetes-rendering/components/troubleshooting",
                    ],
                  },
                  "kubernetes-rendering/git-integration",
                ],
              },
              {
                label: "Workflow",
                items: [
                  "kubernetes-rendering/cli-workflows",
                  "kubernetes-rendering/validation/manifest-validation",
                  "kubernetes-rendering/validation/kyverno-policies",
                  "kubernetes-rendering/renovate",
                ],
              },
              { label: "Resources", items: [{ autogenerate: { directory: "kubernetes-rendering/resource-guides" } }] },
            ],
          },
          {
            id: "kubernetes-gitops",
            label: layerLabel("kubernetes-gitops"),
            link: "/kubernetes-gitops/",
            icon: "code-branch",
            items: [
              "kubernetes-gitops",
              "kubernetes-gitops/rendered-manifest-pattern",
              {
                label: "Concepts",
                items: [
                  "kubernetes-gitops/project-structure",
                  "kubernetes-gitops/targets-and-clusters",
                  "kubernetes-gitops/release-inputs",
                  "kubernetes-gitops/rendered-layout",
                ],
              },
              {
                label: "Workflow",
                items: [
                  "kubernetes-gitops/workflow/render",
                  "kubernetes-gitops/workflow/diff",
                  "kubernetes-gitops/workflow/publish",
                  "kubernetes-gitops/workflow/move-publication",
                ],
              },
              {
                label: "Resources",
                items: [
                  { slug: "kubernetes-gitops/resources", label: "Overview" },
                  { autogenerate: { directory: "kubernetes-gitops/resource-guides" } },
                ],
              },
              "kubernetes-gitops/security",
            ],
          },
          {
            id: "orchestration",
            label: layerLabel("orchestration"),
            link: "/orchestration/",
            icon: "setting",
            badge: layerBadge("orchestration"),
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
            id: "domain-modeling",
            label: layerLabel("domain-modeling"),
            link: "/domain-modeling/",
            icon: "list-format",
            badge: layerBadge("domain-modeling"),
            items: ["domain-modeling", "domain-modeling/example", "domain-modeling/roadmap"],
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
                  "reference/cli/build",
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
