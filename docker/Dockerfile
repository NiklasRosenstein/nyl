#: Copy Nyl binary (Rust).
#: ----------------------
#: Binary is built by CI (musl, statically linked) and provided in build context
#: as nyl-{amd64,arm64}. No validation here: the arm64 build runs under QEMU
#: emulation which cannot execute the binary; CI already verifies it.

FROM scratch AS nyl-bin
ARG TARGETARCH=amd64
COPY nyl-${TARGETARCH} /usr/local/bin/nyl

#: Base stage in which we'll fetch binaries from third-party tools.
#: ----------------------------------------------------------------

FROM alpine AS fetch-bin-base
RUN apk add bash curl openssl upx
RUN \
    case "$(uname -m)" in \
        x86_64) BIN_ARCH='amd64' ;; \
        aarch64) BIN_ARCH='arm64' ;; \
        *) echo "Unsupported architecture: $(uname -m)" && exit 1 ;; \
    esac \
    && printf "%s" "$BIN_ARCH" > /BINARY_ARCH


#: Fetch binaries used by Nyl rendering workflows.
#: ------------------------------------------------

FROM fetch-bin-base AS helm-bin
# Find Helm releases at https://github.com/helm/helm/releases
# renovate: datasource=github-releases depName=helm packageName=helm/helm
ARG HELM_VERSION=v4.3.0
RUN curl -fsSL -o get_helm.sh https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-4 \
    && chmod 700 get_helm.sh \
    && ./get_helm.sh --version $HELM_VERSION \
    && upx /usr/local/bin/helm \
    && helm version

FROM fetch-bin-base AS sops-bin
# Find SOPS releases at https://github.com/getsops/sops/releases
# renovate: datasource=github-releases depName=sops packageName=getsops/sops
ARG SOPS_VERSION=v3.13.3
RUN curl -fLO https://github.com/getsops/sops/releases/download/${SOPS_VERSION}/sops-${SOPS_VERSION}.linux.$(cat /BINARY_ARCH) \
    && mv sops-${SOPS_VERSION}.linux.$(cat /BINARY_ARCH) /usr/local/bin/sops \
    && chmod +x /usr/local/bin/sops \
    && upx /usr/local/bin/sops \
    && sops --version

FROM fetch-bin-base AS kyverno-bin
# renovate: datasource=github-releases depName=kyverno packageName=kyverno/kyverno
ARG KYVERNO_VERSION=1.19.0
RUN \
    case "$(uname -m)" in \
        x86_64) KYVERNO_ARCH='x86_64' ;; \
        aarch64) KYVERNO_ARCH='arm64' ;; \
        *) echo "Unsupported architecture: $(uname -m)" && exit 1 ;; \
    esac \
    && curl -fLO https://github.com/kyverno/kyverno/releases/download/v${KYVERNO_VERSION}/kyverno-cli_v${KYVERNO_VERSION}_linux_${KYVERNO_ARCH}.tar.gz \
    && tar -xvf kyverno-cli_v${KYVERNO_VERSION}_linux_${KYVERNO_ARCH}.tar.gz \
    && mv kyverno /usr/local/bin/ \
    && upx /usr/local/bin/kyverno

#: Build the CI rendering image.
#: -----------------------------

FROM fetch-bin-base AS kubeconform-bin
# Version and archive digests identify the tested validator distribution.
ARG KUBECONFORM_VERSION=v0.8.0
RUN case "$(cat /BINARY_ARCH)" in \
        amd64) KUBECONFORM_SHA256=9bc2bffbf71f261128533edaf912153948b7ff238f9a531ae6d34466ec287883 ;; \
        arm64) KUBECONFORM_SHA256=1f53fc8e81258197a35e8603054162a5af1de8c5af13746c71ab680d9534ed87 ;; \
    esac \
    && curl -fsSL --max-time 120 -o kubeconform.tar.gz \
        https://github.com/yannh/kubeconform/releases/download/$KUBECONFORM_VERSION/kubeconform-linux-$(cat /BINARY_ARCH).tar.gz \
    && printf '%s  kubeconform.tar.gz\n' "$KUBECONFORM_SHA256" | sha256sum -c - \
    && tar -xzf kubeconform.tar.gz -C /usr/local/bin kubeconform

FROM debian:bookworm-slim AS nyl-ci
RUN apt-get update && \
    apt-get install -y --no-install-recommends bash git ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=helm-bin /usr/local/bin/helm /usr/local/bin/helm
COPY --from=sops-bin /usr/local/bin/sops /usr/local/bin/sops
COPY --from=kyverno-bin /usr/local/bin/kyverno /usr/local/bin/kyverno
COPY --from=kubeconform-bin /usr/local/bin/kubeconform /usr/local/bin/kubeconform
COPY --from=nyl-bin /usr/local/bin/nyl /usr/local/bin/nyl

WORKDIR /workspace
CMD ["nyl", "--help"]
