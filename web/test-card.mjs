import React from "react";
import { renderToString } from "react-dom/server";
import assert from "node:assert";
import { Card } from "./dist-test/components/Card.js";
import { Board, isBlocked, sortFor } from "./dist-test/components/Board.js";
import { Head, PlanEditor, planTasksFromArtifact, reduceDetail } from "./dist-test/components/Detail.js";
import { PrimarySidebar } from "./dist-test/components/PrimarySidebar.js";
import { AccountMenu } from "./dist-test/components/AccountMenu.js";
import {
  Cockpit,
  CockpitAttachView,
  CockpitDrop,
  CockpitSessionView,
  CockpitToggle,
  cockpitAttachGate,
  cockpitAttachRetryDelayMs,
  cockpitBarChip,
  cockpitChatGate,
  cockpitPhaseLabel,
  cockpitPollIntervalMs,
} from "./dist-test/components/Cockpit.js";
import { Help } from "./dist-test/components/Help.js";
import { CreateProjectForm } from "./dist-test/components/CreateProjectForm.js";
import {
  CreateTaskForm,
  cloneRepoFromProse,
  proseHasCloneRepo,
  stampCloneIntoIntent,
} from "./dist-test/components/CreateTaskForm.js";
import { OperatorGuide } from "./dist-test/components/OperatorGuide.js";
import { ProjectSandboxPicker, SandboxesPanelView, Settings, WorkspacePanelView, OpenShellPanelView, OpenShellProvidersPanelView, OpenShellPoliciesPanelView, OpenShellProviderTypesPanelView, AgentRuntimePanelView, RepoAccessPanelView, OpenShellReadinessStripView, gatewayReady, gatewayMtlsReady, sandboxSpecReady, sandboxHasNoProviders } from "./dist-test/components/Settings.js";
import { initial, reduce, isSequenceGap, subscribeBoardEvents, emitBoardEvent } from "./dist-test/useBoard.js";
import { sandboardWsHost, sandboardWsUrl } from "./dist-test/wsUrl.js";
import {
  chromeLocationsEqual,
  formatChromePath,
  parseChromeLocation,
  writeChromeLocation,
} from "./dist-test/location.js";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const now = Math.floor(Date.now() / 1000);

// Test 1: Blocked card renders human-readable blocker chips
const blockedItem = {
  id: 7,
  parent: null,
  level: "Story",
  title: "Fail closed when CI is red",
  intent: "A Review card with failing checks should be obvious.",
  definition_of_done: "Done",
  state: "backlog",
  origin: { kind: "human" },
  above_line: false,
  blocked_by: [6],
  blockers: [
    { id: 6, title: "Surface PR checks on the Review card", state: "backlog" },
  ],
  capability: "any",
  lease: null,
  model: null,
  progress: 0,
  escalation: null,
  gates: [],
  gate_failures: 0,
  diff_added: 0,
  diff_removed: 0,
  notes: [],
  pinned: [],
  release_target: null,
  environment: null,
  pull_request: null,
  created_at: new Date().toISOString(),
  entered_state_at: new Date().toISOString(),
  history: [],
};

const blockedHtml = renderToString(
  React.createElement(Card, {
    item: blockedItem,
    column: "backlog",
    now,
    agentTimeout: 600,
    onOpen: () => {},
  })
);

console.log("Blocked Card HTML:\n", blockedHtml);

// Assertions for blocked card
assert(blockedHtml.includes('class="blocker-chips"'), "Should contain blocker-chips container");
assert(blockedHtml.includes('class="blocker-chip state-backlog"'), "Should contain blocker-chip with state class");
assert(blockedHtml.includes('#6'), "Should contain blocker ID");
assert(blockedHtml.includes('Surface PR checks on the Review card'), "Should contain human-readable blocker title");
assert(blockedHtml.includes('backlog'), "Should contain state cue");

// Test 2: Unblocked card is empty when unblocked (no blocker chips rendered)
const unblockedItem = {
  ...blockedItem,
  id: 8,
  blocked_by: [],
  blockers: [],
};

const unblockedHtml = renderToString(
  React.createElement(Card, {
    item: unblockedItem,
    column: "backlog",
    now,
    agentTimeout: 600,
    onOpen: () => {},
  })
);

assert(!unblockedHtml.includes("blocker-chips"), "Unblocked card should be empty when unblocked (no blocker chips)");

// Test 3: Running card with engine null and defaultEngine agy shows agy badge
const runningItem = {
  ...unblockedItem,
  id: 9,
  state: "running",
  engine: null,
  model: "claude-opus-5",
  progress: 0.5,
  lease: {
    agent_id: "agent-1",
    granted_at: new Date().toISOString(),
    last_heartbeat: new Date().toISOString(),
    expires_at: new Date(now + 600_000).toISOString(),
  },
  run_deadline_at: new Date(now + 600_000).toISOString(),
};

const runningAgyHtml = renderToString(
  React.createElement(Card, {
    item: runningItem,
    column: "running",
    now,
    agentTimeout: 600,
    defaultEngine: "agy",
    onOpen: () => {},
  })
);

console.log("\nRunning Card HTML (defaultEngine=agy):\n", runningAgyHtml);
assert(runningAgyHtml.includes("agy"), "Running card with engine null and defaultEngine agy should render agy badge");
assert(runningAgyHtml.includes("data-testid=\"card-model-badge\""), "Running card should show resolved model badge when known");
assert(runningAgyHtml.includes("claude-opus-5"), "Running card model badge should show card.model override");

// Test 4: Running card with engine null and defaultEngine claude shows claude badge
const runningClaudeHtml = renderToString(
  React.createElement(Card, {
    item: runningItem,
    column: "running",
    now,
    agentTimeout: 600,
    defaultEngine: "claude",
    onOpen: () => {},
  })
);

console.log("\nRunning Card HTML (defaultEngine=claude):\n", runningClaudeHtml);
assert(runningClaudeHtml.includes("claude"), "Running card with engine null and defaultEngine claude should render claude badge");

// Test 4b: Running card shows spec-resolved model when card.model unset
const runningSpecModelHtml = renderToString(
  React.createElement(Card, {
    item: {
      ...runningItem,
      id: 11,
      model: null,
      resolved_model: "gemini-3.6-flash-high",
      engine: "agy",
    },
    column: "running",
    now,
    agentTimeout: 600,
    defaultEngine: "agy",
    onOpen: () => {},
  }),
);
assert(runningSpecModelHtml.includes("data-testid=\"card-model-badge\""), "Card shows model badge from resolved_model");
assert(runningSpecModelHtml.includes("gemini-3.6-flash-high"), "Card model badge shows spec-resolved model");

// Test 5: isBlocked helper
assert.strictEqual(isBlocked(blockedItem), true, "blockedItem should be blocked");
assert.strictEqual(isBlocked(unblockedItem), false, "unblockedItem should be unblocked");

const resolvedBlockerItem = {
  ...blockedItem,
  id: 10,
  blocked_by: [6],
  blockers: [{ id: 6, title: "Supervisor runs the gates", state: "done" }],
};
assert.strictEqual(isBlocked(resolvedBlockerItem), false, "Item with done blocker should be unblocked");

// Test 6: Backlog column sorts claimable cards first, including after claim-release bounce
const oldDate = new Date(Date.now() - 3600 * 1000).toISOString();
const olderDate = new Date(Date.now() - 7200 * 1000).toISOString();

const card1_unblocked = {
  ...unblockedItem,
  id: 1,
  title: "Unblocked Card 1",
  entered_state_at: oldDate,
};

const card2_blocked = {
  ...blockedItem,
  id: 2,
  title: "Blocked Card 2",
  entered_state_at: olderDate, // older timestamp
};

const card3_blocked = {
  ...blockedItem,
  id: 3,
  title: "Blocked Card 3",
  entered_state_at: olderDate,
};

const card4_blocked = {
  ...blockedItem,
  id: 4,
  title: "Blocked Card 4",
  entered_state_at: olderDate,
};

const card5_blocked = {
  ...blockedItem,
  id: 5,
  title: "Blocked Card 5",
  entered_state_at: olderDate,
};

// Backlog column sorting: Card 1 (unblocked) must sort before Cards 2..5 (blocked)
let readyCards = [card2_blocked, card3_blocked, card4_blocked, card5_blocked, card1_unblocked];
readyCards.sort(sortFor("backlog"));
assert.strictEqual(readyCards[0].id, 1, "Unblocked card #1 must sort first");

// Claim -> Release bounce: Card 1 is claimed and then released back to Backlog.
// Its entered_state_at refreshes to NOW (newest timestamp).
card1_unblocked.entered_state_at = new Date().toISOString();

readyCards = [card2_blocked, card3_blocked, card4_blocked, card5_blocked, card1_unblocked];
readyCards.sort(sortFor("backlog"));

assert.strictEqual(readyCards[0].id, 1, "After claim-release bounce, unblocked card #1 must STILL sort first");

// Test 7: Board surfaces Needs you action cards with humanized copy
const projectItem = {
  id: 100,
  parent: null,
  level: "Project",
  title: "Test Project",
  intent: "Test project intent",
  definition_of_done: "Done",
  state: "backlog",
  origin: { kind: "human" },
  above_line: true,
  blocked_by: [],
  blockers: [],
  capability: null,
  lease: null,
  model: null,
  progress: 0,
  escalation: null,
  gates: [],
  gate_failures: 0,
  diff_added: 0,
  diff_removed: 0,
  notes: [],
  pinned: [],
  release_target: null,
  environment: null,
  pull_request: null,
  created_at: new Date().toISOString(),
  entered_state_at: new Date().toISOString(),
  history: [],
};

const needsYouItem = {
  ...blockedItem,
  id: 9,
  parent: 100,
  state: "needs_human",
  blocked_by: [],
  blockers: [],
  escalation: {
    question:
      "Task failed to run 3 times. Last failure: clone failed: fatal: unable to access 'https://example.com/x.git/': CONNECT tunnel failed, response 403",
    options: [
      { label: "Investigate the environment", detail: "infra" },
      { label: "Cut scope", detail: "drop" },
    ],
    recommended: 0,
    blocked_since: new Date(Date.now() - 3600_000).toISOString(),
    answer: null,
  },
};

const boardHtml = renderToString(
  React.createElement(Board, {
    items: new Map([
      [100, projectItem],
      [9, needsYouItem],
    ]),
    goals: [
      {
        id: 100,
        title: "Test Project",
        intent: "Test",
        progress: 0,
        leaves_done: 0,
        leaves_total: 1,
        agents_live: 0,
        needs_you: 1,
        plan_status: "approved_v1",
        columns: [],
        story: [],
      },
    ],
    stories: new Map(),
    goalOf: () => 100,
    breadcrumbOf: () => "Test Project",
    now,
    agentTimeout: 600,
    onOpen: () => {},
    onChanged: () => {},
  })
);

console.log("\nBoard HTML:\n", boardHtml.slice(0, 800));
assert(boardHtml.includes("board-needs"), "Board should show Needs you section");
assert(
  boardHtml.includes("Sandbox couldn") && boardHtml.includes("clone"),
  "Board Needs you should humanize clone failures",
);
assert(boardHtml.includes("Investigate the environment"), "Board should offer answer options");
// Create Project entry on non-empty board (collapsible trigger; form closed).
assert(boardHtml.includes('data-testid="create-project"'),
  "Populated board exposes Create Project root");
assert(boardHtml.includes('data-testid="create-project-open"'),
  "Populated board shows Create Project open control");
assert(boardHtml.includes("Create Project"),
  "Populated board Create Project affordance copy");
assert(!boardHtml.includes('data-testid="create-project-form"'),
  "Populated board keeps Create Project form collapsed until opened");
// Create Task on Project swimlane (not Welcome) — collapsible trigger when lane is open.
assert(boardHtml.includes('data-testid="create-task"'),
  "Populated board Project lane exposes Create Task root");
assert(boardHtml.includes('data-testid="create-task-open"'),
  "Populated board shows Create Task open control");
assert(boardHtml.includes("Create Task"),
  "Populated board Create Task affordance copy");
assert(!boardHtml.includes('data-testid="create-task-form"'),
  "Populated board keeps Create Task form collapsed until opened");
assert(boardHtml.includes('data-testid="create-task-open"') &&
  boardHtml.indexOf('data-testid="create-task"') >
    boardHtml.indexOf('data-testid="create-project"'),
  "Create Task lives under Project lanes, separate from Create Project");

// Test 8: Detail Head renders Archive and Delete actions

const headHtml = renderToString(
  React.createElement(Head, {
    title: "#100 Test Project",
    onClose: () => {},
    onArchive: () => {},
    onDelete: () => {},
  })
);

console.log("\nDetail Head HTML:\n", headHtml);
assert(headHtml.includes("📦 Archive"), "Detail Head should offer Archive action button");
assert(headHtml.includes("🗑 Delete"), "Detail Head should offer Delete action button");
assert(!headHtml.includes("data-testid=\"drawer-unarchive\""),
  "Detail Head should not offer Unarchive when onUnarchive is omitted");

const headUnarchiveHtml = renderToString(
  React.createElement(Head, {
    title: "#101 Archived Project",
    onClose: () => {},
    onUnarchive: () => {},
    onDelete: () => {},
  })
);
assert(headUnarchiveHtml.includes("data-testid=\"drawer-unarchive\""),
  "Detail Head should offer Unarchive for retired Projects");
assert(headUnarchiveHtml.includes("Unarchive"), "Detail Head Unarchive label");
assert(!headUnarchiveHtml.includes("📦 Archive"),
  "Detail Head should not offer Archive when only onUnarchive is set");

// Test 9: Detail Plan editor renders plan task blocker selection UI
const samplePlanTasksSpec = [
  { key: "t1", title: "Setup Database", intent: "Setup DB intent", definition_of_done: "DB ready", blocked_by_keys: [] },
  { key: "t2", title: "Build API", intent: "Build API intent", definition_of_done: "API ready", blocked_by_keys: ["t1"] },
];

const editPlanTasks = planTasksFromArtifact(samplePlanTasksSpec);

const planEditorHtml = renderToString(
  React.createElement(PlanEditor, {
    planTasks: editPlanTasks,
    setPlanTasks: () => {},
  })
);

console.log("\nPlan Editor HTML:\n", planEditorHtml);
assert(planEditorHtml.includes("Blocked by tasks:"), "Plan editor should render 'Blocked by tasks:' label");
assert(planEditorHtml.includes("Setup Database"), "Blocker chip for t1 should display human readable sibling task title");
assert(planEditorHtml.includes("+ Select blocker task..."), "Plan editor should offer '+ Select blocker task...' dropdown to select sibling tasks");

// Test 10: reduce tracks lastSeenSeq from Snapshot and BoardEvent
let s = reduce(initial, {
  type: "snapshot",
  snap: {
    items: [unblockedItem],
    levels: [],
    goals: [],
    server_time: new Date().toISOString(),
    agent_timeout_secs: 1800,
    seq: 10,
  },
});
assert.strictEqual(s.lastSeenSeq, 10, "Snapshot seq 10 should update lastSeenSeq to 10");

const liveItem = { ...unblockedItem, title: "Updated Live by Event" };
s = reduce(s, {
  type: "event",
  ev: {
    type: "upsert",
    seq: 11,
    item: liveItem,
  },
});
assert.strictEqual(s.lastSeenSeq, 11, "BoardEvent seq 11 should update lastSeenSeq to 11");
assert.strictEqual(s.items.get(8).title, "Updated Live by Event", "Upsert event should update item in state");

// Test 11: Stale REST snapshot race protection (only while connected + small gap)
s = reduce(s, { type: "connected", ok: true });
const beforeStaleLoadAt = s.lastLoadedAt;
const staleSnap = {
  items: [{ ...unblockedItem, title: "Stale REST Snapshot Title" }],
  levels: [],
  goals: [],
  server_time: new Date().toISOString(),
  agent_timeout_secs: 1800,
  seq: 9, // older sequence number than lastSeenSeq=11
};

const sAfterStaleSnap = reduce(s, { type: "snapshot", snap: staleSnap });
assert.strictEqual(sAfterStaleSnap.lastSeenSeq, 11, "Stale snapshot (seq 9 < 11) must not lower lastSeenSeq");
assert.strictEqual(
  sAfterStaleSnap.items.get(8).title,
  "Updated Live by Event",
  "Stale REST snapshot (seq 9 < 11) must not overwrite newer live event state"
);
assert.ok(
  sAfterStaleSnap.lastLoadedAt != null &&
    (beforeStaleLoadAt == null || sAfterStaleSnap.lastLoadedAt >= beforeStaleLoadAt),
  "Successful REST during a tiny race must still refresh lastLoadedAt (retry / NOT LIVE)"
);

// Test 11b: After disconnect (or sandboard restart seq rewind), REST snapshot wins
const sDisconnected = reduce(sAfterStaleSnap, { type: "connected", ok: false });
const rewoundSnap = {
  items: [{ ...unblockedItem, title: "Post-restart Snapshot" }],
  levels: [],
  goals: [],
  server_time: new Date().toISOString(),
  agent_timeout_secs: 1800,
  seq: 2,
};
const sAfterRewind = reduce(sDisconnected, { type: "snapshot", snap: rewoundSnap });
assert.strictEqual(sAfterRewind.lastSeenSeq, 2, "Disconnected retry must accept rewound server seq");
assert.strictEqual(
  sAfterRewind.items.get(8).title,
  "Post-restart Snapshot",
  "Disconnected retry must apply REST after sandboard restart"
);

// Test 11c: reset event rewinds high-water mark so later events apply
const sAfterReset = reduce(sAfterStaleSnap, { type: "event", ev: { type: "reset", seq: 3 } });
assert.strictEqual(sAfterReset.lastSeenSeq, 3, "reset event must rewind lastSeenSeq");

// Test 12: Stale/duplicate BoardEvent ignored
const staleEvent = {
  type: "upsert",
  seq: 10, // older than lastSeenSeq=11
  item: { ...unblockedItem, title: "Duplicate/Stale Event Title" },
};
const sAfterStaleEvent = reduce(s, { type: "event", ev: staleEvent });
assert.strictEqual(sAfterStaleEvent.lastSeenSeq, 11, "Stale event (seq 10 <= 11) must keep lastSeenSeq at 11");
assert.strictEqual(
  sAfterStaleEvent.items.get(8).title,
  "Updated Live by Event",
  "Stale event (seq 10 <= 11) must not overwrite newer state"
);

// Test 13: Sequence Gap detection helper
assert.strictEqual(isSequenceGap(11, 12), false, "Sequential event (12 after 11) is not a gap");
assert.strictEqual(isSequenceGap(11, 14), true, "Event with gap (14 after 11) is detected as sequence gap");
assert.strictEqual(isSequenceGap(0, 5), false, "Initial event with lastSeenSeq=0 is not a gap");

// Test 14: reduceDetail updates card Detail drawer state live upon receiving Upsert event
const detailInitial = {
  ...unblockedItem,
  id: 7,
  title: "Initial Card Title",
  state: "running",
  notes: [{ author: "human", text: "initial note" }],
  ancestry: [{ level: "Project", title: "Parent Project", intent: "project intent" }],
  children: [10, 11],
};

const upsertEv = {
  type: "upsert",
  seq: 20,
  item: {
    ...unblockedItem,
    id: 7,
    title: "Updated Card Title Live",
    state: "review",
    pull_request: { url: "https://github.com/sandboard-app/sandboard/pull/186" },
    notes: [
      { author: "human", text: "initial note" },
      { author: "agent", text: "PR opened" },
    ],
  },
};

const updatedDetail = reduceDetail(detailInitial, upsertEv, 7);
assert.strictEqual(updatedDetail.title, "Updated Card Title Live", "Upsert event for id 7 must update detail title live");
assert.strictEqual(updatedDetail.state, "review", "Upsert event for id 7 must update detail state live");
assert.strictEqual(updatedDetail.pull_request?.url, "https://github.com/sandboard-app/sandboard/pull/186", "Upsert event for id 7 must update pull_request.url live");
assert.strictEqual(updatedDetail.notes.length, 2, "Upsert event for id 7 must update notes live");
assert.strictEqual(updatedDetail.ancestry.length, 1, "reduceDetail must preserve existing detail ancestry");

// Upsert event for a different card ID does not alter Detail state for card 7
const otherUpsertEv = {
  type: "upsert",
  seq: 21,
  item: { ...unblockedItem, id: 99, title: "Unrelated Card" },
};
const unchangedDetail = reduceDetail(updatedDetail, otherUpsertEv, 7);
assert.strictEqual(unchangedDetail.title, "Updated Card Title Live", "Upsert event for different id 99 must not modify detail for id 7");

// Delete event for card 7 clears detail
const deleteEv = { type: "delete", seq: 22, id: 7 };
const deletedDetail = reduceDetail(updatedDetail, deleteEv, 7);
assert.strictEqual(deletedDetail, null, "Delete event for matching id 7 must clear detail");

// Test 14b: reduceDetail applies story events for the card's goal and preserves steer notes
const mainAdvancedSteerNote =
  "Main advanced (refs/heads/main @ deadbeefabc). First action: fetch upstream main and rebase this card's branch onto upstream/main (not origin/main alone — the fork's base freezes at create time), then continue the card.";
const steerUpsertEv = {
  type: "upsert",
  seq: 24,
  item: {
    ...unblockedItem,
    id: 7,
    title: "Live Card",
    state: "backlog",
    awaiting_dispatch: true,
    notes: [{ author: "human", text: mainAdvancedSteerNote }],
  },
};
const steerDetail = reduceDetail(detailInitial, steerUpsertEv, 7);
assert.strictEqual(
  steerDetail.notes.at(-1)?.text,
  mainAdvancedSteerNote,
  "Detail drawer must show main-advanced steer note on live upsert",
);
assert(
  steerDetail.notes.at(-1)?.text.toLowerCase().includes("rebase"),
  "Main-advanced steer note must mention rebase",
);

const goalId = 1;
const storyEv = {
  type: "story",
  seq: 25,
  goal: goalId,
  at: new Date().toISOString(),
  text: "Live Card: refs/heads/main @ deadbeefabc advanced — live run interrupted for rebase (auto-steered; fetch upstream main and continue).",
};
const storyDetail = reduceDetail(steerDetail, storyEv, 7, goalId);
assert.strictEqual(storyDetail.story?.length, 1, "Story event for goal must append to detail story");
assert(
  storyDetail.story?.[0]?.text.includes("auto-steered"),
  "Goal story must describe main-advanced auto-steer",
);
assert(
  storyDetail.story?.[0]?.text.includes("interrupted for rebase"),
  "Goal story must name live-run rebase interrupt",
);

const unrelatedStoryEv = { ...storyEv, seq: 26, goal: 99, text: "Other goal story" };
const unchangedStoryDetail = reduceDetail(storyDetail, unrelatedStoryEv, 7, goalId);
assert.strictEqual(
  unchangedStoryDetail.story?.length,
  1,
  "Story events for other goals must not alter this card's detail story",
);

// Test 15: subscribeBoardEvents and emitBoardEvent live drawer subscription
let receivedEvent = null;
const unsubscribe = subscribeBoardEvents((ev) => {
  receivedEvent = ev;
});

emitBoardEvent(upsertEv);
assert.deepStrictEqual(receivedEvent, upsertEv, "subscribeBoardEvents listener must receive emitted board event");

// Unsubscribe cleanly removes listener
receivedEvent = null;
unsubscribe();

const nextEv = { type: "delete", seq: 23, id: 88 };
emitBoardEvent(nextEv);
// Test 16: WebSocket subscribe and ping/pong message protocol
let mockSent = [];
class MockWebSocket {
  constructor(url) {
    this.url = url;
    this.readyState = 1;
  }
  send(data) {
    mockSent.push(data);
  }
  close() {
    if (this.onclose) this.onclose();
  }
}

const mockWs = new MockWebSocket("ws://board.example/api/ws");
const subPayload = JSON.stringify({ type: "subscribe", last_seq: 15 });
mockWs.send(subPayload);
assert.strictEqual(mockSent.length, 1, "Mock WebSocket send must record sent message");
assert(mockSent[0].includes('"type":"subscribe"') && mockSent[0].includes('"last_seq":15'), "Subscribe message must match required protocol");

const pingPayload = JSON.stringify({ type: "ping" });
const parsedPing = JSON.parse(pingPayload);
assert.strictEqual(parsedPing.type, "ping", "Ping frame type must be ping");

// Test 17: App chrome — Board | Help | Settings in sidebar; account menu keeps Settings
const sidebarHtml = renderToString(
  React.createElement(PrimarySidebar, {
    view: "board",
    onNavigate: () => {},
  }),
);
assert(sidebarHtml.includes("data-testid=\"app-sidebar\""), "App should render primary sidebar");
assert(sidebarHtml.includes("Board"), "Sidebar should include Board nav");
assert(sidebarHtml.includes("Help"), "Sidebar should include Help nav");
assert(sidebarHtml.includes("Settings"), "Sidebar should include Settings nav");
assert(sidebarHtml.includes("data-testid=\"nav-board\""), "Sidebar should expose Board control");
assert(sidebarHtml.includes("data-testid=\"nav-help\""), "Sidebar should expose Help control");
assert(
  sidebarHtml.includes("data-testid=\"nav-settings\""),
  "Sidebar should expose Settings control",
);
assert(
  !sidebarHtml.includes("data-testid=\"nav-cockpit\""),
  "Cockpit must not live in primary nav",
);
assert(!sidebarHtml.includes("Cockpit"), "Sidebar must not list Cockpit");

const sidebarSettingsHtml = renderToString(
  React.createElement(PrimarySidebar, {
    view: "settings",
    onNavigate: () => {},
  }),
);
assert(
  sidebarSettingsHtml.includes('aria-current="page"') &&
    sidebarSettingsHtml.includes("data-testid=\"nav-settings\""),
  "Settings nav marks active when view is settings",
);

const accountHtml = renderToString(
  React.createElement(AccountMenu, {
    login: "shanemcd",
    themePref: "dark",
    onThemeChange: () => {},
    onOpenSettings: () => {},
    onLogout: () => {},
    defaultOpen: true,
  }),
);
assert(accountHtml.includes("data-testid=\"auth-user\""), "Account menu trigger shows user");
assert(accountHtml.includes("shanemcd"), "Account menu shows login");
assert(accountHtml.includes("data-testid=\"account-menu\""), "Account menu panel opens");
assert(accountHtml.includes("Settings"), "Account menu still exposes Settings as a secondary path");
assert(
  accountHtml.includes("data-testid=\"account-menu-settings\""),
  "Account menu Settings uses a distinct test id from sidebar nav-settings",
);
assert(accountHtml.includes("data-testid=\"auth-logout\""), "Sign out lives in the account menu");
assert(accountHtml.includes("Theme"), "Account menu includes theme switcher");
assert(accountHtml.includes("Dark"), "Account menu theme select includes Dark");

const toggleClosedHtml = renderToString(
  React.createElement(CockpitToggle, { open: false, onToggle: () => {} }),
);
assert(toggleClosedHtml.includes("data-testid=\"cockpit-toggle\""), "Top bar exposes Cockpit toggle");
assert(toggleClosedHtml.includes("cockpit-bar-btn"), "Toggle uses top-bar grip chrome");
assert(toggleClosedHtml.includes("cockpit-bar-icon"), "Grip uses chevron SVG icon");
assert(toggleClosedHtml.includes("<svg"), "Grip renders an SVG chevron");
assert(
  toggleClosedHtml.includes('aria-expanded="false"') ||
    !toggleClosedHtml.includes('aria-expanded="true"'),
  "Closed toggle is not expanded",
);

const toggleOpenHtml = renderToString(
  React.createElement(CockpitToggle, { open: true, onToggle: () => {} }),
);
assert(toggleOpenHtml.includes("cockpit-bar-btn open"), "Open toggle marks open class");
assert(toggleOpenHtml.includes("cockpit-bar-icon"), "Open grip keeps chevron icon");
assert(
  toggleOpenHtml.includes('aria-expanded="true"') ||
    toggleOpenHtml.includes('aria-expanded=""'),
  "Open toggle is expanded",
);

const dropClosedHtml = renderToString(React.createElement(CockpitDrop, { open: false }));
assert.equal(
  dropClosedHtml,
  "",
  "Drop stays unmounted until first open (collapse later keeps it mounted client-side)",
);

const dropOpenHtml = renderToString(React.createElement(CockpitDrop, { open: true }));
assert(dropOpenHtml.includes("data-testid=\"cockpit-drop\""), "Open drop mounts under the top bar");
assert(dropOpenHtml.includes("data-testid=\"cockpit-pane\""), "Open drop mounts Cockpit pane");
// `open` class is applied after rAF so the slide can run — not present in SSR.

const cockpitHtml = renderToString(React.createElement(Cockpit));
assert(cockpitHtml.includes("data-testid=\"cockpit-pane\""), "Cockpit renders as drop pane");
assert(!cockpitHtml.includes("data-testid=\"cockpit-page\""), "Cockpit is not a separate page");
assert(cockpitHtml.includes("data-testid=\"cockpit-attach\""), "Cockpit should show cockpit attach surface");
assert(cockpitHtml.includes("data-testid=\"cockpit-term-window\""), "Cockpit should show terminal chrome");
assert(cockpitHtml.includes("data-testid=\"cockpit-xterm\""), "Cockpit should mount xterm host");
assert(cockpitHtml.includes("data-testid=\"cockpit-session\""), "Cockpit should show Start/Stop strip");
assert(cockpitHtml.includes("data-testid=\"cockpit-session-start\""), "Cockpit should expose Start");
assert(cockpitHtml.includes("data-testid=\"cockpit-session-stop\""), "Cockpit should expose Stop");
assert(!cockpitHtml.includes("data-testid=\"cockpit-open-cursor\""), "Cockpit should not shell out Open in Cursor");
assert(!cockpitHtml.includes("data-testid=\"cockpit-mcp-provision\""), "Cockpit should not expose Refresh MCP");
assert(!cockpitHtml.includes("data-testid=\"cockpit-mcp-status\""), "Cockpit should not dump MCP status");
assert(!cockpitHtml.includes("data-testid=\"cockpit-session-status\""), "Cockpit should not dump session status");
assert(!cockpitHtml.includes("data-testid=\"cockpit-session-park\""), "Cockpit should not expose Park");
assert(!cockpitHtml.includes("data-testid=\"cockpit-session-resume\""), "Cockpit should not expose Resume");
assert(!cockpitHtml.includes("/api/cockpit-attach"), "Cockpit should not show attach API lede");
assert(
  cockpitHtml.indexOf("data-testid=\"cockpit-session\"") <
    cockpitHtml.indexOf("data-testid=\"cockpit-attach\""),
  "Start/Stop precede the attach window in the drop",
);

// CockpitSessionView — Start/Stop only; no status dump
const noop = () => {};
const buttonTag = (html, testId) => {
  // React SSR may place attrs in any order — match the whole opening tag.
  const all = html.match(/<button\b[^>]*>/g) || [];
  const tag = all.find((t) => t.includes(`data-testid="${testId}"`));
  assert(tag, `missing button ${testId}`);
  return tag;
};
const isDisabled = (html, testId) => /\bdisabled\b/.test(buttonTag(html, testId));

const absentHtml = renderToString(
  React.createElement(CockpitSessionView, {
    session: null,
    onStart: noop,
    onStop: noop,
  }),
);
assert(!absentHtml.includes("data-testid=\"cockpit-session-status\""), "No status dump when absent");
assert(!absentHtml.includes("data-testid=\"cockpit-session-phase\""), "No phase strip when absent");
assert(!isDisabled(absentHtml, "cockpit-session-start"), "Start enabled when no session");
assert(isDisabled(absentHtml, "cockpit-session-stop"), "Stop disabled when no session");

const runningSession = {
  environment: "sandboard-cockpit",
  conversation_id: "conv-cockpit-1",
  status: "running",
  sandbox_phase: "ready",
  phase_since: new Date().toISOString(),
  created_at: new Date().toISOString(),
  updated_at: new Date().toISOString(),
};
const runningHtml = renderToString(
  React.createElement(CockpitSessionView, {
    session: runningSession,
    onStart: noop,
    onStop: noop,
  }),
);
assert(!runningHtml.includes("sandboard-cockpit"), "Session strip does not dump environment");
assert(!runningHtml.includes("conv-cockpit-1"), "Session strip does not dump conversation_id");
assert(runningHtml.includes("data-testid=\"cockpit-session-phase\""), "Phase strip when ready");
assert(runningHtml.includes("Ready"), "Ready phase label");
assert(isDisabled(runningHtml, "cockpit-session-start"), "Start disabled when Running");
assert(!isDisabled(runningHtml, "cockpit-session-stop"), "Stop enabled when Running");
assert(!runningHtml.includes("data-testid=\"cockpit-open-cursor\""), "No Open in Cursor button");
assert(!runningHtml.includes("openshell sandbox connect"), "No host TTY hint in Cockpit");

const reclaimSession = {
  ...runningSession,
  environment: null,
  sandbox_phase: "waiting_for_delete",
  phase_detail: "Waiting for previous sandbox to finish deleting",
  phase_since: new Date(Date.now() - 38_000).toISOString(),
};
const reclaimHtml = renderToString(
  React.createElement(CockpitSessionView, {
    session: reclaimSession,
    nowMs: Date.now(),
    onStart: noop,
    onStop: noop,
  }),
);
assert(reclaimHtml.includes("Waiting for previous sandbox"), "Reclaim detail in phase strip");
assert(reclaimHtml.includes("38s") || reclaimHtml.includes("37s") || reclaimHtml.includes("39s"), "Elapsed seconds on reclaim");

const parkedSession = {
  ...runningSession,
  status: "parked",
  conversation_id: null,
};
const parkedHtml = renderToString(
  React.createElement(CockpitSessionView, {
    session: parkedSession,
    onStart: noop,
    onStop: noop,
  }),
);
assert(isDisabled(parkedHtml, "cockpit-session-start"), "Start disabled while a session exists");
assert(!isDisabled(parkedHtml, "cockpit-session-stop"), "Stop enabled when Parked");
assert(!parkedHtml.includes("data-testid=\"cockpit-session-park\""), "No Park control");
assert(!parkedHtml.includes("data-testid=\"cockpit-session-resume\""), "No Resume control");

// Cockpit attach — gated by Board session; xterm host present when attachable
const attachAbsent = renderToString(
  React.createElement(CockpitAttachView, {
    canAttach: false,
    disabledReason: "Start a cockpit session to open the terminal.",
  }),
);
assert(attachAbsent.includes("data-testid=\"cockpit-attach\""), "Attach root");
assert(attachAbsent.includes("data-testid=\"cockpit-term-window\""), "Terminal chrome");
assert(attachAbsent.includes("data-testid=\"cockpit-attach-gate\""), "Gate copy when disabled");
assert(attachAbsent.includes("Start a cockpit session"), "Absent gate explains Start");

const attachParked = renderToString(
  React.createElement(CockpitAttachView, {
    canAttach: false,
    disabledReason: "Cockpit session is parked. Stop it, then Start again.",
  }),
);
assert(attachParked.includes("Stop it, then Start again"), "Parked gate explains Stop+Start");

const attachRunning = renderToString(
  React.createElement(CockpitAttachView, {
    canAttach: true,
    disabledReason: null,
    environment: "sandboard-cockpit",
    sessionStatus: "running",
  }),
);
assert(attachRunning.includes("data-testid=\"cockpit-xterm\""), "xterm host present");
assert(attachRunning.includes("sandboard-cockpit"), "Title bar shows environment");
assert(!attachRunning.includes("data-testid=\"cockpit-attach-gate\""), "No gate when attachable");

// Attach reconnect backoff (sandboard restart must not stick on a dead socket)
assert.equal(cockpitAttachRetryDelayMs(0), 1000);
assert.equal(cockpitAttachRetryDelayMs(1), 2000);
assert.equal(cockpitAttachRetryDelayMs(5), 15_000);
assert.equal(cockpitAttachRetryDelayMs(9), 15_000, "backoff caps at 15s");

// Gate helpers
assert.deepEqual(cockpitAttachGate(null), {
  canAttach: false,
  reason: "Start a cockpit session to open the terminal.",
});
assert.equal(cockpitAttachGate(parkedSession).canAttach, false);
assert.match(cockpitAttachGate(parkedSession).reason, /Stop it, then Start again/);
assert.equal(cockpitAttachGate(runningSession).canAttach, true);
assert.equal(cockpitAttachGate(runningSession).reason, null);
assert.equal(
  cockpitAttachGate({ ...runningSession, environment: null }).canAttach,
  false,
);
assert.equal(cockpitAttachGate(reclaimSession).canAttach, false);
assert.match(
  cockpitAttachGate(reclaimSession).reason,
  /previous sandbox to finish deleting/,
);
assert.match(
  cockpitAttachGate({
    ...runningSession,
    environment: null,
    sandbox_phase: "provisioning",
    phase_detail: "Creating cockpit sandbox",
  }).reason,
  /Creating cockpit sandbox/,
);
assert.equal(cockpitPhaseLabel(reclaimSession), "Waiting for previous sandbox to finish deleting");
assert.equal(cockpitPollIntervalMs(reclaimSession), 1000);
assert.equal(cockpitPollIntervalMs(runningSession), 4000);
assert.equal(cockpitPollIntervalMs(null), 4000);
assert.deepEqual(cockpitBarChip(reclaimSession), { text: "reclaiming", busy: true });
assert.deepEqual(cockpitBarChip(runningSession), { text: "ready", busy: false });
assert.equal(cockpitBarChip(null), null);
const chipHtml = renderToString(
  React.createElement(CockpitToggle, {
    open: false,
    onToggle: noop,
    chip: { text: "reclaiming", busy: true },
  }),
);
assert(chipHtml.includes("data-testid=\"cockpit-bar-chip\""), "Bar chip renders");
assert(chipHtml.includes("reclaiming"), "Bar chip text");

// Vite :5173 → dial API :8080 (cookie is host-scoped; WSS through Vite+Tailscale stalls).
assert.equal(
  sandboardWsHost({ hostname: "tot.tail43beb.ts.net", port: "5173", host: "tot.tail43beb.ts.net:5173" }),
  "tot.tail43beb.ts.net:8080",
);
assert.equal(
  sandboardWsUrl("/api/cockpit-attach", {
    protocol: "https:",
    hostname: "tot.tail43beb.ts.net",
    port: "5173",
    host: "tot.tail43beb.ts.net:5173",
  }),
  "wss://tot.tail43beb.ts.net:8080/api/cockpit-attach",
);
assert.equal(
  sandboardWsUrl("/api/ws", {
    protocol: "https:",
    hostname: "sandboard.tail43beb.ts.net",
    port: "",
    host: "sandboard.tail43beb.ts.net",
  }),
  "wss://sandboard.tail43beb.ts.net/api/ws",
);

// Legacy alias
assert.equal(cockpitChatGate(runningSession).canSend, true);

const helpHtml = renderToString(React.createElement(Help));
assert(helpHtml.includes("data-testid=\"help-page\""), "Help view should render");
assert(helpHtml.includes("Welcome to sandboard"), "Help uses Welcome hero");
assert(helpHtml.includes('data-testid="help-welcome"'), "Help wraps welcome stack");
assert(!helpHtml.includes('data-testid="create-project"'), "Help must not expose Create Project");
assert(!helpHtml.includes('data-testid="create-project-form"'), "Help must not show Create Project form");
assert(helpHtml.includes("data-testid=\"openshell-readiness\""), "Help shows OpenShell readiness strip");
assert(helpHtml.includes("data-testid=\"openshell-readiness-gateway\""), "Help readiness: gateway row");
assert(helpHtml.includes("data-testid=\"openshell-readiness-sandbox\""), "Help readiness: sandbox row");
assert(helpHtml.includes("data-testid=\"operator-guide\""), "Help embeds OperatorGuide");
assert(helpHtml.includes("data-testid=\"operator-guide-quickstart\""), "Help shows OperatorGuide Quickstart");
assert(helpHtml.includes("data-testid=\"operator-guide-mcp\""), "Help shows OperatorGuide MCP section");
assert(helpHtml.includes("data-testid=\"operator-guide-openshell\""), "Help shows OperatorGuide OpenShell section");
assert(helpHtml.includes("create_project"), "Help should document create_project");
assert(helpHtml.includes("create_task"), "Help should document create_task");
assert(helpHtml.includes("Create Task"), "Help should name board Create Task");
assert(helpHtml.includes("clone_repo"), "Help should document clone_repo");
assert(helpHtml.includes("owner/name"), "Help labels clone_repo as owner/name");
assert(helpHtml.includes("on the board"), "Help mentions on-board Create Project");
assert(helpHtml.includes("plan.json"), "Help should document plan.json");
assert(helpHtml.includes("Approve"), "Help should document Approve");
assert(helpHtml.includes("dispatch"), "Help should document dispatch");
assert(helpHtml.includes("/mcp"), "Help should show MCP URL");
assert(!helpHtml.includes("127.0.0.1:8080"), "Help must not hardcode loopback:8080");
assert(helpHtml.includes("Streamable HTTP"), "Help should name Streamable HTTP transport");
assert(helpHtml.includes("Quickstart"), "Help includes Quickstart");
assert(helpHtml.includes("Connect MCP"), "Help includes Connect MCP");
assert(helpHtml.includes("Configuration layers"), "Help lede mentions configuration layers");
assert(helpHtml.includes("standing instructions"), "Help lede mentions standing instructions");
assert(helpHtml.includes("data-testid=\"operator-guide-config\""), "Help shows configuration section");
assert(helpHtml.includes("project_prompt"), "Help documents project_prompt");
{
  const readyIdx = helpHtml.indexOf('data-testid="openshell-readiness"');
  const guideIdx = helpHtml.indexOf('data-testid="operator-guide"');
  assert(
    readyIdx >= 0 && guideIdx > readyIdx,
    "Help orders OpenShell readiness before OperatorGuide",
  );
}
// Help surface order: Quickstart pillar before Configuration before MCP pillar.
{
  const helpQuickstartIdx = helpHtml.indexOf("data-testid=\"operator-guide-quickstart\"");
  const helpConfigIdx = helpHtml.indexOf("data-testid=\"operator-guide-config\"");
  const helpMcpIdx = helpHtml.indexOf("data-testid=\"operator-guide-mcp\"");
  assert(
    helpQuickstartIdx >= 0 && helpConfigIdx > helpQuickstartIdx,
    "Help orders Quickstart before Configuration",
  );
  assert(
    helpConfigIdx >= 0 && helpMcpIdx > helpConfigIdx,
    "Help orders Configuration before MCP",
  );
}

// OperatorGuide — Quickstart → MCP → OpenShell/sandbox (Board empty / Help)
const guideHtml = renderToString(React.createElement(OperatorGuide));
assert(guideHtml.includes("data-testid=\"operator-guide\""), "OperatorGuide root testid");
assert(guideHtml.includes("data-testid=\"operator-guide-quickstart\""), "OperatorGuide Quickstart section");
assert(guideHtml.includes("data-testid=\"operator-guide-quickstart-steps\""), "OperatorGuide Quickstart steps");
assert(guideHtml.includes("data-testid=\"operator-guide-mcp\""), "OperatorGuide MCP section");
assert(guideHtml.includes("data-testid=\"operator-guide-openshell\""), "OperatorGuide OpenShell/sandbox section");
assert(guideHtml.includes("data-testid=\"operator-guide-client-examples\""), "OperatorGuide client examples are secondary");
assert(guideHtml.includes("data-testid=\"operator-guide-mcp-url\""), "OperatorGuide copyable MCP URL");
assert(guideHtml.includes("data-testid=\"operator-guide-cursor-snippet\""), "OperatorGuide Cursor snippet");
assert(guideHtml.includes("data-testid=\"operator-guide-claude-snippet\""), "OperatorGuide Claude snippet");
assert(guideHtml.includes("/mcp"), "OperatorGuide shows MCP endpoint");
assert(!guideHtml.includes("127.0.0.1:8080"), "OperatorGuide must not hardcode loopback:8080");
assert(guideHtml.includes("Streamable HTTP"), "OperatorGuide names Streamable HTTP transport");
assert(guideHtml.includes("create_project"), "OperatorGuide documents create_project");
assert(guideHtml.includes("clone_repo"), "OperatorGuide documents clone_repo");
assert(guideHtml.includes("owner/name"), "OperatorGuide labels clone_repo as owner/name");
assert(guideHtml.includes("on the board"), "OperatorGuide mentions on-board Create Project");
assert(guideHtml.includes("plan.json"), "OperatorGuide documents plan.json");
assert(guideHtml.includes("Approve"), "OperatorGuide documents Approve");
assert(guideHtml.includes("idle"), "OperatorGuide notes agents stay idle until dispatch");
assert(
  guideHtml.includes('data-testid="operator-guide-create-task-note"'),
  "OperatorGuide documents ad-hoc Create Task on existing Projects",
);
assert(guideHtml.includes("create_task"), "OperatorGuide documents MCP create_task");
assert(guideHtml.includes("Create Task"), "OperatorGuide names board Create Task");
assert(
  guideHtml.includes("never merges") || guideHtml.includes("never merge"),
  "OperatorGuide keeps Approve ≠ merge for create-Task path",
);
assert(guideHtml.includes("claude mcp add"), "OperatorGuide has Claude mcp add example");
assert(guideHtml.includes("mcp.json"), "OperatorGuide has Cursor mcp.json example");
assert(guideHtml.includes("OpenShell + sandbox"), "OperatorGuide OpenShell section title");
assert(guideHtml.includes("/settings/openshell/connectivity"), "OpenShell deep link: Connectivity");
assert(guideHtml.includes("/settings/openshell/providers"), "OpenShell deep link: Providers");
assert(guideHtml.includes("/settings/openshell/policies"), "OpenShell deep link: Policies");
assert(guideHtml.includes("/settings/openshell/profiles"), "OpenShell deep link: Sandbox specs");
assert(guideHtml.includes("/settings/agent-runtime"), "OpenShell deep link: Agent runtime");
assert(guideHtml.includes("github-app"), "OperatorGuide mentions github-app provider");
assert(guideHtml.includes("GH_TOKEN"), "OperatorGuide mentions GH_TOKEN");
assert(guideHtml.includes("cursor-agent"), "OperatorGuide places github-app with other shipped types");
assert(guideHtml.includes("Policies"), "OperatorGuide names Policies tab");
assert(guideHtml.includes("Sandbox specs"), "OperatorGuide names Sandbox specs tab");
assert(guideHtml.includes("mTLS"), "OperatorGuide mentions mTLS on Connectivity");
assert(guideHtml.includes("data-testid=\"operator-guide-config\""), "OperatorGuide configuration section");
assert(guideHtml.includes("project_prompt"), "OperatorGuide documents project_prompt");
assert(
  guideHtml.includes("does not assume") && guideHtml.includes("cargo"),
  "OperatorGuide must not invent cargo gates",
);
assert(guideHtml.includes("quality gates"), "OperatorGuide documents quality gates");
// Order: Quickstart → Configuration → MCP (with examples) → OpenShell/sandbox.
const quickstartIdx = guideHtml.indexOf("data-testid=\"operator-guide-quickstart\"");
const configIdx = guideHtml.indexOf("data-testid=\"operator-guide-config\"");
const mcpIdx = guideHtml.indexOf("data-testid=\"operator-guide-mcp\"");
const openshellIdx = guideHtml.indexOf("data-testid=\"operator-guide-openshell\"");
const examplesIdx = guideHtml.indexOf("data-testid=\"operator-guide-client-examples\"");
assert(
  quickstartIdx >= 0 && configIdx > quickstartIdx,
  "OperatorGuide leads with Quickstart before Configuration",
);
assert(
  configIdx >= 0 && mcpIdx > configIdx,
  "OperatorGuide places Configuration before MCP",
);
assert(
  openshellIdx > mcpIdx,
  "OpenShell/sandbox follows MCP (after the two Help pillars)",
);
assert(examplesIdx > mcpIdx && examplesIdx < openshellIdx, "Client examples sit under MCP, before OpenShell");

const settingsHtml = renderToString(React.createElement(Settings));
assert(settingsHtml.includes("data-testid=\"settings\""), "Settings view should render");
assert(!settingsHtml.includes("data-testid=\"settings-nav-sandboxes\""), "Sandboxes nav item removed");
assert(settingsHtml.includes("data-testid=\"settings-nav-openshell\""), "Settings should nav to OpenShell");
assert(settingsHtml.includes("data-testid=\"settings-nav-openshell/mcp-servers\""), "Settings should nav to MCP servers");
assert(settingsHtml.includes("data-testid=\"settings-nav-github-app\""), "Settings should nav to GitHub App");
assert(settingsHtml.includes("data-testid=\"openshell-panel\""), "Default section is OpenShell");
assert(settingsHtml.includes("data-testid=\"openshell-subnav\""), "OpenShell has section subnav");
assert(settingsHtml.includes("data-testid=\"openshell-tab-profiles\""), "OpenShell tab for Profiles");
assert(settingsHtml.includes("data-testid=\"openshell-tab-policies\""), "OpenShell tab for Policies");
assert(!settingsHtml.includes("data-testid=\"openshell-tab-mcp-servers\""), "MCP servers is not an OpenShell tab");
assert(settingsHtml.includes("data-testid=\"openshell-connectivity\""), "Default OpenShell tab is Connectivity");
assert(settingsHtml.includes("Connectivity"), "Settings OpenShell names Connectivity");
assert(settingsHtml.includes("Forge"), "Settings should include Forge section");
assert(settingsHtml.includes("data-testid=\"settings-nav-workspace\""), "Settings should nav to Forge (workspace id)");
assert(!settingsHtml.includes("data-testid=\"settings-nav-repo-access\""), "Repo access removed — replaced by GitHub App");
assert(settingsHtml.includes("OpenShell"), "Settings should include OpenShell section");
assert(settingsHtml.includes("MCP servers"), "Settings should include MCP servers section");
assert(settingsHtml.includes("Agent runtime"), "Settings should include Agent runtime section");
assert(settingsHtml.includes("data-testid=\"settings-nav-agent-runtime\""), "Settings should nav to Agent runtime");
assert(!settingsHtml.includes("data-testid=\"general-stub\""), "General stub must be gone");
assert(!settingsHtml.includes("settings-stub-tag"), "Forge must not be a stub section");

const agentRuntimeHtml = renderToString(
  React.createElement(AgentRuntimePanelView, {
    draft: {
      engine: "agy",
      max_concurrent: 1,
      agent_timeout_secs: 1800,
      max_attempts: 3,
      sweep_interval_ms: 2000,
      standing_prompt: "Board policy here.",
    },
    onDraftChange: () => {},
    onSave: () => {},
  }),
);
assert(agentRuntimeHtml.includes("data-testid=\"agent-runtime-panel\""), "Agent runtime panel should render");
assert(agentRuntimeHtml.includes("data-testid=\"agent-runtime-field-engine\""), "Agent runtime engine field");
assert(!agentRuntimeHtml.includes("data-testid=\"agent-runtime-field-enabled\""), "Agents enabled checkbox removed");
assert(!agentRuntimeHtml.includes("data-testid=\"agent-runtime-field-providers\""), "Providers field removed");
assert(!agentRuntimeHtml.includes("data-testid=\"agent-runtime-field-vertex-location\""), "Vertex fields removed");
assert(!agentRuntimeHtml.includes("data-testid=\"agent-runtime-field-quality-gates\""), "Quality gates removed");
assert(!agentRuntimeHtml.includes("data-testid=\"agent-runtime-field-branch-prefix\""), "Branch prefix removed");
assert(agentRuntimeHtml.includes("data-testid=\"agent-runtime-field-standing-prompt\""), "Agent runtime standing prompt");
assert(agentRuntimeHtml.includes("data-testid=\"agent-runtime-field-sweep\""), "Agent runtime sweep interval");
assert(agentRuntimeHtml.includes("data-testid=\"agent-runtime-save\""), "Agent runtime save control");

const repoAccessHtml = renderToString(
  React.createElement(RepoAccessPanelView, {
    view: {
      refreshed_at: "2026-08-19T00:00:00Z",
      install_url: "https://github.com/settings/installations",
      token_installation_id: 99,
      installations: [
        {
          id: 99,
          account_login: "acme",
          account_type: "Organization",
          manage_url: "https://github.com/organizations/acme/settings/installations/99",
          repos: [
            {
              full_name: "acme/widgets",
              installation_id: 99,
              permissions: { push: "true", pull: "true" },
              last_seen_at: "2026-08-19T00:00:00Z",
            },
          ],
        },
      ],
    },
    onRefresh: () => {},
  }),
);
assert(repoAccessHtml.includes("data-testid=\"repo-access-panel\""), "Repo access panel should render");
assert(repoAccessHtml.includes("data-testid=\"repo-access-refresh\""), "Repo access refresh action");
assert(repoAccessHtml.includes("data-testid=\"repo-access-install-link\""), "Repo access GitHub install deep link");
assert(repoAccessHtml.includes("https://github.com/settings/installations"), "Repo access install URL");
assert(repoAccessHtml.includes("acme/widgets"), "Repo access lists cached repos");
assert(repoAccessHtml.includes("data-testid=\"repo-access-manage-99\""), "Per-installation manage link");

const openshellPanelProps = {
  gatewayEndpoint: "https://127.0.0.1:17670",
  authMode: "mtls",
  oidc: { issuer: "", client_id: "", audience: "" },
  caPem: "",
  clientCertPem: "",
  clientKeyPem: "",
  mtls: { ca: false, client_cert: false, client_key: false, complete: false },
  onGatewayEndpointChange: () => {},
  onAuthModeChange: () => {},
  onOidcChange: () => {},
  onCaPemChange: () => {},
  onClientCertPemChange: () => {},
  onClientKeyPemChange: () => {},
  onRefresh: () => {},
  onSave: () => {},
  onImportCliMtls: () => {},
  onClearMtls: () => {},
  onOidcLogin: () => {},
  onOidcLogout: () => {},
};

const openshellHtml = renderToString(
  React.createElement(OpenShellPanelView, {
    ...openshellPanelProps,
    status: {
      healthy: true,
      summary: "Connected\nAuthenticated (mTLS transport)",
      not_configured: false,
    },
  }),
);
assert(openshellHtml.includes("data-testid=\"openshell-panel\""), "OpenShell panel should render");
assert(openshellHtml.includes("data-testid=\"openshell-connectivity\""), "Connectivity band wrapper");
assert(
  openshellHtml.includes(">Connectivity<") || openshellHtml.includes("Connectivity</h3>"),
  "Connectivity heading",
);
assert(openshellHtml.includes("data-testid=\"openshell-health\""), "OpenShell health block");
assert(openshellHtml.includes("Healthy"), "OpenShell healthy label");
assert(openshellHtml.includes("data-testid=\"openshell-health-summary\""), "OpenShell status summary");
assert(openshellHtml.includes("data-testid=\"openshell-field-endpoint\""), "OpenShell gateway endpoint field");
assert(openshellHtml.includes("data-testid=\"openshell-field-ca\""), "OpenShell CA PEM field");
assert(!openshellHtml.includes("data-testid=\"openshell-field-binary\""), "OpenShell must not expose CLI binary path");
assert(!openshellHtml.includes("openshell-health-bin"), "Legacy binary health CSS class removed");
assert(openshellHtml.includes("data-testid=\"openshell-subnav\""), "OpenShell subnav for sections");
assert(
  openshellHtml.includes("data-testid=\"openshell-tab-connectivity\"") &&
    openshellHtml.includes("data-testid=\"openshell-tab-providers\"") &&
    openshellHtml.includes("data-testid=\"openshell-tab-provider-types\"") &&
    openshellHtml.includes("data-testid=\"openshell-tab-policies\"") &&
    openshellHtml.includes("data-testid=\"openshell-tab-profiles\"") &&
    !openshellHtml.includes("data-testid=\"openshell-tab-mcp-servers\""),
  "OpenShell tabs: Connectivity / Providers / Provider types / Policies / Sandbox specs",
);

const openshellUnhealthyHtml = renderToString(
  React.createElement(OpenShellPanelView, {
    ...openshellPanelProps,
    status: {
      healthy: false,
      summary: "gateway unreachable",
      not_configured: false,
    },
  }),
);
assert(openshellUnhealthyHtml.includes("Unhealthy"), "OpenShell unhealthy label");

const openshellOidcPasteHtml = renderToString(
  React.createElement(OpenShellPanelView, {
    ...openshellPanelProps,
    authMode: "oidc",
    oidc: {
      issuer: "https://idp.example/realms/openshell",
      client_id: "openshell-cli",
      audience: "openshell-cli",
    },
    oidcAwaitingPaste: true,
    oidcPaste: "",
    oidcRedirectUri: "http://127.0.0.1:48539/callback",
    oidcAuthorizeUrl: "https://idp.example/auth",
    onOidcPasteChange: () => {},
    onOidcCompletePaste: () => {},
    status: { healthy: false, summary: "", not_configured: false },
  }),
);
assert(
  openshellOidcPasteHtml.includes("data-testid=\"openshell-oidc-paste\""),
  "OIDC paste-back after Log in",
);
assert(
  openshellOidcPasteHtml.includes("data-testid=\"openshell-oidc-paste-url\""),
  "OIDC paste textarea",
);
assert(
  openshellOidcPasteHtml.includes("http://127.0.0.1:48539/callback"),
  "OIDC paste hint shows loopback redirect_uri",
);

const openshellProvidersHtml = renderToString(
  React.createElement(OpenShellProvidersPanelView, {
    providers: [
      {
        name: "gh-clankr",
        type: "github",
        config: {},
        credential_keys: ["GH_TOKEN"],
        has_credentials: true,
        has_refresh: false,
        gateway_synced: true,
      },
    ],
    gatewayReachable: true,
    profiles: [
      {
        id: "github",
        display_name: "GitHub",
        description: "",
        category: "scm",
        credential_env_vars: ["GH_TOKEN"],
        config_keys: [],
      },
    ],
    draft: null,
    onDraftChange: () => {},
    onSave: () => {},
    onCancelEdit: () => {},
    onEdit: () => {},
    onDelete: () => {},
    onSync: () => {},
  }),
);
assert(openshellProvidersHtml.includes("data-testid=\"openshell-providers\""), "Providers band renders");
assert(
  openshellProvidersHtml.includes(">Providers<") || openshellProvidersHtml.includes("Providers</h3>"),
  "Providers catalog heading",
);
assert(openshellProvidersHtml.includes("data-testid=\"openshell-provider-gh-clankr\""), "Provider row renders");
assert(openshellProvidersHtml.includes("data-testid=\"openshell-providers-sync\""), "Sync all control");
assert(!openshellProvidersHtml.includes("openshell-providers-import-adc"), "Import ADC control removed");
assert(openshellProvidersHtml.includes("on gateway"), "Gateway sync badge");
assert(!openshellProvidersHtml.includes("sk-"), "Providers view must not echo secrets");
assert(
  openshellProvidersHtml.includes("github-app"),
  "Providers intro mentions github-app type",
);
assert(
  !openshellProvidersHtml.includes("openshell-provider-attach-"),
  "Attach toggles live on Sandbox specs, not Providers",
);
assert(
  openshellProvidersHtml.includes("Sandbox spec"),
  "Providers copy points attach to Sandbox specs",
);

// github-app ships as a normal catalog row (type github-app).
const openshellManagedGithubHtml = renderToString(
  React.createElement(OpenShellProvidersPanelView, {
    providers: [
      {
        name: "github-app",
        type: "github-app",
        config: { GITHUB_APP_ID: "123" },
        credential_keys: ["GITHUB_APP_PRIVATE_KEY", "GH_TOKEN"],
        has_credentials: true,
        has_refresh: false,
        gateway_synced: true,
      },
      {
        name: "vertex",
        type: "google-vertex-ai",
        config: {},
        credential_keys: [],
        has_credentials: false,
        has_refresh: false,
        gateway_synced: true,
      },
    ],
    gatewayReachable: true,
    profiles: [
      {
        id: "github-app",
        display_name: "GitHub Application Access Token",
        description: "minted GH_TOKEN",
        source: "board",
        credential_env_vars: ["GH_TOKEN"],
        form_config_keys: ["GITHUB_APP_ID", "GITHUB_INSTALLATION_ID"],
      },
    ],
    draft: null,
    onDraftChange: () => {},
    onSave: () => {},
    onCancelEdit: () => {},
    onEdit: () => {},
    onDelete: () => {},
    onSync: () => {},
  }),
);
assert(
  openshellManagedGithubHtml.includes("data-testid=\"openshell-provider-github-app\""),
  "github-app lists in the Providers catalog",
);
assert(
  openshellManagedGithubHtml.includes("data-testid=\"openshell-provider-vertex\""),
  "Other providers still list in the catalog",
);

const githubAppDraftHtml = renderToString(
  React.createElement(OpenShellProvidersPanelView, {
    providers: [],
    gatewayReachable: true,
    profiles: [
      {
        id: "github-app",
        display_name: "GitHub Application Access Token",
        description: "minted GH_TOKEN",
        source: "board",
        credential_env_vars: ["GH_TOKEN"],
        form_config_keys: ["GITHUB_APP_ID", "GITHUB_INSTALLATION_ID"],
      },
    ],
    draft: {
      name: "github-app",
      type: "github-app",
      config: { GITHUB_APP_ID: "123" },
      credentials: {},
    },
    installations: [{ id: 99, account_login: "acme", account_type: "Organization" }],
    onRefreshInstallations: () => {},
    onDraftChange: () => {},
    onSave: () => {},
    onCancelEdit: () => {},
    onEdit: () => {},
    onDelete: () => {},
    onSync: () => {},
  }),
);
assert(
  githubAppDraftHtml.includes("data-testid=\"openshell-provider-cred-GITHUB_APP_PRIVATE_KEY\""),
  "github-app form shows private key, not pasted GH_TOKEN",
);
assert(
  !githubAppDraftHtml.includes("data-testid=\"openshell-provider-cred-GH_TOKEN\""),
  "mint-managed GH_TOKEN is not a form credential field",
);
assert(
  githubAppDraftHtml.includes("data-testid=\"github-app-install-link\""),
  "Install / manage on GitHub link on provider form",
);
assert(
  githubAppDraftHtml.includes("data-testid=\"github-app-refresh-installations\""),
  "Refresh installations on provider form",
);
assert(
  githubAppDraftHtml.includes("data-testid=\"openshell-provider-config-GITHUB_APP_ID\""),
  "App ID config field",
);

const openshellProvidersEmptyHtml = renderToString(
  React.createElement(OpenShellProvidersPanelView, {
    providers: [],
    gatewayReachable: false,
    profiles: [],
    draft: null,
    onDraftChange: () => {},
    onSave: () => {},
    onCancelEdit: () => {},
    onEdit: () => {},
    onDelete: () => {},
    onSync: () => {},
  }),
);
assert(openshellProvidersEmptyHtml.includes("data-testid=\"openshell-providers-empty\""), "Empty providers state");
assert(openshellProvidersEmptyHtml.includes("gateway offline"), "Offline gateway badge");

const openshellProvidersAddBlankTypeHtml = renderToString(
  React.createElement(OpenShellProvidersPanelView, {
    providers: [],
    gatewayReachable: true,
    profiles: [
      {
        id: "cursor-agent",
        display_name: "Cursor Agent",
        description: "CURSOR_API_KEY",
        source: "board",
        credential_env_vars: ["CURSOR_API_KEY"],
        form_config_keys: [],
      },
      {
        id: "google-vertex-ai",
        display_name: "Google Vertex AI",
        description: "vertex",
        source: "gateway",
        credential_env_vars: [],
        form_config_keys: ["VERTEX_AI_PROJECT_ID"],
      },
    ],
    draft: { name: "", type: "", config: {}, credentials: {} },
    onDraftChange: () => {},
    onSave: () => {},
    onCancelEdit: () => {},
    onEdit: () => {},
    onDelete: () => {},
    onSync: () => {},
  }),
);
assert(
  openshellProvidersAddBlankTypeHtml.includes("Select a provider type…"),
  "Add provider starts with no type selected",
);
assert(
  openshellProvidersAddBlankTypeHtml.includes('value=""') &&
    openshellProvidersAddBlankTypeHtml.includes("disabled=\"\""),
  "Type placeholder option is disabled empty value",
);

const openshellCursorAgentHtml = renderToString(
  React.createElement(OpenShellProvidersPanelView, {
    providers: [],
    gatewayReachable: true,
    profiles: [
      {
        id: "cursor-agent",
        display_name: "Cursor Agent",
        description: "CURSOR_API_KEY",
        source: "board",
        credential_env_vars: ["CURSOR_API_KEY"],
        form_config_keys: [],
      },
    ],
    draft: {
      name: "cursor-cli",
      type: "cursor-agent",
      config: {},
      credentials: {},
    },
    onDraftChange: () => {},
    onSave: () => {},
    onCancelEdit: () => {},
    onEdit: () => {},
    onDelete: () => {},
    onSync: () => {},
  }),
);
assert(
  openshellCursorAgentHtml.includes("data-testid=\"openshell-provider-cred-CURSOR_API_KEY\""),
  "cursor-agent type renders CURSOR_API_KEY credential field",
);

const openshellProviderTypesHtml = renderToString(
  React.createElement(OpenShellProviderTypesPanelView, {
    types: [
      {
        id: "cursor-agent",
        display_name: "Cursor Agent",
        description: "",
        source: "board",
        credential_env_vars: ["CURSOR_API_KEY"],
        form_config_keys: [],
        yaml: "id: cursor-agent\n",
        shipped: true,
      },
    ],
    draft: null,
    editingId: null,
    onDraftChange: () => {},
    onSave: () => {},
    onCancelEdit: () => {},
    onEdit: () => {},
    onDelete: () => {},
    onAdd: () => {},
  }),
);
assert(
  openshellProviderTypesHtml.includes("data-testid=\"openshell-provider-types\""),
  "Provider types band renders",
);
assert(
  openshellProviderTypesHtml.includes("data-testid=\"openshell-provider-type-cursor-agent\""),
  "Shipped cursor-agent type row renders",
);
assert(
  openshellProviderTypesHtml.includes("shipped"),
  "Shipped badge on provider type row",
);

const openshellProviderTypesGithubHtml = renderToString(
  React.createElement(OpenShellProviderTypesPanelView, {
    types: [
      {
        id: "github-app",
        display_name: "GitHub Application Access Token",
        description: "minted GH_TOKEN",
        source: "board",
        credential_env_vars: ["GH_TOKEN"],
        form_config_keys: ["GITHUB_APP_ID", "GITHUB_INSTALLATION_ID"],
        yaml: "id: github-app\n",
        shipped: true,
      },
      {
        id: "antigravity",
        display_name: "Google Antigravity (agy)",
        description: "",
        source: "board",
        credential_env_vars: ["ANTIGRAVITY_ACCESS_TOKEN"],
        form_config_keys: ["ANTIGRAVITY_GCP_PROJECT", "ANTIGRAVITY_GCP_LOCATION"],
        yaml: "id: antigravity\n",
        shipped: true,
      },
    ],
    draft: null,
    editingId: null,
    onDraftChange: () => {},
    onSave: () => {},
    onCancelEdit: () => {},
    onEdit: () => {},
    onDelete: () => {},
    onAdd: () => {},
  }),
);
assert(
  openshellProviderTypesGithubHtml.includes("data-testid=\"openshell-provider-type-github-app\""),
  "Shipped github-app type lists next to antigravity",
);
assert(
  openshellProviderTypesGithubHtml.includes("data-testid=\"openshell-provider-type-antigravity\""),
  "antigravity type still listed",
);

const openshellWithBandsHtml = renderToString(
  React.createElement(OpenShellPanelView, {
    ...openshellPanelProps,
    activeTab: "providers",
    status: {
      healthy: true,
      summary: "ok",
      not_configured: false,
    },
    providers: React.createElement("div", { "data-testid": "openshell-providers-slot" }, "providers"),
    profiles: React.createElement("div", { "data-testid": "openshell-profiles-slot" }, "profiles"),
  }),
);
assert(openshellWithBandsHtml.includes("data-testid=\"openshell-providers-slot\""), "Providers tab hosts providers slot");
assert(!openshellWithBandsHtml.includes("data-testid=\"openshell-profiles-slot\""), "Profiles slot hidden off-tab");
assert(!openshellWithBandsHtml.includes("data-testid=\"openshell-connectivity\""), "Connectivity pane hidden off-tab");

const openshellProfilesTabHtml = renderToString(
  React.createElement(OpenShellPanelView, {
    ...openshellPanelProps,
    activeTab: "profiles",
    status: { healthy: true, summary: "ok", not_configured: false },
    providers: React.createElement("div", { "data-testid": "openshell-providers-slot" }, "providers"),
    profiles: React.createElement("div", { "data-testid": "openshell-profiles-slot" }, "profiles"),
  }),
);
assert(openshellProfilesTabHtml.includes("data-testid=\"openshell-profiles-slot\""), "Profiles tab hosts profiles slot");

const openshellPoliciesTabHtml = renderToString(
  React.createElement(OpenShellPanelView, {
    ...openshellPanelProps,
    activeTab: "policies",
    status: { healthy: true, summary: "ok", not_configured: false },
    policies: React.createElement("div", { "data-testid": "openshell-policies-slot" }, "policies"),
    profiles: React.createElement("div", { "data-testid": "openshell-profiles-slot" }, "profiles"),
  }),
);
assert(openshellPoliciesTabHtml.includes("data-testid=\"openshell-policies-slot\""), "Policies tab hosts policies slot");
assert(!openshellPoliciesTabHtml.includes("data-testid=\"openshell-profiles-slot\""), "Profiles slot hidden on Policies tab");

const mcpServersSectionHtml = renderToString(
  React.createElement(Settings, { section: "openshell/mcp-servers" }),
);
assert(
  mcpServersSectionHtml.includes("data-testid=\"settings-panel-openshell/mcp-servers\""),
  "MCP servers settings panel host",
);
assert(
  mcpServersSectionHtml.includes("data-testid=\"mcp-servers-panel\""),
  "MCP servers section renders catalog panel",
);

const fixturePolicies = [
  {
    id: "minimal",
    name: "Minimal",
    yaml: "version: 1\n# minimal\n",
  },
  {
    id: "heavy-pol",
    name: "Heavy allow",
    yaml: "version: 1\n# heavy\n",
  },
];

const openshellPoliciesHtml = renderToString(
  React.createElement(OpenShellPoliciesPanelView, {
    policies: fixturePolicies,
    editingId: null,
    draft: null,
    onDraftChange: () => {},
    onSave: () => {},
    onCancelEdit: () => {},
    onEdit: () => {},
    onDelete: () => {},
    onStartCreate: () => {},
  }),
);
assert(openshellPoliciesHtml.includes("data-testid=\"openshell-policies\""), "Policies band renders");
assert(
  openshellPoliciesHtml.includes(">Policies<") || openshellPoliciesHtml.includes("Policies</h3>"),
  "Policies catalog heading",
);
assert(openshellPoliciesHtml.includes("data-testid=\"openshell-policy-minimal\""), "Policy row renders");
assert(openshellPoliciesHtml.includes("data-testid=\"openshell-policies-add\""), "Add policy control");
assert(openshellPoliciesHtml.includes("Sandbox spec"), "Policies copy points attach to Sandbox specs");

const openshellPoliciesEmptyHtml = renderToString(
  React.createElement(OpenShellPoliciesPanelView, {
    policies: [],
    editingId: null,
    draft: null,
    onDraftChange: () => {},
    onSave: () => {},
    onCancelEdit: () => {},
    onEdit: () => {},
    onDelete: () => {},
    onStartCreate: () => {},
  }),
);
assert(openshellPoliciesEmptyHtml.includes("data-testid=\"openshell-policies-empty\""), "Empty policies state");

const openshellPolicyFormHtml = renderToString(
  React.createElement(OpenShellPoliciesPanelView, {
    policies: fixturePolicies,
    editingId: "",
    draft: {
      id: "",
      name: "CI allow",
      yaml: "version: 1\nfilesystem_policy:\n  include_workdir: true\n",
    },
    onDraftChange: () => {},
    onSave: () => {},
    onCancelEdit: () => {},
    onEdit: () => {},
    onDelete: () => {},
    onStartCreate: () => {},
  }),
);
assert(openshellPolicyFormHtml.includes("data-testid=\"openshell-policy-form\""), "Policy create form");
assert(openshellPolicyFormHtml.includes("data-testid=\"openshell-policy-field-name\""), "Policy name field");
assert(openshellPolicyFormHtml.includes("data-testid=\"openshell-policy-field-yaml\""), "Policy YAML field");
assert(openshellPolicyFormHtml.includes("<textarea"), "Policy YAML is a textarea");
assert(openshellPolicyFormHtml.includes("data-testid=\"openshell-policy-save\""), "Policy save control");

const workspaceHtml = renderToString(
  React.createElement(WorkspacePanelView, {
    draft: {
      forge: "github",
    },
    poll: {
      enabled: false,
      interval_secs: 60,
      provider_name: null,
    },
    credentialOptions: [],
    onDraftChange: () => {},
    onPollChange: () => {},
    onSave: () => {},
  }),
);
assert(workspaceHtml.includes("data-testid=\"workspace-panel\""), "Forge panel should render");
assert(workspaceHtml.includes("data-testid=\"workspace-form\""), "Forge form should render");
assert(workspaceHtml.includes("data-testid=\"workspace-field-forge\""), "Provider field");
assert(workspaceHtml.includes("data-testid=\"workspace-poll\""), "Poll fallback controls");
assert(workspaceHtml.includes("data-testid=\"workspace-poll-enabled\""), "Poll enabled checkbox");
assert(workspaceHtml.includes("data-testid=\"workspace-poll-interval\""), "Poll interval field");
assert(workspaceHtml.includes("data-testid=\"workspace-poll-credential\""), "Credential select");
assert(workspaceHtml.includes("data-testid=\"workspace-poll-auth\""), "Poll credential status");
assert(workspaceHtml.includes("no GitHub credentials"), "Empty state names missing credentials");
assert(workspaceHtml.includes("Create a provider"), "Empty state tells what to create");
assert(!workspaceHtml.includes("data-testid=\"workspace-first-clone-defaults\""), "no first-clone defaults");
assert(!workspaceHtml.includes("data-testid=\"workspace-field-upstream\""), "no upstream field");
assert(!workspaceHtml.includes("data-testid=\"workspace-field-fork\""), "no fork field");
assert(!workspaceHtml.includes("data-testid=\"workspace-field-base\""), "no base field");
assert(workspaceHtml.includes("GitLab (future)"), "GitLab listed as future/disabled");
assert(!workspaceHtml.includes("data-testid=\"workspace-webhook-hint\""), "no gh webhook forward hint");
assert(!workspaceHtml.includes("gh webhook forward"), "no gh webhook forward copy");
assert(!workspaceHtml.includes("sandboard-app/sandboard"), "Forge panel must not hardcode Shane repo");
assert(
  workspaceHtml.includes("poll") || workspaceHtml.includes("Polling"),
  "Forge copy mentions polling",
);
assert(workspaceHtml.includes("data-testid=\"workspace-save\""), "Forge save control");

const workspaceAuthReadyHtml = renderToString(
  React.createElement(WorkspacePanelView, {
    draft: { forge: "github" },
    poll: { enabled: true, interval_secs: 30, provider_name: "github-app" },
    credentialOptions: [
      {
        name: "github-app",
        type: "github-app",
        config: {},
        credential_keys: ["GITHUB_APP_PRIVATE_KEY", "GH_TOKEN"],
        has_credentials: true,
        has_refresh: false,
        gateway_synced: true,
      },
      {
        name: "gh-pat",
        type: "github",
        config: {},
        credential_keys: ["GH_TOKEN"],
        has_credentials: true,
        has_refresh: false,
        gateway_synced: true,
      },
    ],
    githubAppConfigured: true,
    onDraftChange: () => {},
    onPollChange: () => {},
    onSave: () => {},
  }),
);
assert(
  workspaceAuthReadyHtml.includes("github-app ready"),
  "Ready state labels selected credential ready",
);
assert(
  workspaceAuthReadyHtml.includes("gh-pat"),
  "PAT-style github provider listed as selectable credential",
);
assert(
  workspaceAuthReadyHtml.includes("/settings/openshell/providers"),
  "Link to OpenShell Providers for App config",
);
assert(
  !workspaceAuthReadyHtml.includes("data-testid=\"workspace-poll-auth-warn\""),
  "No skip warning when selected credential ready",
);

const workspaceAuthMissingHtml = renderToString(
  React.createElement(WorkspacePanelView, {
    draft: { forge: "github" },
    poll: { enabled: true, interval_secs: 30, provider_name: null },
    credentialOptions: [
      {
        name: "github-app",
        type: "github-app",
        config: {},
        credential_keys: ["GITHUB_APP_PRIVATE_KEY"],
        has_credentials: true,
        has_refresh: false,
      },
    ],
    githubAppConfigured: false,
    onDraftChange: () => {},
    onPollChange: () => {},
    onSave: () => {},
  }),
);
assert(
  workspaceAuthMissingHtml.includes("credential not selected"),
  "No auto-select when provider_name unset",
);
assert(
  workspaceAuthMissingHtml.includes("data-testid=\"workspace-poll-auth-warn\""),
  "Warn when poll enabled without selected credential",
);
assert(
  workspaceAuthMissingHtml.includes("will skip"),
  "Warn copy says poll will skip",
);
assert(
  workspaceAuthMissingHtml.includes("supplies the GitHub token for polling"),
  "Copy explains choosing a poll credential",
);
const fixtureProfiles = [
  {
    id: "default",
    name: "Default",
    image: "img:1",
    policy_id: "minimal",
    cpu: "2",
    memory: "4Gi",
    engine: "cursor",
    model: null,
  },
  {
    id: "heavy",
    name: "Heavy",
    image: "img:heavy",
    policy_id: "heavy-pol",
    cpu: "8",
    memory: "16Gi",
    engine: "agy",
    model: "gemini-3.6-flash-high",
    env: { TOOL_PATH: "/opt/tools" },
    prompt: "Prefer agy for this seat.",
  },
];

const sandboxPanelBase = {
  profiles: fixtureProfiles,
  policies: fixturePolicies,
  defaultId: "default",
  cockpitId: "default",
  availableProviders: [
    {
      name: "vertex",
      type: "google-vertex-ai",
      config: {},
      credential_keys: [],
      has_credentials: true,
      has_refresh: false,
      gateway_synced: true,
    },
  ],
  availableMcpServers: [
    {
      id: "sandboard",
      name: "sandboard",
      transport: { kind: "http", url: "", auth: { kind: "cockpit_bearer" } },
      audience: "cockpit",
      shipped: true,
    },
  ],
  selectedId: "default",
  editingId: null,
  draft: {
    id: "",
    name: "",
    image: "",
    policy_id: "minimal",
    cpu: "",
    memory: "",
    engine: "cursor",
    model: "",
    provider_names: [],
    mcp_server_ids: [],
    env: {},
    prompt: "",
  },
  onSelect: () => {},
  onDraftChange: () => {},
  onStartCreate: () => {},
  onStartEdit: () => {},
  onCancelEdit: () => {},
  onSave: () => {},
  onDelete: () => {},
  onSetDefault: () => {},
  onSetCockpit: () => {},
};

const sandboxesHtml = renderToString(
  React.createElement(SandboxesPanelView, sandboxPanelBase),
);
assert(sandboxesHtml.includes("data-testid=\"openshell-profiles\""), "Sandbox specs band wrapper");
assert(sandboxesHtml.includes("data-testid=\"sandboxes-panel\""), "Sandbox specs panel should render");
assert(
  sandboxesHtml.includes(">Sandbox specs<") || sandboxesHtml.includes("Sandbox specs</h3>"),
  "Sandbox specs heading",
);
assert(sandboxesHtml.includes("data-testid=\"sandbox-profile-list\""), "Sandbox specs panel should list specs");
assert(sandboxesHtml.includes("data-testid=\"sandbox-profile-default\""), "Should list default profile");
assert(sandboxesHtml.includes("data-testid=\"sandbox-profile-heavy\""), "Should list heavy profile");
assert(sandboxesHtml.includes("data-testid=\"sandbox-default-badge\""), "Default profile should be badged");
assert(sandboxesHtml.includes("data-testid=\"sandbox-cockpit-badge\""), "Cockpit profile should be badged");
assert(sandboxesHtml.includes("data-testid=\"sandbox-create\""), "Sandbox specs panel should support create");
assert(sandboxesHtml.includes("data-testid=\"sandbox-edit-default\""), "Selected profile offers Edit");
assert(sandboxesHtml.includes("cursor"), "Selected profile shows engine");
assert(sandboxesHtml.includes("data-testid=\"sandbox-delete-default\""), "Default profile can be deleted");
assert(sandboxesHtml.includes("data-testid=\"sandbox-policy-summary\""), "Selected profile shows policy summary");
assert(sandboxesHtml.includes("data-testid=\"sandbox-policy-name\""), "Selected profile shows policy name");
assert(sandboxesHtml.includes(">Minimal<") || sandboxesHtml.includes("Minimal"), "Policy name resolved from catalog");
assert(sandboxesHtml.includes("data-testid=\"sandbox-no-providers-warn\""),
  "Selected profile with no providers shows warning");
assert(sandboxesHtml.includes("data-testid=\"sandbox-no-providers-badge\""),
  "Rail badges specs that attach no providers");
assert.strictEqual(sandboxHasNoProviders({ provider_names: [] }), true, "sandboxHasNoProviders empty");
assert.strictEqual(sandboxHasNoProviders({ provider_names: ["vertex"] }), false, "sandboxHasNoProviders with attach");
assert(!sandboxesHtml.includes("data-testid=\"sandbox-destroy\""),
  "Sandbox specs panel must not offer live OpenShell sandbox destroy");
assert(!/destroy sandbox|delete environment/i.test(sandboxesHtml),
  "Sandbox specs panel must not offer live OpenShell sandbox destroy controls");

const sandboxesHeavyHtml = renderToString(
  React.createElement(SandboxesPanelView, {
    ...sandboxPanelBase,
    cockpitId: "default",
    selectedId: "heavy",
  }),
);
assert(sandboxesHeavyHtml.includes("data-testid=\"sandbox-set-default-heavy\""), "Non-default offers Set default");
assert(sandboxesHeavyHtml.includes("data-testid=\"sandbox-set-cockpit-heavy\""),
  "Non-Cockpit offers Use for Cockpit");
assert(sandboxesHeavyHtml.includes("data-testid=\"sandbox-delete-heavy\""), "Deletable profile offers Delete");
assert(sandboxesHeavyHtml.includes("gemini-3.6-flash-high"), "Selected profile with model shows it in readonly meta");
assert(sandboxesHeavyHtml.includes("data-testid=\"sandbox-env-summary\""), "Readonly view shows env summary");
assert(sandboxesHeavyHtml.includes("TOOL_PATH=/opt/tools"), "Readonly env summary lists key=value");
assert(sandboxesHeavyHtml.includes("data-testid=\"sandbox-prompt-summary\""), "Readonly view shows prompt summary");
assert(sandboxesHeavyHtml.includes("Prefer agy for this seat."), "Readonly prompt shows seat notes");

const createFormHtml = renderToString(
  React.createElement(SandboxesPanelView, {
    ...sandboxPanelBase,
    cockpitId: "cockpit",
    editingId: "",
    draft: {
      id: "",
      name: "CI",
      image: "img:ci",
      policy_id: "minimal",
      cpu: "",
      memory: "",
      engine: "cursor",
      model: "claude-sonnet-4",
      provider_names: ["vertex"],
      mcp_server_ids: [],
      env: { API_URL: "https://api.example.test" },
      prompt: "Use the staging API.",
    },
  }),
);
assert(createFormHtml.includes("data-testid=\"sandbox-profile-form\""), "Create/edit form should render");
assert(createFormHtml.includes("data-testid=\"sandbox-field-mcp-servers\""), "Create form attaches MCP servers");
assert(!createFormHtml.includes("data-testid=\"sandbox-field-id\""),
  "Create form must not require an Id field (server slugs from name)");
assert(createFormHtml.includes("data-testid=\"sandbox-field-name\""), "Create form should include name");
assert(createFormHtml.includes("data-testid=\"sandbox-field-engine\""), "Form should include engine field");
assert(createFormHtml.includes("data-testid=\"sandbox-field-model\""), "Form should include model field");
assert(createFormHtml.includes("claude-sonnet-4"), "Form should show model draft value");
assert(createFormHtml.includes("data-testid=\"sandbox-field-policy\""), "Form should include policy select");
assert(createFormHtml.includes("<select"), "Policy control should be a select by id");
assert(createFormHtml.includes("value=\"minimal\"") || createFormHtml.includes(">Minimal"),
  "Form lists catalog policies");
assert(createFormHtml.includes("data-testid=\"sandbox-field-providers\""), "Form includes per-profile providers");
assert(createFormHtml.includes("data-testid=\"sandbox-provider-vertex\""), "Form lists available providers");
assert(createFormHtml.includes("data-testid=\"sandbox-field-env\""), "Form includes env editor");
assert(createFormHtml.includes("data-testid=\"sandbox-env-non-secret-hint\""),
  "Form shows non-secret env hint");
assert(createFormHtml.includes("secrets on Providers"), "Env hint mentions Providers for secrets");
assert(createFormHtml.includes("data-testid=\"sandbox-env-key-API_URL\""), "Form shows env key");
assert(createFormHtml.includes("data-testid=\"sandbox-env-value-API_URL\""), "Form shows env value");
assert(createFormHtml.includes("https://api.example.test"), "Form shows env draft value");
assert(createFormHtml.includes("data-testid=\"sandbox-env-add\""), "Form offers add env variable");
assert(createFormHtml.includes("data-testid=\"sandbox-field-prompt\""), "Form includes prompt field");
assert(createFormHtml.includes("data-testid=\"sandbox-field-prompt-input\""), "Form includes prompt textarea");
assert(createFormHtml.includes("Use the staging API."), "Form shows prompt draft value");
assert(!createFormHtml.includes("data-testid=\"sandbox-no-providers-warn\""),
  "Create form with providers selected must not warn");
assert(!/policy path|path to.*policy|host path/i.test(createFormHtml),
  "Settings must not ask for a host filesystem policy path");
assert(createFormHtml.includes("data-testid=\"sandbox-save\""), "Form should include save");

const emptyProvidersFormHtml = renderToString(
  React.createElement(SandboxesPanelView, {
    ...sandboxPanelBase,
    cockpitId: "cockpit",
    editingId: "",
    draft: {
      id: "",
      name: "Bare",
      image: "img:bare",
      policy_id: "minimal",
      cpu: "",
      memory: "",
      engine: "cursor",
      model: "",
      provider_names: [],
      mcp_server_ids: [],
      env: {},
      prompt: "",
    },
  }),
);
assert(emptyProvidersFormHtml.includes("data-testid=\"sandbox-no-providers-warn\""),
  "Create form with no providers selected shows warning");

const editFormHtml = renderToString(
  React.createElement(SandboxesPanelView, {
    ...sandboxPanelBase,
    cockpitId: null,
    editingId: "default",
    draft: {
      id: "default",
      name: "Default",
      image: "img",
      policy_id: "minimal",
      cpu: "",
      memory: "",
      engine: "cursor",
      model: "gpt-5",
      provider_names: ["vertex"],
      mcp_server_ids: ["sandboard"],
      env: {},
      prompt: "",
    },
  }),
);
assert(editFormHtml.includes("data-testid=\"sandbox-field-id\""),
  "Edit form may show id read-only");
assert(editFormHtml.includes("disabled") || editFormHtml.includes("readonly"),
  "Edit id field should be non-editable");
// Cockpit-effective spec: shipped sandboard is locked on (cannot uncheck).
assert(
  /data-testid="sandbox-mcp-sandboard"[^>]*disabled/.test(editFormHtml) ||
    /disabled[^>]*data-testid="sandbox-mcp-sandboard"/.test(editFormHtml),
  "Cockpit spec must lock shipped sandboard MCP checkbox",
);
assert(editFormHtml.includes("(required)"), "Locked sandboard shows required label");

const nonCockpitMcpHtml = renderToString(
  React.createElement(SandboxesPanelView, {
    ...sandboxPanelBase,
    cockpitId: "default",
    editingId: "heavy",
    draft: {
      id: "heavy",
      name: "Heavy",
      image: "img",
      policy_id: "minimal",
      cpu: "",
      memory: "",
      engine: "cursor",
      model: "",
      provider_names: [],
      mcp_server_ids: [],
      env: {},
      prompt: "",
    },
  }),
);
assert(
  !/data-testid="sandbox-mcp-sandboard"[^>]*disabled/.test(nonCockpitMcpHtml) &&
    !/disabled[^>]*data-testid="sandbox-mcp-sandboard"/.test(nonCockpitMcpHtml),
  "Non-Cockpit spec may toggle sandboard MCP",
);

const pickerHtml = renderToString(
  React.createElement(ProjectSandboxPicker, {
    projectId: 42,
    value: null,
    profiles: fixtureProfiles,
    defaultId: "default",
    onChange: () => {},
  }),
);
assert(pickerHtml.includes("data-testid=\"project-sandbox-picker\""), "Project sandbox picker should render");
assert(pickerHtml.includes("data-testid=\"project-sandbox-select-42\""), "Project sandbox select should render");
assert(pickerHtml.includes("Use global default"), "Unset option should read 'Use global default'");
assert(!pickerHtml.includes("Global default ("), "Unset option must not duplicate name as 'Global default (…)'");
assert(pickerHtml.includes("Default · global default"), "Global default profile marked once by name");
assert(pickerHtml.includes("Heavy"), "Named profiles list by display name");
assert(!pickerHtml.includes("Default (default)"), "Must not show 'Default (default)' duplication");
assert(!pickerHtml.includes("Heavy (heavy)"), "Must not show raw id in every option");
assert(pickerHtml.includes("data-testid=\"project-sandbox-no-providers-warn\""),
  "Project picker warns when effective sandbox has no providers");
assert(pickerHtml.includes("no providers"), "Options mark specs with no providers");

// Board view still mounts Board (regression: chrome must not replace it).
const emptyBoardHtml = renderToString(
  React.createElement(Board, {
    goals: [],
    items: new Map(),
    stories: new Map(),
    goalOf: (id) => id,
    breadcrumbOf: () => "",
    now: Date.now(),
    agentTimeout: 300,
    onOpen: () => {},
  }),
);
assert(emptyBoardHtml.includes("board-page") || emptyBoardHtml.includes("Welcome to sandboard"),
  "Board view should still render Board");
assert(emptyBoardHtml.includes("Welcome to sandboard"), "Board empty keeps Welcome hero");
assert(emptyBoardHtml.includes("data-testid=\"board-empty\""), "Board empty shell testid");
assert(emptyBoardHtml.includes("Create a Project, approve its plan"),
  "Welcome lede stays consistent with on-board create");
assert(emptyBoardHtml.includes('data-testid="create-project"'),
  "Empty board exposes Create Project root");
assert(emptyBoardHtml.includes('data-testid="create-project-open"'),
  "Empty board shows Create Project open control (form collapsed)");
assert(!emptyBoardHtml.includes('data-testid="create-project-form"'),
  "Empty board keeps Create Project form collapsed until opened");
assert(emptyBoardHtml.includes("Create Project"),
  "Empty board Create Project affordance copy");
assert(!emptyBoardHtml.includes('data-testid="create-task"'),
  "Empty Welcome board must not expose Create Task");
assert(!emptyBoardHtml.includes('data-testid="create-task-form"'),
  "Empty Welcome board must not show Create Task form");
assert(emptyBoardHtml.includes("data-testid=\"operator-guide\""), "Board empty embeds OperatorGuide");
assert(emptyBoardHtml.includes("data-testid=\"operator-guide-quickstart\""), "Board empty shows Quickstart section");
assert(emptyBoardHtml.includes("data-testid=\"operator-guide-config\""), "Board empty shows Configuration section");
assert(emptyBoardHtml.includes("data-testid=\"operator-guide-mcp\""), "Board empty shows MCP section");
assert(emptyBoardHtml.includes("data-testid=\"operator-guide-openshell\""), "Board empty shows OpenShell section");
assert(emptyBoardHtml.includes("clone_repo"), "Board empty documents clone_repo");
assert(emptyBoardHtml.includes("project_prompt"), "Board empty documents project_prompt");
assert(emptyBoardHtml.includes("on the board"),
  "Board empty OperatorGuide stays consistent with on-board create");
assert(emptyBoardHtml.includes("plan.json"), "Board empty documents plan.json");
assert(emptyBoardHtml.includes("Approve"), "Board empty documents Approve");
assert(emptyBoardHtml.includes("/mcp"), "Board empty shows MCP URL");
assert(!emptyBoardHtml.includes("127.0.0.1:8080"), "Board empty must not hardcode loopback:8080");
assert(emptyBoardHtml.includes("Streamable HTTP"), "Board empty names Streamable HTTP transport");
assert(emptyBoardHtml.includes("Configuration layers"), "Board Welcome lede mentions configuration layers");
assert(emptyBoardHtml.includes("OpenShell + sandbox"), "Board empty includes OpenShell setup section");
assert(emptyBoardHtml.includes("/settings/openshell/connectivity"), "Board empty deep-links Connectivity");
assert(emptyBoardHtml.includes("/settings/agent-runtime"), "Board empty deep-links Agent runtime");
assert(emptyBoardHtml.includes("data-testid=\"openshell-readiness\""), "Board empty shows OpenShell readiness strip");
assert(emptyBoardHtml.includes("data-testid=\"openshell-readiness-gateway\""), "Board empty readiness: gateway row");
assert(emptyBoardHtml.includes("data-testid=\"openshell-readiness-sandbox\""), "Board empty readiness: sandbox row");
assert(!emptyBoardHtml.includes("data-testid=\"openshell-readiness-agents\""), "Board empty readiness: no agents-enabled row");
// Board empty shares OperatorGuide order: Quickstart → Configuration → MCP.
{
  const boardQuickstartIdx = emptyBoardHtml.indexOf("data-testid=\"operator-guide-quickstart\"");
  const boardConfigIdx = emptyBoardHtml.indexOf("data-testid=\"operator-guide-config\"");
  const boardMcpIdx = emptyBoardHtml.indexOf("data-testid=\"operator-guide-mcp\"");
  assert(
    boardQuickstartIdx >= 0 && boardConfigIdx > boardQuickstartIdx,
    "Board empty orders Quickstart before Configuration",
  );
  assert(
    boardConfigIdx >= 0 && boardMcpIdx > boardConfigIdx,
    "Board empty orders Configuration before MCP",
  );
}

// Create Project form — required clone_repo (owner/name); presentational field contract.
const createProjectFormHtml = renderToString(
  React.createElement(CreateProjectForm, {
    initiallyOpen: true,
    onCreated: () => {},
  }),
);
assert(createProjectFormHtml.includes('data-testid="create-project-form"'),
  "Create Project form root testid");
assert(createProjectFormHtml.includes('data-testid="create-project-title"'),
  "Create Project form title field");
assert(createProjectFormHtml.includes('data-testid="create-project-intent"'),
  "Create Project form intent field");
assert(createProjectFormHtml.includes('data-testid="create-project-clone-repo"'),
  "Create Project form clone_repo field");
assert(createProjectFormHtml.includes('data-testid="create-project-prompt"'),
  "Create Project form optional project_prompt field");
assert(createProjectFormHtml.includes('data-testid="create-project-submit"'),
  "Create Project form submit control");
assert(
  /clone_repo[\s\S]*owner\/name/.test(createProjectFormHtml),
  "Create Project form labels clone_repo as owner/name",
);
{
  const cloneIdx = createProjectFormHtml.indexOf('data-testid="create-project-clone-repo"');
  assert(cloneIdx >= 0, "clone_repo input testid present");
  // required may be on the same tag before or after the testid attribute.
  const tagStart = createProjectFormHtml.lastIndexOf("<input", cloneIdx);
  const tagEnd = createProjectFormHtml.indexOf(">", cloneIdx);
  const cloneInputTag = createProjectFormHtml.slice(tagStart, tagEnd + 1);
  assert(
    /\srequired(?:[\s>=]|=\"\")/.test(cloneInputTag) || cloneInputTag.includes("required"),
    "Create Project form requires clone_repo (fails if required drops)",
  );
  assert(
    cloneInputTag.includes('placeholder="owner/name"'),
    "Create Project clone_repo placeholder is owner/name",
  );
}

// Create Task form — matches MCP create_task (no clone_repo field); optional blockers.
assert.strictEqual(
  cloneRepoFromProse(
    "Clone repository: sandboard-app/sandboard into /sandbox/repo for planning.",
  ),
  "sandboard-app/sandboard",
  "cloneRepoFromProse reads stamped Project line",
);
assert.strictEqual(cloneRepoFromProse("no stamp"), null, "cloneRepoFromProse misses bare prose");
assert.strictEqual(
  proseHasCloneRepo("Clone repository: acme/widgets. Ship it.", "done"),
  true,
  "proseHasCloneRepo sees intent stamp",
);
assert.strictEqual(
  proseHasCloneRepo("why", "Clone repository: acme/widgets."),
  true,
  "proseHasCloneRepo sees DoD stamp",
);
assert.strictEqual(
  stampCloneIntoIntent("Ship it.", "sandboard-app/sandboard"),
  "Clone repository: sandboard-app/sandboard. Ship it.",
  "stampCloneIntoIntent prefixes clone line",
);

const createTaskFormHtml = renderToString(
  React.createElement(CreateTaskForm, {
    parentId: 100,
    projectIntent:
      "Clone repository: sandboard-app/sandboard into /sandbox/repo for planning.",
    siblings: [
      { id: 9, title: "Sibling A" },
      { id: 10, title: "Sibling B" },
    ],
    collapsible: false,
    onCreated: () => {},
  }),
);
assert(createTaskFormHtml.includes('data-testid="create-task-form"'),
  "Create Task form root testid");
assert(createTaskFormHtml.includes('data-testid="create-task-title"'),
  "Create Task form title field");
assert(createTaskFormHtml.includes('data-testid="create-task-intent"'),
  "Create Task form intent field");
assert(createTaskFormHtml.includes('data-testid="create-task-dod"'),
  "Create Task form definition of done field");
assert(!createTaskFormHtml.includes('data-testid="create-task-clone-repo"'),
  "Create Task form has no clone_repo field (Project/MCP contract)");
assert(createTaskFormHtml.includes('data-testid="create-task-clone-hint"'),
  "Create Task form surfaces clone hint in lede");
assert(createTaskFormHtml.includes('data-testid="create-task-submit"'),
  "Create Task form submit control");
assert(createTaskFormHtml.includes('data-testid="create-task-blockers"'),
  "Create Task form exposes optional blockers when siblings exist");
assert(createTaskFormHtml.includes('data-testid="create-task-blocker-add"'),
  "Create Task form can add a sibling blocker");
assert(createTaskFormHtml.includes("sandboard-app/sandboard"),
  "Create Task form surfaces Project default clone in hint");
assert(createTaskFormHtml.includes("create_task"),
  "Create Task form names MCP create_task");

const createTaskNoDefaultHtml = renderToString(
  React.createElement(CreateTaskForm, {
    parentId: 100,
    projectIntent: "A Project with no clone stamp",
    collapsible: false,
    onCreated: () => {},
  }),
);
assert(
  !createTaskNoDefaultHtml.includes('data-testid="create-task-clone-repo"'),
  "Create Task still has no clone_repo field without Project default",
);
assert(
  createTaskNoDefaultHtml.includes("Clone repository: owner/name"),
  "Create Task without Project default tells you to name clone in Why/DoD",
);

// OpenShell readiness strip — presentational ready / not-ready fixtures
assert.strictEqual(
  gatewayReady({
    healthy: true,
    summary: "Connected",
    not_configured: false,
    auth_mode: "mtls",
    mtls: { ca: true, client_cert: true, client_key: true, complete: true },
  }),
  true,
  "gatewayReady when healthy + complete mTLS",
);
assert.strictEqual(
  gatewayReady({
    healthy: true,
    summary: "Healthy (gateway 0.0.101)",
    not_configured: false,
    auth_mode: "oidc",
    mtls: { ca: false, client_cert: false, client_key: false, complete: false },
    oidc_status: { logged_in: true },
  }),
  true,
  "gatewayReady when healthy + OIDC logged in (no mTLS)",
);
assert.strictEqual(
  gatewayReady({
    healthy: true,
    summary: "Healthy (gateway 0.0.101)",
    not_configured: false,
    auth_mode: "oidc",
    mtls: { ca: false, client_cert: false, client_key: false, complete: false },
    oidc_status: { logged_in: false },
  }),
  false,
  "gatewayReady fails closed when OIDC not logged in",
);
assert.strictEqual(
  gatewayReady({
    healthy: true,
    summary: "Connected",
    not_configured: false,
    auth_mode: "mtls",
    mtls: { ca: true, client_cert: false, client_key: false, complete: false },
  }),
  false,
  "gatewayReady fails closed on incomplete mTLS",
);
assert.strictEqual(
  gatewayReady({
    healthy: false,
    summary: "unreachable",
    not_configured: false,
    auth_mode: "mtls",
    mtls: { ca: true, client_cert: true, client_key: true, complete: true },
  }),
  false,
  "gatewayReady fails closed when unhealthy",
);
assert.strictEqual(
  gatewayReady({
    healthy: false,
    summary: "not configured",
    not_configured: true,
    mtls: { ca: false, client_cert: false, client_key: false, complete: false },
  }),
  false,
  "gatewayReady fails closed when not_configured",
);
assert.strictEqual(gatewayReady(null), false, "gatewayReady fails closed on null");
assert.strictEqual(
  gatewayMtlsReady({
    healthy: true,
    summary: "Connected",
    not_configured: false,
    auth_mode: "mtls",
    mtls: { ca: true, client_cert: true, client_key: true, complete: true },
  }),
  true,
  "gatewayMtlsReady alias still works",
);
assert.strictEqual(
  sandboxSpecReady({
    profiles: [{ id: "default", name: "Default", image: "sandboard-sandbox:latest", policy_id: "minimal" }],
    default_sandbox_profile_id: "default",
    cockpit_sandbox_profile_id: null,
  }),
  true,
  "sandboxSpecReady when default profile set",
);
assert.strictEqual(
  sandboxSpecReady({
    profiles: [],
    default_sandbox_profile_id: null,
    cockpit_sandbox_profile_id: null,
  }),
  false,
  "sandboxSpecReady fails closed without default",
);
assert.strictEqual(sandboxSpecReady(null), false, "sandboxSpecReady fails closed on null");
const readinessReadyHtml = renderToString(
  React.createElement(OpenShellReadinessStripView, {
    gateway: { ready: true, detail: "Connected" },
    sandbox: { ready: true, detail: "Default: Default" },
  }),
);
assert(readinessReadyHtml.includes("data-testid=\"openshell-readiness\""), "Readiness strip root");
assert(readinessReadyHtml.includes("data-ready=\"true\""), "Ready strip marks rows ready");
assert(readinessReadyHtml.includes("data-testid=\"openshell-readiness-gateway-status\""), "Gateway status testid");
assert(readinessReadyHtml.includes(">Ready<"), "Ready strip shows Ready labels");
assert(readinessReadyHtml.includes("href=\"/settings/openshell/connectivity\""), "Ready strip CTA: Connectivity");
assert(readinessReadyHtml.includes("href=\"/settings/openshell/profiles\""), "Ready strip CTA: Sandbox specs");
assert(!readinessReadyHtml.includes("href=\"/settings/agent-runtime\""), "Ready strip no longer gates on agents enabled");
assert(readinessReadyHtml.includes("Settings → Connectivity"), "Ready strip Connectivity CTA copy");
assert(readinessReadyHtml.includes("Settings → Sandbox specs"), "Ready strip Sandbox specs CTA copy");

const readinessNotReadyHtml = renderToString(
  React.createElement(OpenShellReadinessStripView, {
    gateway: { ready: false, detail: "gateway unreachable" },
    sandbox: { ready: false, detail: "No default sandbox profile" },
  }),
);
assert(readinessNotReadyHtml.includes("data-ready=\"false\""), "Not-ready strip marks rows not ready");
assert(readinessNotReadyHtml.includes(">Not ready<"), "Not-ready strip shows Not ready labels");
assert(!readinessNotReadyHtml.includes(">Ready<"), "Not-ready strip has no Ready label");
assert(readinessNotReadyHtml.includes("gateway unreachable"), "Not-ready strip shows gateway detail");
assert(readinessNotReadyHtml.includes("No default sandbox profile"), "Not-ready strip shows sandbox detail");
assert(readinessNotReadyHtml.includes("href=\"/settings/openshell/connectivity\""), "Not-ready CTA: Connectivity");
assert(readinessNotReadyHtml.includes("href=\"/settings/openshell/profiles\""), "Not-ready CTA: Sandbox specs");
assert(!readinessNotReadyHtml.includes("href=\"/settings/agent-runtime\""), "Not-ready strip no agents CTA");

const readinessCheckingHtml = renderToString(
  React.createElement(OpenShellReadinessStripView, {
    gateway: { ready: false, checking: true },
    sandbox: { ready: false, checking: true },
  }),
);
assert(readinessCheckingHtml.includes("data-ready=\"false\""), "Checking state fails closed (not ready)");
assert(readinessCheckingHtml.includes("Checking…"), "Checking state shows Checking label");

// Archived toggle on empty board when only retired projects exist.
const archivedEmptyBoardHtml = renderToString(
  React.createElement(Board, {
    goals: [
      {
        id: 7,
        title: "Old project",
        intent: "done",
        progress: 1,
        leaves_done: 1,
        leaves_total: 1,
        agents_live: 0,
        needs_you: 0,
        plan_status: "approved_v1",
        archived: true,
        columns: [],
        story: [],
      },
    ],
    items: new Map(),
    stories: new Map(),
    goalOf: (id) => id,
    breadcrumbOf: () => "",
    now: Date.now(),
    agentTimeout: 300,
    onOpen: () => {},
  }),
);
assert(archivedEmptyBoardHtml.includes("data-testid=\"board-empty\""),
  "Archived-only board still shows empty shell");
assert(archivedEmptyBoardHtml.includes("data-testid=\"operator-guide\""),
  "Archived-only empty embeds OperatorGuide");
assert(archivedEmptyBoardHtml.includes("data-testid=\"board-empty-show-archived\""),
  "Archived toggle present on empty board");
assert(
  /Show\s*(?:<!-- -->)?1(?:<!-- -->)?\s*archived/.test(archivedEmptyBoardHtml),
  "Archived toggle labels count",
);

// Archived Project lane offers confirm-gated Unarchive (Board source).
const boardSrc = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "src/components/Board.tsx"),
  "utf8",
);
assert(boardSrc.includes('data-testid="lane-unarchive"'),
  "Archived Project lane should expose Unarchive control");
assert(boardSrc.includes("api.unarchive"),
  "Board Unarchive should call api.unarchive");
assert(boardSrc.includes("CreateProjectForm"),
  "Board mounts Create Project form");
assert(boardSrc.includes("<CreateProjectForm collapsible"),
  "Create Project is collapsible (closed until opened)");
assert(!boardSrc.includes("<CreateProjectForm initiallyOpen"),
  "Welcome board does not force Create Project open");
assert(boardSrc.includes("CreateTaskForm"),
  "Board mounts Create Task form on Project lanes");
assert(boardSrc.includes("lane-create-task"),
  "Create Task sits in Project swimlane body");
{
  const emptyIdx = boardSrc.indexOf('data-testid="board-empty"');
  const emptyBlock = boardSrc.slice(
    Math.max(0, emptyIdx - 200),
    emptyIdx + 400,
  );
  assert(
    !emptyBlock.includes("CreateTaskForm"),
    "Create Task must not live in the empty Welcome path",
  );
}

const createProjectFormSrc = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "src/components/CreateProjectForm.tsx"),
  "utf8",
);
assert(createProjectFormSrc.includes('data-testid="create-project-clone-repo"'),
  "Create Project form exposes clone_repo testid");
assert(createProjectFormSrc.includes("clone_repo (<code>owner/name</code>)"),
  "Create Project form labels clone_repo as owner/name");
assert(createProjectFormSrc.includes("api.createProject"),
  "Create Project form uses api.createProject");
assert(createProjectFormSrc.includes("project_prompt"),
  "Create Project form passes optional project_prompt to api.createProject");
assert(
  /create-project-clone-repo[\s\S]*?required|required[\s\S]*?create-project-clone-repo/.test(
    createProjectFormSrc.replace(/\n/g, " "),
  ),
  "Create Project form keeps clone_repo required (source)",
);

const createTaskFormSrc = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "src/components/CreateTaskForm.tsx"),
  "utf8",
);
assert(createTaskFormSrc.includes("api.createTask"),
  "Create Task form uses api.createTask");
assert(createTaskFormSrc.includes("definition_of_done"),
  "Create Task form sends definition_of_done");
assert(createTaskFormSrc.includes("blocked_by"),
  "Create Task form supports optional blocked_by");
assert(createTaskFormSrc.includes("stampCloneIntoIntent"),
  "Create Task stamps Project default clone into intent when omitted");

const apiSrc = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "src/api.ts"),
  "utf8",
);
assert(apiSrc.includes("/items/${id}/unarchive"),
  "api.unarchive should POST /items/{id}/unarchive");
assert(apiSrc.includes("createProject:"),
  "api.ts has typed createProject helper");
assert(apiSrc.includes("clone_repo:"),
  "api.createProject requires clone_repo");
assert(apiSrc.includes("project_prompt"),
  "api.createProject accepts optional project_prompt");
assert(apiSrc.includes("createTask:"),
  "api.ts has typed createTask helper");
assert(apiSrc.includes("definition_of_done: body.definition_of_done"),
  "api.createTask posts definition_of_done");
assert(apiSrc.includes("parent: body.parent"),
  "api.createTask posts parent Project id");

const detailSrc = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "src/components/Detail.tsx"),
  "utf8",
);
assert(detailSrc.includes("CreateTaskForm"),
  "Detail drawer mounts Create Task on Projects");
assert(detailSrc.includes('title="Create Task"') || detailSrc.includes("Create Task"),
  "Detail exposes Create Task section on Projects");

const pkg = JSON.parse(
  readFileSync(join(dirname(fileURLToPath(import.meta.url)), "package.json"), "utf8"),
);
assert(!Object.keys(pkg.dependencies || {}).some((d) => /patternfly/i.test(d)),
  "Must not add a PatternFly dependency");
assert(!Object.keys(pkg.devDependencies || {}).some((d) => /patternfly/i.test(d)),
  "Must not add a PatternFly devDependency");

// Chrome URL location contract (History API — no router dependency)
assert.deepStrictEqual(parseChromeLocation("/"), {
  view: "board",
  cardId: null,
  settingsSection: "openshell",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/help"), {
  view: "help",
  cardId: null,
  settingsSection: "openshell",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/help/"), {
  view: "help",
  cardId: null,
  settingsSection: "openshell",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings"), {
  view: "settings",
  cardId: null,
  settingsSection: "openshell",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/openshell"), {
  view: "settings",
  cardId: null,
  settingsSection: "openshell",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/openshell/providers"), {
  view: "settings",
  cardId: null,
  settingsSection: "openshell/providers",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/openshell/provider-types"), {
  view: "settings",
  cardId: null,
  settingsSection: "openshell/provider-types",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/openshell/policies"), {
  view: "settings",
  cardId: null,
  settingsSection: "openshell/policies",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/openshell/mcp-servers"), {
  view: "settings",
  cardId: null,
  settingsSection: "openshell/mcp-servers",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/openshell/profiles"), {
  view: "settings",
  cardId: null,
  settingsSection: "openshell/profiles",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/openshell/connectivity"), {
  view: "settings",
  cardId: null,
  settingsSection: "openshell",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/github-app"), {
  view: "settings",
  cardId: null,
  settingsSection: "github-app",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/auth"), {
  view: "settings",
  cardId: null,
  settingsSection: "auth",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/workspace"), {
  view: "settings",
  cardId: null,
  settingsSection: "workspace",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/github-app"), {
  view: "settings",
  cardId: null,
  settingsSection: "github-app",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/agent-runtime"), {
  view: "settings",
  cardId: null,
  settingsSection: "agent-runtime",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/settings/nope"), {
  view: "settings",
  cardId: null,
  settingsSection: "openshell",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/card/42"), {
  view: "board",
  cardId: 42,
  settingsSection: "openshell",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/card/0"), {
  view: "board",
  cardId: null,
  settingsSection: "openshell",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/card/nope"), {
  view: "board",
  cardId: null,
  settingsSection: "openshell",
  openShellTab: "connectivity",
});
assert.deepStrictEqual(parseChromeLocation("/unknown"), {
  view: "board",
  cardId: null,
  settingsSection: "openshell",
  openShellTab: "connectivity",
});
assert.strictEqual(
  formatChromePath({
    view: "board",
    cardId: null,
    settingsSection: "openshell",
    openShellTab: "connectivity",
  }),
  "/",
);
assert.strictEqual(
  formatChromePath({
    view: "help",
    cardId: null,
    settingsSection: "openshell",
    openShellTab: "connectivity",
  }),
  "/help",
);
assert.strictEqual(
  formatChromePath({
    view: "settings",
    cardId: 99,
    settingsSection: "openshell",
    openShellTab: "connectivity",
  }),
  "/settings",
);
assert.strictEqual(
  formatChromePath({
    view: "settings",
    cardId: null,
    settingsSection: "openshell",
    openShellTab: "providers",
  }),
  "/settings/openshell/providers",
);
assert.strictEqual(
  formatChromePath({
    view: "settings",
    cardId: null,
    settingsSection: "openshell",
    openShellTab: "provider-types",
  }),
  "/settings/openshell/provider-types",
);
assert.strictEqual(
  formatChromePath({
    view: "settings",
    cardId: null,
    settingsSection: "openshell",
    openShellTab: "policies",
  }),
  "/settings/openshell/policies",
);
assert.strictEqual(
  formatChromePath({
    view: "settings",
    cardId: null,
    settingsSection: "mcp-servers",
    openShellTab: "connectivity",
  }),
  "/settings/mcp-servers",
);
assert.strictEqual(
  formatChromePath({
    view: "settings",
    cardId: null,
    settingsSection: "openshell",
    openShellTab: "profiles",
  }),
  "/settings/openshell/profiles",
);
assert.strictEqual(
  formatChromePath({
    view: "settings",
    cardId: null,
    settingsSection: "openshell",
    openShellTab: "providers",
  }),
  "/settings/openshell/providers",
);
assert.strictEqual(
  formatChromePath({
    view: "settings",
    cardId: null,
    settingsSection: "access",
    openShellTab: "connectivity",
  }),
  "/settings/access",
);
assert.strictEqual(
  formatChromePath({
    view: "settings",
    cardId: null,
    settingsSection: "workspace",
    openShellTab: "connectivity",
  }),
  "/settings/workspace",
);
assert.strictEqual(
  formatChromePath({
    view: "settings",
    cardId: null,
    settingsSection: "agent-runtime",
    openShellTab: "connectivity",
  }),
  "/settings/agent-runtime",
);
assert.strictEqual(
  formatChromePath({
    view: "board",
    cardId: 7,
    settingsSection: "openshell",
    openShellTab: "connectivity",
  }),
  "/card/7",
);
assert(
  chromeLocationsEqual(
    {
      view: "board",
      cardId: 1,
      settingsSection: "openshell",
      openShellTab: "connectivity",
    },
    {
      view: "board",
      cardId: 1,
      settingsSection: "access",
      openShellTab: "providers",
    },
  ),
  "equal chrome locations (board ignores settings axes)",
);
assert(
  chromeLocationsEqual(
    {
      view: "settings",
      cardId: null,
      settingsSection: "openshell",
      openShellTab: "providers",
    },
    {
      view: "settings",
      cardId: null,
      settingsSection: "openshell",
      openShellTab: "providers",
    },
  ),
  "equal settings+openshell tab locations",
);
assert(
  !chromeLocationsEqual(
    {
      view: "settings",
      cardId: null,
      settingsSection: "openshell",
      openShellTab: "providers",
    },
    {
      view: "settings",
      cardId: null,
      settingsSection: "openshell",
      openShellTab: "profiles",
    },
  ),
  "distinct openshell tabs",
);
assert(
  !chromeLocationsEqual(
    {
      view: "settings",
      cardId: null,
      settingsSection: "workspace",
      openShellTab: "connectivity",
    },
    {
      view: "settings",
      cardId: null,
      settingsSection: "access",
      openShellTab: "connectivity",
    },
  ),
  "distinct settings sections",
);
assert(
  !chromeLocationsEqual(
    {
      view: "board",
      cardId: 1,
      settingsSection: "openshell",
      openShellTab: "connectivity",
    },
    {
      view: "help",
      cardId: null,
      settingsSection: "openshell",
      openShellTab: "connectivity",
    },
  ),
  "distinct chrome locations",
);
{
  const pushes = [];
  const replaces = [];
  const hist = {
    pushState: (_s, _t, url) => pushes.push(url),
    replaceState: (_s, _t, url) => replaces.push(url),
  };
  writeChromeLocation(
    {
      view: "help",
      cardId: null,
      settingsSection: "openshell",
      openShellTab: "connectivity",
    },
    "push",
    hist,
    { pathname: "/" },
  );
  writeChromeLocation(
    {
      view: "help",
      cardId: null,
      settingsSection: "openshell",
      openShellTab: "connectivity",
    },
    "push",
    hist,
    { pathname: "/help" },
  );
  writeChromeLocation(
    {
      view: "board",
      cardId: 3,
      settingsSection: "openshell",
      openShellTab: "connectivity",
    },
    "replace",
    hist,
    { pathname: "/help" },
  );
  writeChromeLocation(
    {
      view: "settings",
      cardId: null,
      settingsSection: "openshell",
      openShellTab: "providers",
    },
    "push",
    hist,
    { pathname: "/settings" },
  );
  writeChromeLocation(
    {
      view: "settings",
      cardId: null,
      settingsSection: "agent-runtime",
      openShellTab: "connectivity",
    },
    "push",
    hist,
    { pathname: "/settings/openshell/providers" },
  );
  assert.deepStrictEqual(
    pushes,
    ["/help", "/settings/openshell/providers", "/settings/agent-runtime"],
    "pushState for help + settings section/tab changes",
  );
  assert.deepStrictEqual(replaces, ["/card/3"], "replaceState for card deep link");
}
// Round-trip: settings section paths
for (const path of [
  "/settings",
  "/settings/openshell/providers",
  "/settings/openshell/profiles",
  "/settings/auth",
  "/settings/workspace",
  "/settings/agent-runtime",
]) {
  const parsed = parseChromeLocation(path);
  assert.strictEqual(
    formatChromePath(parsed),
    path === "/settings/openshell/connectivity" ? "/settings" : path,
    `round-trip ${path}`,
  );
}
assert.strictEqual(
  formatChromePath(parseChromeLocation("/settings/github-app")),
  "/settings/github-app",
  "settings section produces matching path",
);
assert.strictEqual(
  formatChromePath(parseChromeLocation("/settings/openshell")),
  "/settings",
  "openshell default tab canonicalizes to /settings",
);
assert.strictEqual(
  formatChromePath(parseChromeLocation("/settings/openshell/connectivity")),
  "/settings",
  "explicit connectivity tab canonicalizes to /settings",
);
{
  // Controlled Settings deep-link: section from URL contract
  const settingsDeepHtml = renderToString(
    React.createElement(Settings, {
      section: "openshell/providers",
    }),
  );
  assert(
    settingsDeepHtml.includes("data-testid=\"settings-panel-openshell/providers\""),
    "Settings deep link opens OpenShell Providers section",
  );
  const forgeDeepHtml = renderToString(
    React.createElement(Settings, { section: "workspace" }),
  );
  assert(
    forgeDeepHtml.includes("data-testid=\"settings-panel-workspace\""),
    "Settings deep link opens Forge (workspace) section",
  );
}
assert(
  !Object.keys(pkg.dependencies || {}).some((d) => /react-router|@tanstack\/react-router|wouter/i.test(d)),
  "Must not add a client router dependency for chrome URL sync",
);

console.log("\n✅ All Card, Board, Detail, Settings chrome, and useBoard sequence guard assertions passed!");

