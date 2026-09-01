# Sandbox (pi)

OpenShell sandbox image for the [pi](https://github.com/earendil-works/pi) terminal coding harness.

## What's Included

- **pi** — `@earendil-works/pi-coding-agent` (latest stable)
- Everything from the [base sandbox](../Containerfile) (shared stage)

## Build

```bash
podman build -f sandbox/Containerfile --target pi -t quay.io/sandboard-app/sandbox-pi:latest .
```

## Usage

### As a sandboard sandbox profile

Profile id: `sandbox-pi`
Image: `quay.io/sandboard-app/sandbox-pi:latest`
Engine: `pi`

### With openshell directly

```bash
openshell sandbox create --from sandbox-pi
openshell sandbox create --from sandbox-pi -- pi
```
