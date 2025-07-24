#!/bin/bash

# Common functions for Docker installation scripts
# This file contains shared utilities that can be sourced by other scripts

# Colors for output (if terminal supports it)
if [ -t 1 ]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    NC='\033[0m' # No Color
else
    RED=''
    GREEN=''
    YELLOW=''
    BLUE=''
    NC=''
fi

# Logging functions
log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Check if running as root
is_root() {
    [ "$EUID" -eq 0 ]
}

# Check if command exists
command_exists() {
    command -v "$1" >/dev/null 2>&1
}

# Detect the operating system
detect_os() {
    if [[ "$OSTYPE" == "linux-gnu"* ]]; then
        if [ -f /etc/os-release ]; then
            . /etc/os-release
            echo "$ID"
        elif [ -f /etc/redhat-release ]; then
            echo "rhel"
        elif [ -f /etc/debian_version ]; then
            echo "debian"
        else
            echo "linux"
        fi
    elif [[ "$OSTYPE" == "darwin"* ]]; then
        echo "macos"
    elif [[ "$OSTYPE" == "cygwin" ]] || [[ "$OSTYPE" == "msys" ]]; then
        echo "windows"
    else
        echo "unknown"
    fi
}

# Check if Docker is already installed and working
check_docker_status() {
    if command_exists docker; then
        if docker info >/dev/null 2>&1; then
            return 0  # Docker is installed and working
        else
            return 1  # Docker is installed but not working
        fi
    else
        return 2  # Docker is not installed
    fi
}

# Wait for Docker daemon to be ready
wait_for_docker() {
    local timeout=${1:-60}
    local count=0

    log_info "Waiting for Docker daemon to be ready..."

    while ! docker info >/dev/null 2>&1; do
        if [ $count -ge $timeout ]; then
            log_error "Docker failed to start within $timeout seconds"
            return 1
        fi

        log_info "Docker not ready yet, waiting... ($count/$timeout)"
        sleep 5
        count=$((count + 5))
    done

    log_success "Docker daemon is ready!"
    return 0
}

# Add user to docker group
add_user_to_docker_group() {
    local user=${1:-$USER}

    if ! groups "$user" | grep -q docker; then
        log_info "Adding user '$user' to docker group..."
        sudo usermod -aG docker "$user"
        log_success "User '$user' added to docker group"
        log_warning "You may need to log out and back in for group changes to take effect"
    else
        log_info "User '$user' is already in docker group"
    fi
}

# Start Docker service
start_docker_service() {
    log_info "Starting Docker service..."

    if command_exists systemctl; then
        sudo systemctl start docker
        sudo systemctl enable docker
    elif command_exists service; then
        sudo service docker start
    elif command_exists rc-service; then
        # Alpine Linux
        sudo rc-service docker start
        sudo rc-update add docker default
    else
        log_warning "Could not determine how to start Docker service"
        return 1
    fi

    log_success "Docker service started"
}

# Cleanup function for failed installations
cleanup_failed_installation() {
    log_error "Installation failed, cleaning up..."

    # Remove any partially installed packages (this is OS-specific)
    case "$(detect_os)" in
        ubuntu|debian)
            sudo apt-get autoremove -y docker-ce docker-ce-cli containerd.io || true
            ;;
        fedora|centos|rhel)
            sudo dnf remove -y docker-ce docker-ce-cli containerd.io || true
            ;;
        arch)
            sudo pacman -R --noconfirm docker docker-compose || true
            ;;
    esac
}

# Verify Docker installation
verify_docker_installation() {
    log_info "Verifying Docker installation..."

    # Check docker command
    if ! command_exists docker; then
        log_error "Docker command not found"
        return 1
    fi

    # Check docker version
    log_info "Docker version:"
    docker --version || return 1

    # Check if daemon is running
    if ! docker info >/dev/null 2>&1; then
        log_error "Docker daemon is not running or not accessible"
        return 1
    fi

    # Check docker compose
    if docker compose version >/dev/null 2>&1; then
        log_info "Docker Compose (plugin) version:"
        docker compose version
    elif command_exists docker-compose; then
        log_info "Docker Compose (standalone) version:"
        docker-compose --version
    else
        log_warning "Docker Compose not found"
    fi

    log_success "Docker installation verified successfully!"
    return 0
}

# Print system information
print_system_info() {
    log_info "System Information:"
    echo "  OS: $(detect_os)"
    echo "  Architecture: $(uname -m)"
    echo "  Kernel: $(uname -r)"
    echo "  User: $USER"
    echo "  Groups: $(groups)"
}
