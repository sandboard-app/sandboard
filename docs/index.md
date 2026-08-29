<div class="sb-landing" data-landing>
<header class="sb-nav" aria-label="Primary navigation">
<a class="sb-brand" href="index.html" aria-label="sandboard home">
<svg class="sb-brand-mark" viewBox="0 0 128 128" aria-hidden="true">
<g fill="currentColor" transform="translate(64 64) rotate(45)">
<path d="M-34 -34 H34 V-8 Q8 -2 -34 6 Z" />
<path d="M-34 14 Q0 6 34 12 L34 16 Q0 10 -34 18 Z" />
<path d="M-34 26 Q8 18 34 24 V34 H-34 Z" />
</g>
</svg>
<span>sandboard</span>
</a>
<nav class="sb-nav-links">
<a href="#loop">The loop</a>
<a href="#runtime">OpenShell</a>
<a href="tour.html">Tour</a>
<a href="architecture.html">Architecture</a>
</nav>
<div class="sb-nav-meta">
<span>open source / self-hosted</span>
<a class="sb-nav-cta" href="quickstart.html">Get started <span aria-hidden="true">↗</span></a>
</div>
</header>

<section class="sb-hero" aria-labelledby="landing-title">
<div class="sb-hero-copy">
<p class="sb-eyebrow sb-reveal"><span class="sb-pulse" aria-hidden="true"></span> an open-source platform for coding agents</p>
<h1 id="landing-title" class="sb-reveal sb-delay-1">Run coding agents<br /><em>in OpenShell sandboxes.</em></h1>
<p class="sb-hero-lede sb-reveal sb-delay-2">Sandboard coordinates coding agents. OpenShell provides the isolated execution environment. You create a Project, dispatch its Tasks, and review the resulting changes or pull request on GitHub.</p>
<div class="sb-hero-actions sb-reveal sb-delay-3">
<a class="sb-button sb-button-primary" href="quickstart.html">Read the quickstart <span aria-hidden="true">↗</span></a>
<a class="sb-button sb-button-quiet" href="tour.html">See how it works</a>
</div>
<div class="sb-install-line sb-reveal sb-delay-3">
<span>run it locally</span>
<code>git clone https://github.com/sandboard-app/sandboard.git</code>
<button class="sb-copy" type="button" data-copy="git clone https://github.com/sandboard-app/sandboard.git" aria-label="Copy clone command">copy</button>
</div>
</div>

<div class="sb-hero-art sb-reveal sb-delay-2" aria-label="A sandboard workflow moving from backlog to done">
<div class="sb-board-window">
<div class="sb-window-bar">
<span class="sb-window-controls" aria-hidden="true"><i></i><i></i><i></i></span>
<span class="sb-window-title">sandboard / operator view</span>
<span>WORKFLOW</span>
</div>
<div class="sb-window-meta"><strong>SANDBOXED AGENT WORK</strong><span>OpenShell · policy · provider</span></div>
<div class="sb-board-grid">
<div class="sb-lane">
<div class="sb-lane-head"><span>Backlog</span><span class="sb-lane-count">2</span></div>
<div class="sb-mini-card"><span class="sb-card-key">PLAN-01</span><span class="sb-card-title">Shape the next change</span></div>
<div class="sb-mini-card"><span class="sb-card-key">TASK-04</span><span class="sb-card-title">Waiting on its dependency</span></div>
</div>
<div class="sb-lane">
<div class="sb-lane-head"><span>Running</span><span class="sb-lane-count">1</span></div>
<div class="sb-mini-card is-live"><span class="sb-card-key">TASK-03</span><span class="sb-card-title">Agent is working</span></div>
</div>
<div class="sb-lane">
<div class="sb-lane-head"><span>Needs You</span><span class="sb-lane-count">1</span></div>
<div class="sb-mini-card is-waiting"><span class="sb-card-key">TASK-02</span><span class="sb-card-title">A decision is waiting</span></div>
</div>
<div class="sb-lane">
<div class="sb-lane-head"><span>Review</span><span class="sb-lane-count">1</span></div>
<div class="sb-mini-card"><span class="sb-card-key">TASK-01</span><span class="sb-card-title">Pull request is ready</span></div>
</div>
<div class="sb-lane">
<div class="sb-lane-head"><span>Done</span><span class="sb-lane-count">12</span></div>
<div class="sb-mini-card"><span class="sb-card-key">MERGED</span><span class="sb-card-title">Merged on GitHub</span></div>
</div>
</div>
<div class="sb-art-footer"><span>board / sandbox / github</span><strong>review on github</strong></div>
</div>
</div>
</section>

<div class="sb-scroll-cue" aria-hidden="true">scroll to see the handoff</div>

<section class="sb-manifesto" id="runtime" aria-labelledby="manifesto-title">
<div class="sb-reveal">
<p class="sb-label">what sandboard is</p>
<h2 id="manifesto-title">The board for<br /><em>coding agents.</em></h2>
</div>
<div class="sb-manifesto-copy sb-reveal sb-delay-1">
<p>Sandboard is the part you operate: the control plane for repository work. It stores Projects and Tasks, provides the operator UI and MCP endpoint, and moves each Task from backlog to review.</p>
<p>OpenShell is the isolated execution runtime. Sandboard uses its gateway to create the sandbox, select the image, apply the network policy, inject provider credentials at runtime, and start the agent.</p>
<div class="sb-manifesto-note"><span>separate responsibilities</span><span>Sandboard controls lifecycle; OpenShell controls execution</span></div>
</div>
</section>

<section class="sb-loop" id="loop" data-story-root data-stage="shape" aria-labelledby="loop-title">
<div class="sb-loop-intro">
<div class="sb-reveal">
<p class="sb-label">how a Task moves</p>
<h2 id="loop-title">Sandboard starts the run.<br /><em>OpenShell isolates it.</em></h2>
</div>
<p class="sb-loop-intro-copy sb-reveal sb-delay-1">The board stores state and operator actions. Sandboard uses the OpenShell gateway to create the isolated environment, watch the run, and collect the agent’s result.</p>
</div>

<div class="sb-story">
<div class="sb-stage-column">
<div class="sb-story-stage" aria-label="Animated work card moving through the sandboard columns">
<div class="sb-stage-header"><span><strong>sandboard</strong> / live trace</span><span>card 01</span></div>
<div class="sb-stage-board" aria-hidden="true">
<div class="sb-stage-lane"></div><div class="sb-stage-lane"></div><div class="sb-stage-lane"></div><div class="sb-stage-lane"></div><div class="sb-stage-lane"></div>
<div class="sb-stage-path"></div>
<div class="sb-stage-card"><span class="sb-stage-card-key">task / in OpenShell</span><span class="sb-stage-card-title">Implement the requested change</span></div>
</div>
<div class="sb-stage-caption">
<span data-for="shape"><strong>01 / create</strong><em>Create a Project and Task.</em></span>
<span data-for="dispatch"><strong>02 / provision</strong><em>OpenShell creates the sandbox.</em></span>
<span data-for="decide"><strong>03 / observe</strong><em>Sandboard tracks the run.</em></span>
<span data-for="merge"><strong>04 / review</strong><em>Review the pull request.</em></span>
</div>
<div class="sb-stage-footer"><span>sandboard tracks output</span><strong>●</strong></div>
</div>
</div>

<div class="sb-story-steps">
<article class="sb-story-step sb-reveal is-active" data-stage="shape">
<span class="sb-story-step-number">01 / create</span>
<h3>Create a Project.<br /><em>Describe the Task.</em></h3>
<p>Create a Project, point it at a repository, and explain what you want. Sandboard creates an Initial plan that you can edit before it becomes implementation work.</p>
<a href="concepts.html">Understand Projects and Tasks</a>
</article>
<article class="sb-story-step sb-reveal" data-stage="dispatch">
<span class="sb-story-step-number">02 / provision</span>
<h3>OpenShell<br /><em>creates the sandbox.</em></h3>
<p>When you dispatch a Task, Sandboard asks the OpenShell gateway to provision a sandbox with the selected image and network policy, make the configured provider available, and start the agent.</p>
<a href="sandbox.html">See how sandboxes work</a>
</article>
<article class="sb-story-step sb-reveal" data-stage="decide">
<span class="sb-story-step-number">03 / observe</span>
<h3>Sandboard<br /><em>tracks the run.</em></h3>
<p>Sandboard observes the agent’s output, keeps the card state current, and collects a plan, report, escalation, or split artifact. The worker cannot call the board directly.</p>
<a href="workflow.html">Learn the daily workflow</a>
</article>
<article class="sb-story-step sb-reveal" data-stage="merge">
<span class="sb-story-step-number">04 / review</span>
<h3>Review the<br /><em>pull request.</em></h3>
<p>Completed work arrives with its pull request and evidence. Sandboard can surface the change, but you merge it on GitHub.</p>
<a href="invariants.html">Read the invariants</a>
</article>
</div>
</div>
</section>

<section class="sb-boundary" id="boundary" aria-labelledby="boundary-title">
<div class="sb-reveal">
<p class="sb-label">who owns what</p>
<h2 id="boundary-title">Sandboard owns the work.<br /><em>OpenShell runs the agent.</em></h2>
</div>
<div class="sb-role-stack sb-reveal sb-delay-1">
<div class="sb-role"><span class="sb-role-index">01</span><h3>Sandboard</h3><p>Stores Projects and Tasks, serves the UI and MCP endpoint, and runs the supervisor that owns lifecycle transitions.</p></div>
<div class="sb-role"><span class="sb-role-index">02</span><h3>OpenShell</h3><p>Creates the sandbox and applies the image, network policy, and provider credential binding.</p></div>
<div class="sb-role"><span class="sb-role-index">03</span><h3>Agent</h3><p>Works inside the sandbox with repository access and no network path back to Sandboard.</p></div>
<div class="sb-role"><span class="sb-role-index">04</span><h3>GitHub</h3><p>Receives the pull request. You review and merge it; Sandboard does not merge it for you.</p></div>
</div>
</section>

<section class="sb-start" id="start" aria-labelledby="start-title">
<div class="sb-reveal">
<p class="sb-eyebrow">the complete setup</p>
<h2 id="start-title">Run Sandboard with OpenShell.<br /><em>Then dispatch agent work.</em></h2>
<p class="sb-start-copy">Start the board locally, connect an OpenShell gateway, choose a sandbox spec, and attach a provider. Without those pieces, Sandboard can show the board but it cannot run an agent.</p>
<div class="sb-start-links">
<a href="quickstart.html">Quickstart <span aria-hidden="true">↗</span></a>
<a href="tour.html">Read the tour <span aria-hidden="true">↗</span></a>
<a href="first-agent.html">First agent <span aria-hidden="true">↗</span></a>
</div>
</div>
<div class="sb-start-panel sb-reveal sb-delay-1">
<div class="sb-code-card">
<div class="sb-code-card-head"><span>local / first run</span><span>01—03</span></div>
<div class="sb-code-text" role="textbox" aria-label="Quickstart commands"><span>git clone https://github.com/sandboard-app/sandboard.git</span><span>cd sandboard</span><span>cargo run</span></div>
<button class="sb-copy sb-code-copy" type="button" data-copy="git clone https://github.com/sandboard-app/sandboard.git\ncd sandboard\ncargo run" aria-label="Copy quickstart commands">copy</button>
</div>
</div>
</section>

<footer class="sb-footer">
<a href="index.html">sandboard</a>
<span class="sb-footer-center">Sandboard + OpenShell + GitHub</span>
<a class="sb-footer-right" href="https://github.com/sandboard-app/sandboard">github ↗</a>
</footer>
</div>
