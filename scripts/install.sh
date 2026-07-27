#!/bin/bash
# Grace install script
# Usage: curl -fsSL https://raw.githubusercontent.com/AmnGrg0511/grace/master/scripts/install.sh | bash

set -euo pipefail

REPO="AmnGrg0511/grace"
BINARY_NAME="grace"
INSTALL_DIR="${HOME}/.local/bin"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info() { echo -e "${BLUE}[info]${NC} $*"; }
success() { echo -e "${GREEN}[success]${NC} $*"; }
warn() { echo -e "${YELLOW}[warn]${NC} $*"; }
error() { echo -e "${RED}[error]${NC} $*" >&2; }

# Detect platform
detect_platform() {
    local os arch
    case "$(uname -s)" in
        Linux*) os="linux" ;;
        Darwin*) os="darwin" ;;
        *) error "Unsupported OS: $(uname -s)"; exit 1 ;;
    esac
    case "$(uname -m)" in
        x86_64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) error "Unsupported arch: $(uname -m)"; exit 1 ;;
    esac
    # Use musl on Linux for better portability (no GLIBC version dependency)
    if [[ "$os" == "linux" ]]; then
        echo "${arch}-unknown-${os}-musl"
    else
        echo "${arch}-unknown-${os}-gnu"
    fi
}

# Get latest release version
get_latest_version() {
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name":' \
        | sed -E 's/.*"tag_name": "([^"]+)".*/\1/' \
        | head -1
}

# Download and install
install_binary() {
    local target="$1"
    local version="$2"
    local url="https://github.com/${REPO}/releases/download/${version}/grace-${target}.tar.gz"
    local tmpdir=$(mktemp -d)

    info "Downloading ${version} for ${target}..."
    if ! curl -fsSL "${url}" -o "${tmpdir}/grace.tar.gz"; then
        error "Failed to download ${url}"
        rm -rf "${tmpdir}"
        exit 1
    fi

    info "Extracting..."
    tar -xzf "${tmpdir}/grace.tar.gz" -C "${tmpdir}"

    info "Installing to ${INSTALL_DIR}..."
    mkdir -p "${INSTALL_DIR}"
    mv "${tmpdir}/grace" "${INSTALL_DIR}/grace"
    chmod +x "${INSTALL_DIR}/grace"

    rm -rf "${tmpdir}"
}

# Add to PATH if needed
ensure_path() {
    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) return 0 ;;
        *)
            warn "${INSTALL_DIR} is not in PATH"
            local shell_rc=""
            case "${SHELL}" in
                */bash) shell_rc="${HOME}/.bashrc" ;;
                */zsh) shell_rc="${HOME}/.zshrc" ;;
                */fish) shell_rc="${HOME}/.config/fish/config.fish" ;;
            esac
            if [[ -n "${shell_rc}" ]]; then
                echo "export PATH=\"${INSTALL_DIR}:\$PATH\"" >> "${shell_rc}"
                info "Added ${INSTALL_DIR} to PATH in ${shell_rc}"
                info "Run: source ${shell_rc}"
            fi
            ;;
    esac
}

# Setup shell alias and completions
setup_shell() {
    local alias_line="alias g='grace'"
    local comp_line="eval \"\$(grace --completions \$(basename \${SHELL}))\""

    for shell_rc in "${HOME}/.bashrc" "${HOME}/.zshrc"; do
        [[ -f "${shell_rc}" ]] || continue
        if ! grep -q "alias g=" "${shell_rc}" 2>/dev/null; then
            echo "" >> "${shell_rc}"
            echo "# Grace alias" >> "${shell_rc}"
            echo "${alias_line}" >> "${shell_rc}"
            info "Added alias 'g' to ${shell_rc}"
        fi
        if ! grep -q "grace --completions" "${shell_rc}" 2>/dev/null; then
            echo "${comp_line}" >> "${shell_rc}"
            info "Added shell completions to ${shell_rc}"
        fi
    done

    # Fish
    local fish_rc="${HOME}/.config/fish/config.fish"
    if [[ -f "${fish_rc}" ]]; then
        if ! grep -q "alias g=" "${fish_rc}" 2>/dev/null; then
            echo "" >> "${fish_rc}"
            echo "# Grace alias" >> "${fish_rc}"
            echo "alias g='grace'" >> "${fish_rc}"
            info "Added alias 'g' to ${fish_rc}"
        fi
        if ! grep -q "grace --completions" "${fish_rc}" 2>/dev/null; then
            echo "grace --completions fish | source" >> "${fish_rc}"
            info "Added shell completions to ${fish_rc}"
        fi
    fi
}

# Main
main() {
    info "Installing Grace..."

    local target
    target=$(detect_platform)
    info "Platform: ${target}"

    local version
    version=$(get_latest_version)
    if [[ -z "${version}" ]]; then
        error "Could not determine latest version"
        exit 1
    fi
    info "Latest version: ${version}"

    install_binary "${target}" "${version}"
    ensure_path
    setup_shell

    success "Grace ${version} installed!"
    info "Run 'grace --chat' to start, or 'grace --help' for options."
    info "You may need to restart your shell or run: source ~/.bashrc"
}

main "$@"