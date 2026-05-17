import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";

export default defineConfig({
  site: "https://niklasrosenstein.github.io",
  base: "/nyl",
  integrations: [
    starlight({
      title: "Nyl",
      description: "A fast Kubernetes manifest generator for rendered manifest GitOps, CLI workflows, and ArgoCD CMP integration.",
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
                "commands/new",
                "commands/validate",
                "commands/render",
                "commands/diff",
                "commands/apply",
                "commands/generate",
              ],
            },
          ],
        },
        {
          label: "ArgoCD Integration",
          items: [
            "argocd/overview",
            "argocd/plugin",
            "argocd/bootstrapping",
            "argocd/application-generator",
            "argocd/repository-secrets",
            "argocd/best-practices",
          ],
        },
        {
          label: "Reference",
          items: [
            "reference/resources",
            "reference/resources/component",
            "reference/resources/helmchart",
            "reference/resources/remote-manifest",
            "reference/resources/nyl-release",
            "reference/resources/application-generator",
            "reference/kyverno-policies",
            "reference/schemas",
          ],
        },
        {
          label: "Extras",
          items: ["extras/renovate"],
        },
      ],
    }),
  ],
});
