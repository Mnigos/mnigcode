# Mnig Code - Product and Technical Specification

## Status

- Draft created: 2026-04-04
- Updated: 2026-04-07
- Owner: Igor Makowski
- Working folder: `personal-apps/mnigcode`
- Working title: `Mnig Code`
- Current stage: working proof of concept, not MVP

## Vision

Build a desktop app that combines a real Zed-based editor with a first-class coding-agent experience in one product.

The app should let developers manage many repositories in a single window, run multiple AI sessions in parallel, switch between projects quickly, and open or preview code directly in the integrated Zed editor without bouncing between separate tools.

This is not a "Codex shell with an editor bolted on later". The editor is part of the product from day one.

## Problem Statement

AI-native development changed the shape of the workflow:

1. Developers now manage multiple repositories concurrently while agents run in the background.
2. The workflow is fragmented across at least three tools:
   - AI app
   - editor
   - terminal
3. Switching between multiple editor windows for different projects is slow and mentally expensive.
4. Existing AI wrappers around Codex solve chat and orchestration, but not the "real editor in the same app" problem.

## Product Thesis

If we combine:

- a performant editor developers already like using
- multi-repository orchestration
- first-class agent chat, approvals, and thread management, starting with Codex

then we can remove the biggest friction in AI-heavy development without asking users to leave their preferred editing experience.

## Product Principles

1. Real editor, not a preview-only code pane.
2. Multi-repo is a first-order concept, not an afterthought.
3. Agent workflows should feel native, not embedded as a webview afterthought.
4. MVP should stay simple, but it must solve the main pain points completely.
5. The product should preserve Zed's speed and keyboard-driven feel.
6. Codex is the first harness, but the architecture must not be Codex-only.

## MVP Goals

1. Single desktop app window.
2. Multiple repositories visible and manageable from one sidebar.
3. Two modes:
   - `Editor`
   - `Agents`
4. Full Zed-based editing experience for the active repository.
5. Codex chat inside the same app, with concurrent repo-scoped sessions.
6. Quick switching between repositories without opening multiple editor windows.
7. Ability to jump from Codex output directly to files in the editor.
8. Codex-style model and reasoning controls in the composer.
9. Image and file attachments in the composer.
10. Visible context-window usage indicator.
11. Explicit permission mode selection, including safer/default mode and full-access mode.
12. Shared product shell across `Editor` and `Agents` mode, while preserving native Zed editor UI for MVP.

## Explicit MVP Non-Goals

1. Integrated terminal.
2. Claude Code integration as a shipped provider.
3. Full parity with every official Codex setting or feature.
4. Cross-device sync.
5. Collaboration or multiplayer editing features beyond what already exists in Zed.
6. Simultaneous tiled display of many repositories at once.
7. Generic provider marketplace.
8. Multi-window project workflows as the default path.
9. Deep customization of every Zed setting or every official Codex setting.

The MVP should prepare for future harnesses such as Claude Code, OpenCode, Cursor-style agent backends, and other local or remote agent runtimes. It should not expose them yet unless they fall out naturally from the abstraction.

## Primary User

An AI-heavy developer who actively works across 3-20 repositories and wants:

- one app window
- one repo sidebar
- one place for agent activity, starting with Codex
- one place to inspect and edit code

## Core User Stories

1. As a developer, I want to pin several repositories in one app so I do not need multiple editor windows open.
2. As a developer, I want to switch between repository contexts quickly while agents keep running.
3. As a developer, I want to open a file mentioned by Codex directly in the embedded editor.
4. As a developer, I want a dedicated `Agents` mode for chats, diffs, approvals, and thread history.
5. As a developer, I want a dedicated `Editor` mode where I can focus on code for the currently selected repository.
6. As a developer, I want concurrent Codex sessions across repositories so I can let one agent work while I inspect another project.
7. As a developer, I want to select the Codex model and reasoning level before starting or continuing a thread.
8. As a developer, I want to attach files and screenshots to a Codex message without leaving the app.
9. As a developer, I want to see how full the current context window is before continuing a long session.
10. As a developer, I want to choose whether an agent runs in safer/default permissions or full-access mode before it acts.
11. As a developer, I want the app to feel like one product across editor and agent workflows, while still keeping the editor familiar and native to Zed in MVP.
12. As a developer, I want agent responses to render markdown, code snippets, tool calls, file references, and links clearly.
13. As a developer, I want completed agent runs to show a summary of changed files and code diffs before I inspect or undo the work.
14. As a developer, I want the project/thread sidebar to stay visible when I switch between `Editor` and `Agents` mode.
15. As a developer, I want editor-specific navigation like file tree and Git changes to live in a separate contextual sidebar that does not replace the project/thread sidebar.
16. As a developer, I want to collapse either sidebar when I need more room.

## Product Shape

### Global App Shell

- Persistent left sidebar with projects rendered as directory groups
- Threads rendered as nested items inside their project group
- Contextual right sidebar for active-mode panels, such as file tree, Git changes, diffs, and related editor/agent tools
- Both left and right sidebars are collapsible
- Global command palette
- Active run indicators
- Notifications for completed or blocked agent work
- Recent projects and pinned projects
- Stable mode switcher for `Editor` and `Agents`
- Shared Codex-style bottom composer/control area where appropriate

### Editor Mode

- Zed editor for the selected repository
- File tree in the contextual right sidebar
- Tabs
- Search
- Git changes in the contextual right sidebar
- Diff viewing in the contextual right sidebar or center editor surface
- "Open changed files from latest agent run"
- Same app shell, sidebar, mode switcher, and project/thread model as `Agents` mode
- Active repository state preserved when switching away and back
- Agent status remains visible without taking over the editor surface
- Native Zed editor chrome can remain mostly unchanged in MVP

### Agents Mode

- Repo-scoped chat threads
- Streaming turn output
- Approvals
- Diff summary
- Thread history
- Thread resume
- Jump-to-file actions from messages or file changes
- Markdown rendering
- Syntax-highlighted code snippets
- Tool call rendering
- File references
- Clickable links
- Final run summary with elapsed time and changed-file totals
- Code change review with diffs and undo/revert affordance where feasible
- Model selector
- Reasoning selector
- Image and file attachments
- Context-window usage indicator
- Permission mode selector
- Optional contextual right sidebar for run details, diffs, approvals, or file references when useful

### Agent Transcript Rendering

The transcript is part of MVP quality, not polish. It should support:

- markdown paragraphs, headings, lists, inline code, and blockquotes
- fenced code blocks with syntax highlighting
- streamed assistant text without layout jank
- tool call start/completion states
- command output and structured tool results
- file references that open the file in `Editor` mode
- external links that open in the browser
- inline errors and permission prompts

MVP can start with a pragmatic markdown subset, but raw plaintext transcripts are not acceptable for MVP.

### Code Change Review

When an agent finishes a run that changed files, the app should show a Codex-like completion section:

- elapsed run time, such as "Worked for 1m 14s"
- summary of changed files
- per-file additions and deletions, such as `SPEC.md +32 -19`
- generated summary of what changed
- expandable diff preview
- jump-to-file action
- undo/revert action where technically safe

This can initially be lightweight, but the user must be able to understand what the agent changed without leaving the thread.

### Composer and Run Controls

The MVP composer should feel close to the current Codex app interaction model:

- rounded prompt container
- attachment button for files and images
- model selector, such as `GPT-5.4`
- reasoning selector, such as `High`
- context-window usage ring with tooltip details
- environment selector, such as local workspace
- permission selector, including default/safe mode and full access
- send/stop control

These controls are part of MVP because they affect run quality, safety, and user confidence. They are not visual polish.

### Sidebar Information Architecture

The sidebar should follow the Codex-style mental model:

```text
Projects
  mnigcode/
    Fix missing cargo command
    Add Agents mode default
    Refine MVP strategy
  product-ever/
    Check Playwright access
    Implement Phase 1 backlog
  ludus-api/
    Fix Docker env
```

Rules:

- projects are the top-level directory-like items
- threads are nested under their owning project
- each thread can show recency and run state
- projects can show aggregate status, such as running, blocked, unread, or failed
- selecting a project changes the active repository
- selecting a thread opens that thread in `Agents` mode for its project
- switching projects must not open a separate window
- the project/thread sidebar remains present when switching between `Editor` and `Agents` mode
- collapsing the left sidebar hides project/thread navigation but does not change the active project or active thread

Implementation rule:

- do not reuse the existing upstream Zed multi-workspace sidebar as the product sidebar
- the existing sidebar implementation can be used only as a reference for GPUI patterns, project grouping, status derivation, and same-window activation safety
- Mnig Code needs a product-owned sidebar that intentionally matches the Codex-style project/thread tree and can evolve independently from upstream Zed's current sidebar UX
- the product sidebar should be designed as a stable shell element, not as a temporary panel or a repurposed Zed workspace switcher

### Contextual Right Sidebar

The app should reserve the right side for context-specific panels. This keeps the persistent project/thread sidebar separate from editor and agent tools.

In `Editor` mode, the right sidebar can host:

- file tree
- Git changes
- search/results panels where appropriate
- outline or symbols where appropriate
- changed files from the latest agent run

In `Agents` mode, the right sidebar can host:

- diffs
- approvals
- run details
- tool call details
- file references

Rules:

- the right sidebar is collapsible
- the right sidebar content can change by mode
- the left project/thread sidebar should not be replaced by file tree or Git changes
- collapsing either sidebar should preserve active project, active thread, editor state, and run state

### Visual Direction

MVP should prioritize workflow coherence over a full visual redesign.

The long-term direction is "Codex-like orchestration with a real editor inside". The MVP does not need to make every inherited Zed surface look like Codex.

UI principles:

- use a shared app shell inspired by the Codex app where it supports the workflow
- make both `Editor` and `Agents` mode share the same outer layout and interaction flow
- preserve native Zed editor performance, editing quality, and editor chrome in MVP
- keep the sidebar, mode switcher, repo/thread hierarchy, and run controls visually consistent across modes
- keep the left project/thread sidebar persistent across mode switches
- use the right sidebar for contextual editor or agent panels
- use calm contrast, clear focus states, and compact but readable spacing
- avoid broad Zed UI reskinning until the core product workflow is stable

Agent chat is an exception to the broad-reskin deferral. The first agent chat view should already move toward the Codex app interaction model because the chat transcript, composer, and thread layout are the core product experience, not inherited editor chrome.

The center surface changes by mode:

- `Editor` mode: real Zed editor for the active project
- `Agents` mode: Codex thread timeline, composer, approvals, diffs, and run output

The shell should stay stable while the center surface changes.

Deferred visual polish:

- deeper rounding and softening of inherited Zed panels
- redesign of project panel, command palette, settings, git UI, diagnostics, and other inherited non-agent surfaces
- full Codex-like theme pass for the editor chrome
- custom iconography and animation system

These are intentionally after MVP unless a specific inherited surface blocks the core workflow.

## UX Model for Multi-Repo in MVP

Zed today is still centered around a one-folder, one-workspace model. MVP should not wait for upstream multi-root support to exist.

Instead, MVP should implement:

- one app window
- many repository sessions loaded by the app
- one active repository editor surface at a time
- background Codex activity continuing for inactive repositories

This gives the user a multi-repo product immediately while keeping the editor model understandable and technically achievable.

## Technical Decision

### Decision: Fork Zed, Do Not Build a Thin Wrapper Around It

Reason:

- The whole value proposition requires a real Zed editor inside the app.
- A plugin path is too constrained for the product we want.
- A separate shell that launches Zed externally does not solve the core pain point.

### Decision: Use Native Codex Integration, Not Only ACP

Reason:

- Zed ACP is useful as a reference and may help for prototyping.
- However, stock Zed's current Codex path through external agents does not expose all the behavior we want from the official Codex experience.
- We want thread history, approvals, richer session management, and as much Codex parity as practical.

Therefore:

- prototype can inspect ACP behavior
- product architecture should target direct integration with `codex app-server`

### Decision: Use a Real Zed Fork as the Product Repository

The current local PoC can live in a wrapper folder during experimentation, but the product should move to a canonical fork repository.

Recommendation:

- create a GitHub fork of `zed-industries/zed`
- make the Zed fork the root of the product repository
- keep `upstream` pointed at `zed-industries/zed`
- keep product docs in the fork, such as `docs/product/`
- keep product changes modular to reduce upstream merge pain
- do not maintain the product long-term as a wrapper repo containing `zed-fork/`

Reason:

- CI, packaging, app identity, upstream merges, and release automation are simpler when the fork itself is the product repo.
- The MVP requires app-shell and workspace behavior changes that are deeper than a plugin or wrapper should own.

### Decision: Treat the Product as Open Source by Default

The Zed fork includes strong copyleft licensing obligations. Before public distribution, licensing must be reviewed carefully. The working assumption for product planning is:

- the Mnig Code fork should be open source
- distributed builds must comply with upstream and third-party license obligations
- proprietary closed-source distribution should not be assumed viable

Monetization can still exist:

- paid signed builds
- supporter license
- early access builds
- priority support
- paid packaging and auto-update convenience
- future optional hosted services around sync, team visibility, or managed agent history

GitHub Sponsors can be a channel, but should not be the only monetization strategy.

## High-Level Architecture

### 1. Forked Zed Application

Use Zed itself as the base desktop application and editor runtime.

Responsibilities:

- windowing
- panes
- editor
- project loading
- keyboard and command system
- file navigation

### 2. Multi-Repo Orchestrator

New app-specific layer added in the fork.

Responsibilities:

- maintain repository registry
- pin and unpin projects
- persist recent and active repositories
- track the active repository session
- keep inactive repository sessions alive
- surface run state and notifications in the sidebar
- guarantee that add/open project flows target the current app window by default

### 3. Codex Integration Layer

Native integration against `codex app-server`.

Responsibilities:

- authentication
- thread creation and listing
- thread resume
- turn streaming
- approvals
- rate limit display
- repo-to-thread association
- tool call and tool result event mapping
- code change summary extraction
- changed-file and diff event mapping
- model selection
- reasoning selection
- attachment upload or attachment handoff
- context-window usage display
- permission mode configuration

This layer should be the first implementation of a provider-neutral harness boundary. Codex-specific protocol details can live here initially, but the app-facing API should be named around agents, sessions, threads, turns, approvals, attachments, context usage, and file changes rather than Codex-only concepts. Future harness adapters should be able to target the same app-facing interface.

Future harness candidates:

- Claude Code / Claude Agent
- OpenCode
- Cursor-style agent backends if a usable integration path exists
- other ACP-compatible or app-server-like coding agents

### 4. Agent UI Layer

Custom views and panels inside the Zed fork.

Responsibilities:

- `Agents` mode
- thread list
- markdown message rendering
- code snippet rendering
- tool call rendering
- file and link rendering
- diff rendering
- completed-run change summary
- approval UI
- run state UI
- composer controls
- attachment UI
- context indicator
- permission selector

### 5. Repository Session Model

Each pinned repository gets an internal session model.

Suggested fields:

- repo id
- local path
- display name
- pinned state
- open files
- active branch
- current editor state
- active Codex thread id
- running agent count
- unread activity count
- last activity timestamp
- preferred model
- preferred reasoning level
- preferred permission mode
- context-window usage summary

## Codex Integration Design

### Why `codex app-server`

The app-server exposes the primitives we need for a real client:

- login and logout
- ChatGPT-managed auth and API-key auth
- thread listing
- loaded-thread state
- turn event streaming
- diff updates
- rate limit reads and updates

That makes it the best foundation for reproducing official Codex workflows inside a custom desktop client.

### Codex Process Model

Recommended MVP approach:

- one shared background `codex app-server` process per app instance
- all repositories use the same service
- app maps threads to repository paths using `cwd`

Why this is preferable:

- simpler auth model
- shared thread history
- lower process overhead
- better alignment with app-server's existing thread filtering by cwd

### Possible Future Alternative

If isolation becomes necessary later:

- one Codex process per repo

This should not be the MVP default unless shared-server limitations appear during spikes.

## Zed Integration Strategy

### MVP Strategy

Keep Zed as the editor foundation and add new top-level product concepts:

- repository sidebar
- mode switcher
- Codex panels
- repo switching

### Important Constraint

Zed currently documents a one-folder, one-workspace model, and community demand for multi-root workspaces exists but should not be assumed solved for MVP.

Implication:

We should implement multi-repo orchestration ourselves in the fork rather than waiting on upstream workspace changes.

## Why T3 Code Is Relevant But Not Sufficient

T3 Code is useful as a reference for:

- Codex-centered UX ideas
- app-server-backed client architecture
- settings and provider concepts

It is not sufficient as the base for this product because:

- the core requirement is a real embedded Zed editor
- the main pain point is editor and repository orchestration
- this product needs native editor-level integration, not a frontend-only Codex shell

## Detailed MVP Scope

## First Version Scope

The first implementation in the fork should be smaller than the full MVP, but it should point in the right architectural direction.

First version must include:

1. Codex authentication and run flow.
2. A product-owned left sidebar that displays projects as directory groups and threads nested under those projects, matching the Codex sidebar mental model.
3. `Editor` and `Agents` modes with a stable shared shell.
4. A chat view that is visually closer to Codex than stock Zed Agent Panel, while still allowed to render messages as plain text for now.
5. Same-window project switching and same-window thread activation.

First version can defer:

1. Markdown rendering.
2. Tool call rendering.
3. File reference rendering.
4. Link rendering.
5. Diff rendering.
6. Final code-change summary rendering.
7. Full model/reasoning/permission/context/attachment parity if it would block the core shell.

First version must not:

1. Reuse the existing upstream Zed multi-workspace sidebar as the product sidebar.
2. Implement the sidebar as a thin restyle of an existing Zed panel if the interaction model remains wrong.
3. Hard-code the app architecture so deeply to Codex that Claude Code, OpenCode, or another harness would require a shell rewrite later.

### In Scope

1. Forked Zed-based desktop app.
2. Left sidebar listing repositories.
3. Add local repositories to the app.
4. Pin and reorder repositories.
5. `Editor` mode.
6. `Agents` mode.
7. Repo-scoped Codex threads.
8. Streaming agent output.
9. Approval flow for actions requiring approval.
10. Thread history for each repository.
11. Jump-to-file from Codex output.
12. Status badges for running, blocked, completed, and failed runs.
13. Session restore on app reopen.
14. Model selector in the composer.
15. Reasoning selector in the composer.
16. Image attachments.
17. File attachments.
18. Context-window indicator.
19. Permission mode selector for default/safe mode versus full access.
20. Codex-style project/thread tree in the sidebar.
21. Shared product shell for both `Editor` and `Agents` mode, with native Zed editor UI preserved for MVP.
22. Markdown transcript rendering.
23. Syntax-highlighted code snippet rendering.
24. Tool call and tool result rendering.
25. Clickable file references and links.
26. Completed-run code change summary.
27. Expandable per-file diff previews.
28. Undo/revert affordance for agent-applied changes where technically safe.
29. Persistent left project/thread sidebar across both modes.
30. Contextual right sidebar for file tree, Git changes, diffs, approvals, and related panels.
31. Collapsible left and right sidebars.

### Out of Scope

1. Integrated terminal panel.
2. Claude Code.
3. Non-Codex providers.
4. Full plugin compatibility work specific to this product.
5. Simultaneous editing of multiple repositories side by side.
6. Shared cloud account for repository metadata.
7. Full custom theme engine.
8. Complete redesign of every inherited Zed screen.
9. Hosted billing, teams, or cloud sync in MVP.

## Acceptance Criteria for MVP

1. User can open the app and add at least 5 repositories to the sidebar.
2. User can switch active repository in under one interaction path from the sidebar.
3. User can open `Agents` mode for repo A, start a Codex session, then move to repo B while repo A continues running.
4. User can return to repo A and see streamed or completed results.
5. User can click a changed file or referenced file from agent output and open it in the editor immediately.
6. User can move between `Editor` and `Agents` mode without losing repository context.
7. Restarting the app restores pinned repositories and recent thread associations.
8. User can select model and reasoning level before sending a Codex prompt.
9. User can attach at least one image and at least one file to a Codex prompt.
10. User can see current context-window usage while composing or viewing a thread.
11. User can choose default/safe permissions or full access before starting a run.
12. Sidebar renders projects as directory groups and threads as nested items under those projects.
13. Adding or opening a project from primary app surfaces does not create a separate window by default.
14. `Editor` and `Agents` mode share the same product shell and feel like one app, without requiring a full Zed UI reskin.
15. Agent messages render markdown and syntax-highlighted code blocks.
16. Tool calls and tool results are visibly distinct from normal assistant text.
17. File references in agent output open the referenced file in the editor.
18. Links in agent output are clickable.
19. A completed run with file changes shows elapsed time, changed-file summary, additions/deletions, and an expandable diff preview.
20. User can undo or revert agent-applied changes from the completed-run review when the app can do so safely.
21. Left project/thread sidebar remains present when switching between `Editor` and `Agents` mode.
22. Right sidebar can show file tree and Git changes in `Editor` mode.
23. Right sidebar can show diffs, approvals, or run details in `Agents` mode.
24. User can collapse and reopen both sidebars without losing active project, active thread, editor state, or run state.

## Suggested Internal Modules

### `app-shell`

- top-level window layout
- persistent left project/thread sidebar
- contextual right sidebar
- sidebar collapse state
- mode switching
- global notifications

### `repo-registry`

- local persistence
- repo metadata
- pinned ordering
- active selection

The app may reference Zed's existing `AgentPanel` implementation for lifecycle, focus, panel integration, and ACP behavior, but the visible chat UX should be adapted toward Codex: centered transcript, compact run separators, rounded composer, clearer thread header, and lightweight status controls. The first version can omit tool call and markdown rendering, but it should not preserve a UI shape that obviously feels like a generic Zed side panel.

### `repo-session`

- per-repo UI state
- last active thread
- unread activity
- open editor state

### `codex-client`

- app-server transport
- auth
- threads
- turn events
- approvals
- rate limits
- tool calls and tool results
- changed files
- diffs
- final run summaries
- model and reasoning parameters
- attachment mapping
- context-window usage
- permission mode mapping

### `agent-panels`

- threads list
- chat panel
- diff panel
- activity panel
- markdown renderer
- code block renderer
- tool call renderer
- file reference renderer
- completed-run summary
- composer controls
- context meter
- permission selector

### `design-system`

- shared shell primitives
- sidebar project/thread rows
- mode switcher
- composer controls
- status badges
- context indicator
- permission chips
- left/right sidebar layout primitives
- collapsible sidebar controls

This does not need to become a fully generic design system in MVP. It should cover the new product shell and Codex-facing surfaces first. Native Zed editor surfaces can remain visually native until the workflow is stable.

### `editor-bridge`

- map agent file references to Zed editor actions
- open file
- reveal in tree
- focus editor

## Primary Risks

### 1. Zed Fork Maintenance

Risk:

- maintaining a product fork of a fast-moving editor may create ongoing merge cost

Mitigation:

- keep changes modular
- minimize invasive core changes early
- document fork points carefully

### 2. Multi-Repo Model Friction

Risk:

- Zed's project model may assume a single active workspace in more places than expected

Mitigation:

- spike repository switching before broad UI work
- treat each repo as an app-level session abstraction
- avoid promising simultaneous multi-root editing in MVP
- audit every inherited Zed `Open` flow so project additions do not accidentally create new windows

### 3. Codex Feature Parity Gaps

Risk:

- some official Codex features may depend on behavior not fully documented in public materials

Mitigation:

- build MVP against the documented app-server surface first
- use parity as a directional goal, not a blocker

### 4. Performance Regressions

Risk:

- background agent state and additional panels could reduce Zed's responsiveness

Mitigation:

- keep background work off the UI thread
- measure startup, repo switch, and render performance early

### 5. Licensing and Distribution

Risk:

- shipping a forked editor has licensing, compliance, and distribution implications

Mitigation:

- confirm exact obligations before public distribution
- track third-party license requirements from day one
- plan monetization around open-source-compatible distribution

### 6. UI Fragmentation

Risk:

- the app may feel like a raw Zed fork with a separate Codex panel, rather than one coherent product

Mitigation:

- establish shared shell components early
- make the project/thread sidebar the single source of truth
- keep the outer shell consistent across modes
- defer broad Zed reskinning until after MVP
- avoid mode-specific one-off controls unless the workflow truly requires them

### 7. Sidebar Inheritance Trap

Risk:

- reusing the existing Zed multi-workspace sidebar would preserve behavior and visual structure that already failed the product goal

Mitigation:

- build a new product sidebar for Mnig Code
- reference upstream sidebar code only for safe GPUI patterns and workspace activation details
- keep project/thread grouping and interaction behavior defined by Codex-style UX, not by upstream Zed sidebar constraints

### 8. Harness Lock-In

Risk:

- building the first version directly around Codex-only names and assumptions could make Claude Code, OpenCode, or future harnesses expensive to add

Mitigation:

- define provider-neutral app-facing entities such as harness, session, thread, turn, approval, attachment, and file change
- keep Codex protocol details behind the first harness adapter
- keep UI copy Codex-specific only where the selected harness is actually Codex

## Open Questions

1. Should MVP hard-disable project-opening paths that create new windows, or keep them hidden behind an advanced command?
2. Should repository switching preserve a separate tab stack per repo?
3. Should agent runs be one-per-repo or many-per-repo in MVP?
4. Which parts of Zed's existing AI UI should be referenced for behavior, and which should be replaced for product UX?
5. Do we want a lightweight diff review panel in `Agents` mode or simply jump into editor diffs?
6. Should approvals live inline in chat or in a dedicated action queue?
7. Which exact model and reasoning options should MVP expose first?
8. Should permission mode be global, per-project, per-thread, or per-run?
9. Should attached files be copied into Codex-managed state, referenced by path, or uploaded through app-server primitives?
10. What source of truth should drive the context-window indicator if the app-server does not expose all details directly?
11. Which markdown renderer should we reuse or build for GPUI?
12. Should diffs render inline in the transcript, in a side panel, or both?
13. Should undo/revert use Git state, editor buffer state, Codex-provided patch metadata, or a product-specific checkpoint?
14. Which editor panels must be in the right sidebar for MVP: file tree only, file tree plus Git changes, or more?
15. Should the right sidebar remember separate panel selection per mode and per project?
16. What is the minimum provider-neutral harness interface needed before adding Claude Code or OpenCode?

## Recommended Technical Spikes

### Spike 1: Zed Fork Extension Points

Goal:

- identify where to add:
  - global repository sidebar
  - mode switcher
  - custom Codex panels

Success criteria:

- custom app-level sidebar visible in a forked build
- ability to switch visible project context

### Spike 2: Native Codex Client

Goal:

- validate direct integration with `codex app-server`

Success criteria:

- login flow works
- thread list works
- one thread can start and stream updates
- diff and approval events can be rendered
- markdown messages render with code blocks
- tool call events render distinctly
- file references and links are clickable
- changed-file summaries and diffs can be rendered for completed runs
- model and reasoning parameters can be sent
- file and image attachment path works
- context-window usage can be displayed or approximated
- permission mode can be configured

### Spike 3: Repo Session Switching

Goal:

- prove one app window can manage multiple repository sessions cleanly

Success criteria:

- add three repos
- switch active repo
- preserve previous repo editor state
- background Codex activity survives switches
- no primary add/open project flow creates a new window

### Spike 4: Unified Product Shell

Goal:

- prove the product-owned project/thread sidebar, composer controls, and mode switch can wrap both the agent view and the editor view

Success criteria:

- projects render as sidebar groups
- threads render inside project groups
- `Editor` and `Agents` mode share the same outer product shell
- left project/thread sidebar persists across mode switches
- right sidebar can render mode-specific panels
- both sidebars can collapse and reopen
- model, reasoning, permission, attachment, and context controls are visible in the composer area
- native Zed editor chrome remains usable and does not block the shell workflow

### Spike 5: Harness Boundary

Goal:

- define the minimum provider-neutral boundary for Codex first, Claude Code/OpenCode later

Success criteria:

- app-facing types avoid Codex-only naming except in the Codex adapter
- thread listing, thread opening, turn sending, auth state, and run status are represented generically
- the first Codex implementation can be swapped behind the boundary without rewriting the sidebar or chat shell

## Proposed Milestones

### Milestone 1: Feasibility

- fork Zed
- build locally
- map relevant subsystems
- validate custom panels
- validate `codex app-server` connection

### Milestone 2: Foundation

- create repo registry
- persist pinned repos
- create repo session model
- establish shared Codex service manager
- normalize project add/open flows to current-window multi-workspace behavior
- move product work into a canonical Zed fork repository

### Milestone 3: Core UI

- product-owned repository sidebar
- project/thread tree
- persistent left sidebar
- contextual right sidebar
- collapsible sidebars
- `Editor` and `Agents` mode switch
- agent thread list
- message streaming
- run badges
- shared product shell
- composer controls for model, reasoning, permissions, attachments, and context
- Codex-like chat shell and composer, even if transcript rendering starts plain-text

### Milestone 4: MVP Workflow

- jump-to-file from agent output
- approvals
- thread history
- session restore
- polish switching flow
- attachment flow
- context-window display
- permission mode persistence
- markdown transcript rendering
- tool call rendering
- completed-run change summary
- diff preview and safe undo/revert flow

## Initial Build Order

1. Move from local nested PoC to a canonical Zed fork repository.
2. Fork and compile Zed in that repository.
3. Normalize project add/open flows so primary UX stays in one window.
4. Add a product-owned Codex-style project/thread sidebar. Do not reuse upstream Zed's current multi-workspace sidebar as the product sidebar.
5. Add persistent left sidebar, contextual right sidebar, and shared app shell.
6. Add a local repository list backed by simple persistence.
7. Add collapsible sidebar behavior.
8. Define a provider-neutral harness boundary.
9. Integrate Codex as the first harness.
10. Render a Codex-like minimal chat panel for one repository.
11. Add model and reasoning selectors.
12. Add permission mode selector.
13. Add context-window indicator.
14. Add image and file attachments.
15. Add repo switching and per-repo thread association.
16. Add markdown transcript rendering.
17. Add code block and tool call rendering.
18. Add file reference and link handling.
19. Add jump-to-file and diff flow.
20. Add completed-run change summary.
21. Add safe undo/revert flow where technically feasible.
22. Add approvals and activity badges.

## Suggested Repository Structure

The product should eventually live as a full Zed fork, not as a wrapper repository with `zed-fork/` nested inside it.

Product docs and modules should map into the fork rather than exist as a separate app shell:

```text
mnigcode/
  crates/
    mnigcode_shell/
    mnigcode_repo/
    mnigcode_harness/
    mnigcode_codex/
    mnigcode_ui/
  SPEC.md
  docs/
    product/
    architecture/
    licensing/
```

Names are provisional. The important rule is to keep product-specific modules isolated enough that upstream Zed merges remain manageable.

## Research Summary

| Source | Key Finding | Product Implication |
| --- | --- | --- |
| Zed open-source announcement | Zed editor is open source; GPUI is separate and permissively licensed | Forking is possible, but licensing must be handled intentionally |
| Zed docs: VS Code migration | Zed documents a one-folder, one-workspace model | MVP cannot rely on native multi-root support being present |
| Zed issue #15120 | Multi-root workspaces are a known user request | Confirms the pain exists and validates our product direction |
| Zed ACP / external agents | Zed supports external agents and Codex via ACP | Useful for reference and possibly prototypes, but not enough by itself |
| Existing Zed multi-workspace sidebar | Zed has code for project grouping and thread metadata | Useful for implementation reference, but should not be reused as the product sidebar because the UX does not match the Codex-style app shell |
| OpenAI Codex app-server README | Public app-server exposes auth, threads, turn events, diff updates, and rate limits | Best path to a native Codex client inside the fork |
| T3 Code docs | Similar product direction exists on the Codex side | Useful inspiration, but not enough for real editor integration |
| Local PoC | Agents mode, project/thread sidebar, and multi-workspace switching are feasible inside the fork | Confirms the direction is doable, but current implementation is only a PoC |

## Relevant Links

### Zed

- Zed repository: <https://github.com/zed-industries/zed>
- Zed open-source announcement: <https://zed.dev/blog/zed-is-now-open-source>
- Zed ACP overview: <https://zed.dev/acp>
- Zed external agents docs: <https://zed.dev/docs/ai/external-agents>
- Zed VS Code migration docs: <https://zed.dev/docs/migrate/vs-code>
- Zed multi-root issue: <https://github.com/zed-industries/zed/issues/15120>
- Codex ACP adapter for Zed: <https://github.com/zed-industries/codex-acp>

### Codex

- Codex repository: <https://github.com/openai/codex>
- Codex app-server README: <https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md>
- Introducing the Codex app: <https://openai.com/index/introducing-the-codex-app/>
- Codex harness overview: <https://openai.com/index/unlocking-the-codex-harness/>

### Related Reference

- T3 Code docs: <https://www.mintlify.com/pingdotgg/t3code/installation>

## Immediate Next Step

Create a technical discovery plan focused on four spikes:

1. Zed fork customization points
2. Native `codex app-server` client integration
3. Multi-repo session switching inside one app window
4. Unified product shell and composer controls

That plan should produce enough evidence to decide whether MVP complexity is acceptable before any major implementation begins.
