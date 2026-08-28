import { publicMcpUrl } from "../publicOrigin.js";

/**
 * Reusable operator onboarding — Quickstart (first Project loop), MCP connect,
 * and OpenShell + sandbox setup. Cursor / Claude snippets are secondary examples.
 * Embed from Board empty state or Help; keep chrome (hero, nav) outside.
 */
export function OperatorGuide() {
  const mcpUrl = publicMcpUrl();
  const cursorMcpJson = `{
  "mcpServers": {
    "sandboard": {
      "type": "http",
      "url": "${mcpUrl}",
      "auth": { "CLIENT_ID": "sandboard-cursor", "scopes": ["mcp"] }
    }
  }
}`;
  const claudeMcpAdd = `claude mcp add --transport http sandboard ${mcpUrl}`;

  return (
    <div className="operator-guide" data-testid="operator-guide">
      <section
        className="operator-guide-section"
        aria-labelledby="operator-guide-quickstart-title"
        data-testid="operator-guide-quickstart"
      >
        <h2 id="operator-guide-quickstart-title">Quickstart</h2>
        <p className="dim">
          Create a Project, approve its plan, then dispatch. Agents stay idle
          until you dispatch.
        </p>
        <ol
          className="operator-guide-steps"
          data-testid="operator-guide-quickstart-steps"
        >
          <li>
            Create a Project with required <code>clone_repo</code> as{" "}
            <code>owner/name</code> — on the board or via{" "}
            <code>create_project</code>. That creates an{" "}
            <strong>Initial plan</strong> task for that repo.
          </li>
          <li>
            <code>dispatch</code> the Initial plan — the agent clones{" "}
            <code>clone_repo</code> and writes <code>plan.json</code>. Each
            proposed task should name its clone target in intent/DoD.
          </li>
          <li>
            <strong>Approve</strong> — creates the Project&apos;s Tasks from the
            plan.
          </li>
          <li>
            <code>dispatch</code> each Backlog Task (or turn on Project auto
            mode).
          </li>
        </ol>
        <p className="dim" data-testid="operator-guide-idle-note">
          Agents stay idle until you dispatch. Override the Project clone in a
          Task&apos;s Why/DoD when the card needs a different repo.
        </p>
        <p className="dim" data-testid="operator-guide-create-task-note">
          After a Project exists, add Backlog Tasks with board{" "}
          <strong>Create Task</strong> or MCP <code>create_task</code> — no need
          to re-run Initial plan. <code>clone_repo</code> is a Project field;
          Tasks inherit it unless Why/DoD name{" "}
          <code>Clone repository: owner/name</code>. <strong>Approve</strong>{" "}
          still only materializes proposals and never merges.
        </p>
      </section>

      <section
        className="operator-guide-section"
        aria-labelledby="operator-guide-config-title"
        data-testid="operator-guide-config"
      >
        <h2 id="operator-guide-config-title">Configuration and standing instructions</h2>
        <p className="dim">
          sandboard stacks setup in layers. Lower layers are operator concerns; agents
          read the board standing prompt, optional <code>project_prompt</code>, and
          card prose at claim time.
        </p>
        <ol
          className="operator-guide-steps"
          data-testid="operator-guide-config-steps"
        >
          <li>
            <strong>Process boot</strong> and <strong>board Settings</strong>{" "}
            (Policies, sandbox specs, agent runtime including standing prompt,
            Forge) — host/operator setup.
          </li>
          <li>
            <strong>Project fields</strong> — <code>clone_repo</code> and
            optional sandbox override seed the Initial plan.
          </li>
          <li>
            <strong>Standing prompt</strong> — board-wide agent policy on
            Settings → Agent runtime. Optional <strong>project_prompt</strong>{" "}
            is Project-only extras.
          </li>
          <li>
            <strong>Per-card intent / DoD</strong> — clone target and
            card-specific gates for that Task.
          </li>
        </ol>
        <p className="dim" data-testid="operator-guide-quality-gates-note">
          Name test/lint quality gates in the board standing prompt when they
          apply everywhere. sandboard does not assume <code>cargo</code> or any
          toolchain unless standing text or a card&apos;s DoD names it.
        </p>
      </section>

      <section
        className="operator-guide-section"
        aria-labelledby="operator-guide-mcp-title"
        data-testid="operator-guide-mcp"
      >
        <h2 id="operator-guide-mcp-title">Connect MCP</h2>
        <p className="dim">
          Drive the board from an MCP client: create Projects, create Tasks
          under a Project (<code>create_task</code>), triage, dispatch, park,
          steer, and approve. Start sandboard before adding the server.
        </p>
        <ol className="operator-guide-steps" data-testid="operator-guide-mcp-steps">
          <li>
            Start sandboard so it is listening (API + MCP share the board origin).
          </li>
          <li>
            Point your client at the Streamable HTTP endpoint:
            <pre
              className="operator-guide-snippet"
              data-testid="operator-guide-mcp-url"
            >
              {mcpUrl}
            </pre>
          </li>
          <li>
            Transport is <strong>Streamable HTTP</strong> (not stdio).
          </li>
          <li>
            Add an MCP server named <code>sandboard</code> at that URL.
          </li>
          <li>
            After local admin exists, authenticate via MCP OAuth (browser login /
            consent — same admin or GitHub allowlist as the board).
          </li>
        </ol>
        <p className="dim" data-testid="operator-guide-mcp-empty-tools">
          Tokens survive a sandboard restart. If the tools list stays empty, reload
          the client.
        </p>

        <aside
          className="operator-guide-examples"
          data-testid="operator-guide-client-examples"
        >
          <h3>Client examples</h3>
          <p className="dim">
            Optional — same Streamable HTTP endpoint and server name{" "}
            <code>sandboard</code> in any MCP client.
          </p>
          <p className="operator-guide-example-label">
            Cursor — <code>.cursor/mcp.json</code>, then Tools &amp; MCP →
            Authenticate / Connect (or <code>agent mcp login sandboard</code>)
          </p>
          <pre
            className="operator-guide-snippet"
            data-testid="operator-guide-cursor-snippet"
          >
            {cursorMcpJson}
          </pre>
          <p className="operator-guide-example-label">Claude Code</p>
          <pre
            className="operator-guide-snippet"
            data-testid="operator-guide-claude-snippet"
          >
            {claudeMcpAdd}
          </pre>
        </aside>
      </section>

      <section
        className="operator-guide-section"
        aria-labelledby="operator-guide-openshell-title"
        data-testid="operator-guide-openshell"
      >
        <h2 id="operator-guide-openshell-title">OpenShell + sandbox</h2>
        <p className="dim">
          Before agents can run, connect the OpenShell gateway and set up
          providers plus a sandbox spec. Paste credentials in Settings — sandboard
          does not find them on the host for you.
        </p>
        <ol
          className="operator-guide-steps"
          data-testid="operator-guide-openshell-steps"
        >
          <li>
            <a
              className="operator-guide-link"
              href="/settings/openshell/connectivity"
            >
              Settings → OpenShell → Connectivity
            </a>
            {" "}
            — gateway endpoint and mTLS PEMs (CA, client cert, client key).
            Refresh status until Healthy.
          </li>
          <li>
            <a
              className="operator-guide-link"
              href="/settings/openshell/providers"
            >
              Settings → OpenShell → Providers
            </a>
            {" "}
            — configure providers (including shipped{" "}
            <code>github-app</code> / <code>GH_TOKEN</code>, alongside{" "}
            <code>cursor-agent</code> and others). Sync applies them to the
            gateway.
          </li>
          <li>
            <a
              className="operator-guide-link"
              href="/settings/openshell/policies"
            >
              Settings → OpenShell → Policies
            </a>
            {" "}
            — network / filesystem allow-lists. Sandbox specs pick a policy by
            id.
          </li>
          <li>
            <a
              className="operator-guide-link"
              href="/settings/openshell/profiles"
            >
              Settings → OpenShell → Sandbox specs
            </a>
            {" "}
            — image, resources, engine, and which policy + providers attach on
            create.
          </li>
          <li>
            Tune concurrency / timeouts under{" "}
            <a className="operator-guide-link" href="/settings/agent-runtime">
              Settings → Agent runtime
            </a>
            {" "}
            if needed.
          </li>
        </ol>
      </section>
    </div>
  );
}
