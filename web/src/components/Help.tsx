import { OpenShellReadinessStrip } from "./OpenShellReadiness.js";
import { OperatorGuide } from "./OperatorGuide.js";

/**
 * Help surface — Welcome hero + OpenShell readiness + OperatorGuide
 * (Create Project stays on the Board).
 */
export function Help() {
  return (
    <div className="help-page" data-testid="help-page">
      <header className="board-hero">
        <h1>Welcome to sandboard</h1>
        <p className="board-lede">
          Create a Project, set standing instructions if needed, approve its
          plan, then dispatch work. Configuration layers and setup steps are
          below.
        </p>
      </header>

      <div className="board-empty" data-testid="help-welcome">
        <OpenShellReadinessStrip />
        <OperatorGuide />
      </div>
    </div>
  );
}
