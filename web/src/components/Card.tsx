import { formatCountdown, secsUntil, since } from "../api.js";
import { friendlyState, humanizeEscalation } from "../humanize.js";
import type { ColumnKey, WorkItem } from "../types";
import { cardPullRequests } from "../types.js";

interface Props {
  item: WorkItem;
  column: ColumnKey;
  now: number;
  /** Wall-clock run budget from the board (`agent_timeout_secs`). */
  agentTimeout: number;
  defaultEngine?: string;
  defaultModel?: string;
  breadcrumb?: string;
  labelOf?: (id: number) => string;
  onOpen: (id: number) => void;
}

/** Remaining fraction of the fixed run timeout (1 = full time left). */
export function countdownRemainFrac(
  deadlineIso: string | null | undefined,
  totalSecs: number,
  now: number,
): number {
  if (!deadlineIso || totalSecs <= 0) return 0;
  const remaining = secsUntil(deadlineIso, now);
  return Math.min(1, Math.max(0, remaining / totalSecs));
}

/**
 * Card anatomy differs by column, because the question you're asking differs.
 * Everything on the face is here to answer that column's one question.
 */
export function Card({ item, column, now, agentTimeout, defaultEngine, defaultModel, breadcrumb, labelOf, onOpen }: Props) {
  const machine = item.origin.kind !== "human";
  const idLabel = (id: number) => (labelOf ? labelOf(id) : `#${id}`);

  const engine = item.engine ?? defaultEngine;
  const model = item.resolved_model ?? item.model ?? defaultModel;

  const deadline = item.run_deadline_at ?? item.lease?.expires_at ?? null;
  const remaining = deadline ? secsUntil(deadline, now) : null;
  const remainFrac = countdownRemainFrac(deadline, agentTimeout, now);
  const endingSoon = remaining !== null && remainFrac < 0.1;

  const blockersList = (item.blockers && item.blockers.length > 0
    ? item.blockers
    : item.blocked_by.map((id) => ({ id, title: `Task #${id}`, state: "backlog" as const }))
  ).filter((b) => b.state !== "done" && b.state !== "retired");

  return (
    <div
      className={`card col-${column} ${machine ? "machine" : ""} ${endingSoon ? "ending-soon" : ""}`}
      onClick={() => onOpen(item.id)}
      title={
        machine
          ? item.origin.kind === "split"
            ? `Created by a splitting sibling (${idLabel(item.origin.from)})`
            : `Created by the ${item.origin.kind}`
          : "Created by a human"
      }
    >
      <div className="card-title">
        <span className="id">#{item.id}</span> {item.title}
      </div>

      {blockersList.length > 0 && (
        <div className="blocker-chips" data-testid="blocker-chips">
          <span className="blocker-label">⊘ waiting on</span>
          {blockersList.map((b) => (
            <span
              key={b.id}
              className={`blocker-chip state-${b.state}`}
              title={`#${b.id}: ${b.title} (${friendlyState(b.state)})`}
            >
              <span className="blocker-id">#{b.id}</span>
              <span className="blocker-title">{b.title}</span>
              <span className="state-cue">{friendlyState(b.state)}</span>
            </span>
          ))}
        </div>
      )}

      {(column === "backlog" || column === "ready") && (
        <>
          <div className="row">
            <span className="tag">⊙ {item.capability ?? "any"}</span>
            {item.awaiting_dispatch && <span className="tag">⏳ queued</span>}
            {item.parked && <span className="tag">⏸ parked</span>}
            <span className="dim">{since(item.entered_state_at, now)}</span>
          </div>
          {breadcrumb && <div className="crumb">↑ {breadcrumb}</div>}
        </>
      )}

      {column === "running" && (
        <>
          <div className="row">
            <span className="tag">
              {engine === "agy"
                ? "⚡ agy"
                : engine === "claude"
                  ? "🤖 claude"
                  : engine === "cursor"
                    ? "◈ cursor"
                    : engine === "opencode"
                      ? "◐ opencode"
                      : engine === "hermes"
                        ? "✦ hermes"
                      : `◍ ${engine || "?"}`}
            </span>
            {model && (
              <span className="tag dim" data-testid="card-model-badge">
                {model}
              </span>
            )}
            <span className={endingSoon ? "countdown ending-soon" : "countdown"}>
              {remaining !== null ? formatCountdown(remaining) : "—"}
            </span>
          </div>
          <div className="bar">
            <div
              className={`fill${endingSoon ? " ending-soon" : ""}`}
              style={{ width: `${Math.round(remainFrac * 100)}%` }}
            />
          </div>
          <div className="row dim">
            <span>{Math.round(remainFrac * 100)}% left</span>
            <span>{since(item.entered_state_at, now)}</span>
          </div>
        </>
      )}

      {column === "needs_you" && item.escalation && (
        <>
          <div className="question">
            {humanizeEscalation(item.escalation.question).summary}
          </div>
          <div className="row">
            <span className="tag">
              {item.escalation.options.length} option
              {item.escalation.options.length === 1 ? "" : "s"}
            </span>
            <span className="blocked-for">
              waiting {since(item.escalation.blocked_since, now)}
            </span>
          </div>
        </>
      )}

      {column === "review" && (
        <>
          <div className="row">
            <span className="diff">
              +{item.diff_added} −{item.diff_removed}
            </span>
            <span className="dim">{since(item.entered_state_at, now)}</span>
          </div>
          {/* Review *is* the PR. Without a way to reach it the column asks a
              question you cannot answer from the board. */}
          {cardPullRequests(item).map((pr) => (
            <a
              key={pr.url}
              className="pr-link"
              href={pr.url}
              target="_blank"
              rel="noreferrer"
              onClick={(e) => e.stopPropagation()}
            >
              ↗ {prLabel(pr.url)}{pr.merged ? " (merged)" : ""}
            </a>
          ))}
          {/* Where it ran. The sandbox is gone by now, but the name is what
              the logs and any post-mortem are filed under. */}
          {item.environment && <div className="sandbox">⬚ {item.environment}</div>}
          {breadcrumb && <div className="crumb">↑ {breadcrumb}</div>}
        </>
      )}

      {column === "done" && (
        <div className="row dim">
          <span>
            +{item.diff_added} −{item.diff_removed}
          </span>
        </div>
      )}
    </div>
  );
}

/** `https://github.com/owner/repo/pull/1` -> `owner/repo#1`. */
function prLabel(url: string): string {
  const m = url.match(/github\.com\/([^/]+)\/([^/]+)\/pull\/(\d+)/);
  return m ? `${m[1]}/${m[2]}#${m[3]}` : "pull request";
}
