#!/bin/bash
set -e

# Docker installation script for CI environments
# This script installs Docker in CI/CD environments like GitHub Actions, GitLab CI, etc.

echo "Installing Docker in CI environment..."

# Check if Docker is already available
if command -v docker &> /dev/null; then
    echo "Docker is already installed, checking if it's working..."
    if docker info >/dev/null 2>&1; then
        echo "Docker is working correctly"
        exit 0
    else
        echo "Docker is installed but not working, attempting to start service..."
    fi
else
    echo "Docker not found, installing..."
    # Install Docker using the official convenience script
    curl -fsSL https://get.docker.com | sh

    # Add current user to docker group (may require logout/login in some environments)
    sudo usermod -aG docker $USER
fi

# Ensure Docker service is running
echo "Starting Docker service..."
if command -v systemctl &> /dev/null; then
    sudo systemctl start docker
elif command -v service &> /dev/null; then
    sudo service docker start
else
    echo "Warning: Could not determine how to start Docker service"
fi

# Install docker-compose if not present
if ! command -v docker-compose &> /dev/null; then
    echo "Installing docker-compose..."
    sudo curl -L "https://github.com/docker/compose/releases/latest/download/docker-compose-$(uname -s)-$(uname -m)" -o /usr/local/bin/docker-compose
    sudo chmod +x /usr/local/bin/docker-compose
fi

# Add to GitHub Actions PATH if in GitHub Actions environment
if [ -n "$GITHUB_PATH" ]; then
    echo "/usr/local/bin" >> $GITHUB_PATH
fi

# Verify installation
echo "Verifying Docker installation..."
docker --version
docker info >/dev/null 2>&1

# Check for docker compose (new syntax) or docker-compose (legacy)
if docker compose version >/dev/null 2>&1; then
    docker compose version
elif docker-compose --version >/dev/null 2>&1; then
    docker-compose --version
else
    echo "Warning: Neither 'docker compose' nor 'docker-compose' is working"
fi

echo "Docker installation completed successfully in CI environment!"
