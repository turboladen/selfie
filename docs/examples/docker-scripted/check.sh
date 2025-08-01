#!/bin/bash
set -e

# Docker check script
# This script verifies that Docker is properly installed and working

# Check if docker command exists
if ! command -v docker &> /dev/null; then
    echo "Error: docker command not found"
    exit 1
fi

# Check Docker version
echo "Checking Docker version..."
docker --version || exit 1

# Check if Docker daemon is running
echo "Checking Docker daemon..."
if ! docker info >/dev/null 2>&1; then
    echo "Error: Docker daemon is not running or not accessible"
    echo "Try starting Docker or check permissions"
    exit 1
fi

# Check for Docker Compose (try new syntax first, then legacy)
echo "Checking Docker Compose..."
if docker compose version >/dev/null 2>&1; then
    docker compose version
elif command -v docker-compose &> /dev/null; then
    docker-compose --version
else
    echo "Warning: Docker Compose not found or not working"
    echo "This may be expected depending on your Docker installation method"
fi

echo "Docker is installed and working correctly!"
exit 0
