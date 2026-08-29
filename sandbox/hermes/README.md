# Hermes sandbox image

This image is the sandboard Hermes seat. It installs Hermes from the pinned Git
ref at build time and uses an attached endpoint-bearing OpenShell OpenRouter
provider for model access. OpenShell injects `OPENROUTER_API_KEY` into the
sandbox at runtime; the key is not baked into the image.

The `/usr/local/bin/hermes` wrapper keeps state under `/sandbox/.hermes`, copies
the read-only baseline config on first use, repairs stale configs from older
images to use OpenRouter, and merges the Board-injected
`/sandbox/.sandboard/mcp/hermes_mcp.yaml` fragment before launching Hermes.

Sandboard uses Hermes' `chat --query-file` headless mode for card and Cockpit
chat turns. The interactive Cockpit attach uses `hermes --cli`; the modern TUI
package is not required in the sandbox image.
