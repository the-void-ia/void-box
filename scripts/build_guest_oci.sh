#!/usr/bin/env bash
set -euo pipefail

# Build and push a multi-arch OCI guest image containing vmlinuz + rootfs.cpio.gz.
#
# Usage:
#   scripts/build_guest_oci.sh                     # build + load locally
#   scripts/build_guest_oci.sh --push              # build + push to GHCR
#   TAG=v0.1.0 scripts/build_guest_oci.sh --push   # explicit tag
#
# Requires: docker (with buildx), scripts/build_guest_image.sh, scripts/download_kernel.sh

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

TAG="${TAG:-latest}"
IMAGE="${IMAGE:-ghcr.io/the-void-ia/voidbox-guest}"
PUSH="${1:-}"

# Architectures to build (override with ARCHS="x86_64" for single-arch).
if [[ -z "${ARCHS:-}" ]]; then
    ARCHS=("x86_64" "aarch64")
else
    # shellcheck disable=SC2206
    ARCHS=($ARCHS)
fi

STAGING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/voidbox-guest-oci.XXXXXX")"
trap 'rm -rf "$STAGING_DIR"' EXIT

for arch in "${ARCHS[@]}"; do
    echo "=== Building guest image for ${arch} ==="

    arch_dir="${STAGING_DIR}/${arch}"
    mkdir -p "$arch_dir"

    # Build initramfs.
    ARCH="$arch" OUT_CPIO="${arch_dir}/rootfs.cpio.gz" \
        scripts/build_guest_image.sh

    # Download kernel.
    ARCH="$arch" scripts/download_kernel.sh

    case "$arch" in
        x86_64)  cp target/vmlinuz-amd64  "${arch_dir}/vmlinuz" ;;
        aarch64)
            # macOS download_kernel.sh produces vmlinux (uncompressed) for VZ
            if [[ "$(uname -s)" == "Darwin" ]]; then
                cp target/vmlinux-arm64 "${arch_dir}/vmlinuz"
            else
                cp target/vmlinuz-arm64 "${arch_dir}/vmlinuz"
            fi
            ;;
    esac

    echo "=== ${arch}: vmlinuz + rootfs.cpio.gz ready ==="
done

# --- Build OCI image with docker buildx ---

# Create a temporary Dockerfile (FROM scratch, just copy the two files).
cat > "${STAGING_DIR}/Dockerfile" <<'DOCKERFILE'
FROM scratch
ARG TARGETARCH
COPY ${TARGETARCH}/vmlinuz /vmlinuz
COPY ${TARGETARCH}/rootfs.cpio.gz /rootfs.cpio.gz
DOCKERFILE

# Rename arch dirs to match Docker's TARGETARCH convention.
mv "${STAGING_DIR}/x86_64"  "${STAGING_DIR}/amd64"  2>/dev/null || true
mv "${STAGING_DIR}/aarch64" "${STAGING_DIR}/arm64"   2>/dev/null || true

# Build platform list from ARCHS.
PLATFORMS=()
for arch in "${ARCHS[@]}"; do
    case "$arch" in
        x86_64)  PLATFORMS+=("linux/amd64") ;;
        aarch64) PLATFORMS+=("linux/arm64") ;;
    esac
done
PLATFORM_LIST=$(IFS=,; echo "${PLATFORMS[*]}")

BUILDX_ARGS=(
    --platform "$PLATFORM_LIST"
    -t "${IMAGE}:${TAG}"
    -f "${STAGING_DIR}/Dockerfile"
    "${STAGING_DIR}"
)

if [[ "$PUSH" == "--push" ]]; then
    echo "=== Pushing ${IMAGE}:${TAG} ==="
    docker buildx build --push "${BUILDX_ARGS[@]}"
else
    echo "=== Building ${IMAGE}:${TAG} (local only) ==="
    docker buildx build --load "${BUILDX_ARGS[@]}" 2>/dev/null || \
        docker buildx build "${BUILDX_ARGS[@]}"
fi

echo "=== Done: ${IMAGE}:${TAG} ==="
