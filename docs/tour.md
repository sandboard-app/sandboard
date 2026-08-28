# Tour

One card's life, start to finish. Nothing here needs to be installed — read it
first and decide whether the loop is one you want.

The board below is a real sandboard running against a fixture. Every screenshot on
this page is captured from the running UI, so what you see is what ships.

## 1. The board

![The sandboard board](images/desktop-board.png)

Five columns, each asking a different question:

| Column | The question it asks |
|---|---|
| **Backlog** | What could start? |
| **Running** | What is an agent working on right now? |
| **Needs You** | What is stopped, waiting on a human? |
| **Review** | What finished and is waiting for judgement? |
| **Done** | What landed? |

**Needs You** sits above the columns, with answer buttons on the card — so
blocked decisions are obvious without opening a drawer.

In Backlog, cards carry **`⊘ waiting on`** chips naming what blocks them —
`#3 Fail closed when CI is red` cannot start until `#2 Surface PR checks` lands.
Blocked cards sort to the bottom, so the top of the column is what can start
now.

## 2. A Project proposes its own breakdown

You do not write the task list. You create a **Project**, point it at a repo,
and say what you want. sandboard creates one claimable card — the *Initial plan* —
and an agent reads the repo and proposes the breakdown.

That proposal comes back as a card in Review:

![The Initial plan card with its proposed Tasks](images/desktop-drawer-plan.png)

Four proposed Tasks, each with a key, an intent, a definition of done, and its
dependencies. **You can edit any of it before approving.** Approve, and those
four become real cards in Backlog with the dependency edges already wired — the
same `⊘ waiting on` chips you saw in step 1.

This is the cheapest place to fix the plan. A card that passes every later check
can still be building the wrong thing if the breakdown was wrong here.

## 3. An agent picks up a card

Dispatch a Backlog card (**Start** in the UI) and the supervisor claims it,
creates a sandbox, and runs an agent inside it.

Running cards show what you would want mid-flight: which engine, how much of
the run budget is left, and the sandbox name for logs.

The agent has **no network path back to sandboard**. It cannot see the board, cannot
claim its own card, and cannot approve its own review. The supervisor speaks for
it. Liveness is read from the agent’s output stream — not from a keepalive that
could fire while the agent is wedged.

## 4. When it needs a decision, it stops

![A card waiting in Needs You](images/desktop-drawer-needs-you.png)

An agent that hits a genuine ambiguity does not guess and does not spin. It
writes the question with options and stops. The card lands in **Needs You** and
burns nothing until you answer.

Answering is one tap, from the band at the top of the board. The answer reaches
the agent on its next turn.

## 5. Finished work waits in Review

![A finished card in Review with its pull request](images/desktop-drawer-review.png)

The agent pushes a branch and opens a pull request. The card moves to **Review**
carrying the PR link, the diffstat, and whichever gates it ran.

Review is sorted by size and risk, not arrival time — a large change with a
failed gate sorts above a tiny clean one.

**Approving in sandboard shows the PR.** You merge on GitHub. When the merge lands, a
webhook moves the card to Done. Siblings still in Review stay put unless GitHub
reports CONFLICTING — then they bounce to Backlog for reclaim and rebase.
UNKNOWN retries; repeated overlapping conflicts escalate to Needs You.

## 6. Seeing the shape of the work

![The dependency graph view](images/desktop-graph.png)

Columns answer "what is happening". The graph answers "what depends on what" —
useful when a plan has grown past what the chips on individual cards can show.

## And on a phone

![The board on a phone](images/phone-board.png)

On a phone you mostly see decisions that need you. If that list is short, you
can leave the rest of the board alone until you are back at a desk.

## Next

- Run the board yourself: [Quickstart](quickstart.md)
- The vocabulary in one place: [Concepts](concepts.md) · [Glossary](glossary.md)
- First sandboxed run: empty-board **Welcome** / **Help**, then [Your first agent](first-agent.md)
