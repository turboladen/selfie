#!/bin/bash
set -e

# Docker installation script for macOS
# This script installs Docker Desktop for Mac via Homebrew Cask

echo "Installing Docker Desktop for Mac..."

# Check if Homebrew is installed
if ! command -v brew &> /dev/null; then
    echo "Error: Homebrew is required but not installed."
    echo "Please install Homebrew first: https://brew.sh/"
    exit 1
fi

# Install Docker Desktop for Mac via Homebrew Cask
echo "Installing Docker Desktop via Homebrew..."
brew install --cask docker

# Start Docker Desktop (this will open the GUI)
echo "Starting Docker Desktop..."
open /Applications/Docker.app

# Wait for Docker to start up
echo "Waiting for Docker to start..."
timeout=60
while ! docker info >/dev/null 2>&1; do
  if [ $timeout -le 0 ]; then
    echo "Docker failed to start within 60 seconds"
    echo "Please start Docker Desktop manually and try again"
    exit 1
  fi
  echo "Docker not ready yet, waiting..."
  sleep 5
  timeout=$((timeout - 5))
done

echo "Docker is ready!"
echo "Docker Desktop has been installed and started successfully."
