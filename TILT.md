# Multi-Distribution Testing

Run the selfie test suite inside Debian and Alpine containers with Tilt and Docker Compose. The two
images cover different package managers and libc variants: Debian 12 (`apt`, glibc) and Alpine
(`apk`, musl).

## Prerequisites

- **Docker** with the Compose plugin
- **Tilt**: `brew install tilt-dev/tap/tilt`

Each image has Rust installed. The repository is mounted at `/workspace` in both containers, with a
separate `target/` per distribution (`target/debian`, `target/alpine`) so the two builds never share
artifacts.

## Quick Start

```bash
docker compose up -d          # Build the images and start both containers
tilt up                       # Start the Tilt UI at http://localhost:10350

tilt trigger test-all         # Run the test suite on every distribution
tilt trigger debian-tests     # Debian only
tilt trigger alpine-tests     # Alpine only

tilt down                     # Stop Tilt
docker compose down -v        # Remove the containers and their cargo caches
```

## Tilt Resources

`Tiltfile` defines every resource with a manual trigger, so nothing runs until you ask for it.
Trigger a resource from the UI or with `tilt trigger <resource>`:

- `debian` and `alpine` are the Docker Compose services.
- `debian-tests` and `alpine-tests` run `cargo test --all` inside the matching container.
- `test-all` triggers both test resources in sequence.

Each resource's logs are shown in the UI as it runs.

## Working Inside a Container

```bash
# Open a shell
docker compose exec debian bash
docker compose exec alpine bash

# Inside the container, the usual commands work:
cargo test --all
cargo test --test cli_tests
cargo run -- --help
cargo run -- spec list
cargo run -- spec validate my-package
cargo run -- package check my-package
cargo run -- --verbose package list
cargo run -- config validate
```

`cargo run` inside a container reads the container user's home directory, not yours, so it needs a
config file there before commands that read the package directory succeed.

The same commands run without a shell:

```bash
docker compose exec -T debian cargo run -- spec list
docker compose exec -T alpine cargo test --test cli_tests
docker compose exec -T debian bash -c "apt list --installed | head -5"
docker compose exec -T alpine sh -c "apk list --installed | head -5"
```

## Local Gates

The containers exercise platform behavior. The pre-commit gates still run on the host:

```bash
just check      # every CI gate, in order
just test       # the test suite only
just clippy     # clippy with -D warnings
```

## Troubleshooting

```bash
docker compose ps                  # Container status
docker compose logs debian         # One container's logs
docker compose build --no-cache    # Rebuild the images from scratch
docker compose down -v             # Remove containers and volumes for a clean slate
tilt down && tilt up               # Restart Tilt
```

## Files

```
selfie/
├── Tiltfile                  # Tilt resources
├── docker-compose.yml        # Container definitions
├── docker/
│   ├── debian/Dockerfile     # Debian + Rust
│   └── alpine/Dockerfile     # Alpine + Rust
└── target/
    ├── debian/               # Debian build artifacts
    └── alpine/               # Alpine build artifacts
```
