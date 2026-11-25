# Step 1b: Project Router (root resolution, Project IDs, hardened Git, file watcher, schema room)

Status: resolved

Source: `docs/roadmap.md` (Step 1b), `docs/steps.md` (Step 1, Build item 3 and the `.tractionignore` part of item 5), `docs/protocol.md` sections 2 and 2a, `docs/adr/0001-track-every-project-folder-with-full-paths.md`, and the AGENTS.md invariants "Untrusted-Repo Git Hardening", "Project Tracking Scope", "`FILE_MUTATION` Attribution" and "`TractionEvent` Schema Room".

Gate items this sub-step owns (from the Step 1 Exit Gate):

- [x] Switching across 3 Git repos (including a Git worktree) and 2 non-Git Project folders routes all events to the correct Project IDs without touching `.git/index.lock`. System and dependency folders never become Projects. `git init` on a tracked folder migrates its history.
- [x] Hand-editing files in a known Project with no Harness running produces `FILE_MUTATION` events.
- [x] (AGENTS.md red-team requirement) A repo-local `core.fsmonitor` command never executes.

Gate evidence: 3-OS CI green on `21deeb9` (https://github.com/rubixnet/bounty/actions/runs/45612520436).

## Problem Statement

After 1a, the daemon accepts and stores Traction Events, but it has no idea which Project an event belongs to. Each Adapter would have to compute `project_id` itself, and it can't: the `dir:` form needs a device secret, and the `git:` form needs Git calls that Adapters shouldn't make. Nothing collapses Git worktrees into their repo, nothing keeps `~`, `node_modules` or `/usr` from becoming Projects, and a folder that later runs `git init` would split into two Projects with its history stranded under the old one.

The daemon also sees only AI activity. A Hack Club teen who codes by hand with no Harness open gets no credit, which breaks ADR 0001's promise that every folder they build in counts. Today, crediting hand edits correctly means telling them apart from edits a Harness made.

Finally, the daemon is about to run `git` inside folders the user cloned from strangers. A repo's `.git/config` can name programs (`core.fsmonitor`, hooks) that Git runs on ordinary commands. A naive `git status` would let any cloned repo run code as the user and could grab `.git/index.lock`, breaking the user's own Git commands.

From the user's point of view: "Every folder I work in, by hand or with an AI, shows up as one Project with all its time, even after I `git init` it or open a worktree. My home folder and `node_modules` never show up. Traction never runs anything from a repo I cloned, and it never gets in the way of my Git."

## Solution

Add a **Project Router** to `traction-daemon`, plus the schema room it needs in `traction-core`:

- **Root resolution.** For every accepted Traction Event, the daemon resolves `cwd` to a Project Root: the Git repo root (worktrees collapsed through `--git-common-dir`), else the nearest ancestor with a project marker, else the folder the Harness session started in. Excluded folders (OS folders, `~` itself, caches, dependency folders, user-excluded folders) never become Project Roots.
- **Project IDs.** The daemon is the only place `project_id` is derived. `git:` IDs come from the repo's root commit and normalized origin. `dir:` IDs come from a per-device secret and the canonical Project Root path. When a `dir:` Project gets its first commit, the daemon records an alias from the old `dir:` ID to the new `git:` ID. Stored events are never rewritten: the events table is append-only, and every Project query resolves aliased IDs, so history stays together and stays hash-chain safe.
- **Hardened Git wrapper.** Every daemon `git` call goes through one typed wrapper. It allows only read-only plumbing (`rev-parse`, `rev-list`, `config --get`). It always passes `--no-optional-locks -c core.fsmonitor=false -c safe.bareRepository=explicit` plus a per-OS `core.hooksPath` null device (`/dev/null` on Unix, `NUL` on Windows). It kills the child after a 5-second hard timeout and skips repos rooted at `$HOME`.
- **File watcher.** The daemon watches every known Project, honouring `.gitignore`, `.tractionignore` and the folder exclusions. On startup it re-attaches watchers to every Project stored in SQLite, and a Project path can also be registered directly over the authenticated bus. It emits metadata-only `FILE_MUTATION` events. Each mutation is attributed to a Harness only inside that Harness's window:
  - the exact hook window for hook/plugin sources;
  - for log-tailed sources, the active turn, or a 30-second trailing fallback window when the source has no turn boundaries.

  A 5-second hold lets late log-tailed events claim an edit. Outside any window it is manual work. Mutations that happened while the daemon was down are never backfilled.
- **Schema room on `schema_version: 1`.** `cwd` becomes optional, every event carries a closed `attribution_source`, and `FILE_MUTATION` events may carry no Harness at all (manual work). Reserved browser, editor and `manual_pick` values exist now, so adding those sources later needs no schema migration.

When 1b is done, an Adapter (or `curl`) sends a Traction Event with only a `cwd`, and the daemon stores it under the right Project. Hand edits in that Project appear as `FILE_MUTATION` rows attributed either to the Harness whose window was open or to manual work.

## User Stories

**Routing to the right Project**

1. As an Adapter author, I want to send a Traction Event with just a `cwd` and no `project_id`, so that I never need Git, the device secret or the routing rules in my Adapter.
2. As a user, I want every event from inside a Git repo routed to that repo's Project, whatever subfolder the Harness was in, so that my time isn't split across subfolders.
3. As a user, I want a Git worktree of a repo to route to the same Project as the main checkout, so that `claude --worktree` or `git worktree add` never creates a second Project.
4. As a user, I want a folder with a `package.json`, `Cargo.toml`, `pyproject.toml`, `go.mod`, `*.sln` or `.traction-project` routed to that folder even with no Git, so that local-only work counts as a Project (ADR 0001).
5. As a user, I want a folder with no Git and no marker routed to the folder my Harness session started in, so that a brand-new empty folder still gets credit.
6. As a user, I want a Harness that `cd`s into a subfolder of the same Project mid-session to keep routing to that Project, so that my session isn't fragmented.
7. As a user, I want a Harness that moves into a different Project mid-session to route later events to the new Project, so that time goes where the work happened.
8. As a user, I want events from 3 different Git repos and 2 non-Git folders, interleaved, all routed correctly, so that switching between Projects never mixes up their hours.
9. As a user, I want the same folder to always get the same `dir:` ID on my device, across daemon restarts, so that a Project's history stays together.
10. As a user, I want two different folders to never share a `dir:` ID, so that two Projects never merge by accident.
11. As a user, I want my `dir:` IDs to be impossible to recompute from just the path, so that someone who sees an ID can't confirm which folder it is without my device secret.
12. As a user, I want a repo's `git:` ID derived from its root commit and normalized origin, so that the same repo cloned on two devices can be matched on the website.
13. As a user, I want a repo's `git:` ID to stay pinned after it is first seen, even if I later add or change `origin`, so that adding a remote doesn't split my Project.
14. As a user with a repo that has several root commits, I want one deterministic choice of root commit, so that the ID is stable.
15. As a user, I want a Git repo with no commits yet to stay a `dir:` Project, so that an empty `git init` doesn't produce an unstable ID.

**Migration on `git init`**

16. As a user, I want a tracked non-Git folder that gets `git init` and a first commit to become a `git:` Project with all its earlier history, so that starting version control never costs me hours.
17. As a user, I want events that still arrive with the old `dir:` ID after migration to be stored under the new `git:` ID, so that in-flight Adapter events don't recreate the old Project.
18. As a user, I want migration to happen once and survive a daemon restart, so that a crash mid-way never leaves history split.
19. As a maintainer, I want migration recorded by the single SQLite writer in one transaction, so that the WAL single-writer rule still holds.
20. As a developer of the Step 3 hash-chain, I want stored Traction Events never updated or deleted, with migration recorded only as an alias, so that events hashed before a migration still verify afterwards.
21. As a maintainer, I want every query for a Project's events to include events stored under any ID aliased to it, so that migrated history is never invisible.

**Folder exclusions**

22. As a user, I want my home folder itself to never become a Project, so that a Harness started in `~` doesn't log my whole account as one Project.
23. As a user, I want OS folders (`/`, `/usr`, `/etc`, `/System`, `/Library`, `C:\Windows`, `C:\Program Files`, …) never to become Projects, so that system activity is never tracked.
24. As a user, I want cache and toolchain folders (`~/.cache`, `~/Library/Caches`, `~/.cargo`, `~/.rustup`, `~/.npm`, pnpm stores, temp folders) never to become Projects, so that tool noise isn't tracked.
25. As a user, I want a Harness working inside `node_modules/<pkg>`, `target/`, `.venv/` or `.git/` to route to the enclosing Project, not to the dependency, so that dependency markers like a package's own `package.json` never create Projects.
26. As a user, I want a dotfile repo rooted at `$HOME` ignored as a Git repo, so that my dotfiles repo doesn't swallow every folder in my home.
27. As a user, I want to list folders in a global Traction ignore file so that events from inside them are dropped entirely, so that private work is never recorded.
28. As an Adapter author, I want events dropped by a user exclusion to still get `202`, so that my Adapter doesn't retry or log errors.
29. As a user, I want an event whose `cwd` resolves to no allowed Project stored without a `project_id`, so that Agent Compute isn't lost but nothing is attributed to an excluded folder.

**Hardened Git**

30. As a user, I want the daemon to never execute a repo-local `core.fsmonitor` program, so that cloning a hostile repo can't run code through Traction.
31. As a user, I want the daemon to never run repo-local hooks, so that `core.hooksPath` or `.git/hooks` can't be abused.
32. As a user, I want the daemon to never create or hold `.git/index.lock`, so that my own `git commit` never fails with "another git process seems to be running".
33. As a user, I want every daemon `git` call killed after a 5-second hard timeout, so that a huge or hung repo can't stall the daemon.
34. As a user, I want the daemon's Git calls to refuse implicitly discovered bare repositories (`safe.bareRepository=explicit`), so that a bare repo planted inside a cloned project can't bring its own config and hooks into play.
35. As a Windows user, I want the hooks path disabled with `NUL` rather than `/dev/null`, so that Git for Windows never creates or uses a literal `dev/null` path.
36. As a maintainer, I want the daemon's Git usage limited to a fixed list of typed queries (common dir, top-level, root commits, origin URL), so that no code path can pass arbitrary arguments to `git`.
37. As a user without Git installed, I want the daemon to keep running and route Git-looking folders as `dir:` Projects, so that a missing `git` binary never crashes tracking.
38. As a user, I want Git output size-capped and never logged, so that a hostile repo can't flood memory or logs.

**File watcher and `FILE_MUTATION`**

39. As a user coding by hand with no Harness open, I want my file edits in a known Project recorded as `FILE_MUTATION` events, so that manual work earns credit.
40. As a user, I want `FILE_MUTATION` events to carry only metadata (Project, time, mutation kind), never file contents, names of secrets or diffs, so that no raw content is stored.
41. As a user, I want files matched by `.gitignore` or `.tractionignore` never to produce `FILE_MUTATION` events, so that build output and private files don't count or leak.
42. As a user, I want `node_modules`, `target`, `.git`, `.venv` and similar folders never watched, so that `npm install` or `cargo build` doesn't count as work or exhaust watcher limits.
43. As a user, I want a burst of saves to one file coalesced into one `FILE_MUTATION`, so that editor autosave doesn't inflate events.
44. As a user, I want watching to start as soon as a new Project is first routed, so that coverage is always on (ADR 0001).
45. As a user, I want the daemon to re-attach watchers to every Project stored in SQLite on startup, so that hand edits keep counting after a reboot even if no Harness has run since.
46. As a test author or future CLI author, I want to register a Project path directly over the authenticated bus, so that a folder can be watched without first sending a Harness event.
47. As a user, I want a Project folder that is deleted or moved to stop being watched without errors, so that the daemon stays healthy.
48. As a Linux user, I want the watcher to degrade gracefully when the inotify watch limit is hit, logging once instead of panicking, so that the daemon keeps running.
49. As a user on low-spec hardware, I want the watcher to add no polling loops at idle, so that the CPU budget (<1%) holds.

**Attribution**

50. As a user, I want an edit made while a Harness tool call is open (hook window `TOOL_PRE`→`TOOL_POST`/`TOOL_FAIL`, inside an open turn) attributed to that Harness and session, so that AI edits aren't counted as mine.
51. As a user, I want an edit made inside an active log-tailed turn attributed to that Harness, so that Codex and the fallback tailers are handled even though their events arrive late and lack exact tool windows.
52. As a user, I want an edit made within 30 seconds after the last event of a log-tailed source with no turn boundaries attributed to that Harness, so that sources that report no turns still claim their own edits.
53. As a user, I want each edit held for 5 seconds before attribution is decided, so that a log-tailed event written slightly after the edit can still claim it.
54. As a user, I want an edit outside every window recorded as manual work, so that my own hand edits always count as mine.
55. As a user, I want a Harness window in Project A never to claim an edit in Project B, so that attribution is scoped to the Project.
56. As a user, I want edits that happened while the daemon was stopped never recorded or attributed afterwards, so that attribution is live-only and can't be gamed by replay.
57. As a reviewer, I want one named constant for the trailing fallback window and one for the hold, both documented in `protocol.md`, so that I know exactly how log-tailed attribution works.
58. As a user, I want precedence fixed (hook window, then log-tailed turn, then trailing fallback window, and the most recently opened among equals), so that attribution is deterministic.

**Schema room**

59. As a future browser/editor Adapter author, I want `cwd` optional on the wire, so that a browser tab with no folder can still send events without a schema bump.
60. As a maintainer, I want every event to carry an `attribution_source` from a closed enum (`harness_hook`, `harness_plugin`, `harness_log`, `harness_db_poll`, `file_watcher`, reserved `browser_extension`, `editor_extension`, `manual_pick`), so that each event's origin is explicit and typos are rejected.
61. As a user, I want the bus to reject any external event claiming `attribution_source: file_watcher`, so that no local process can forge "manual work" mutations.
62. As a maintainer, I want the reserved `browser_extension`, `editor_extension` and `manual_pick` values accepted by the schema but refused by the v1 bus, so that turning them on later is a daemon policy change, not a schema migration.
63. As a user, I want manual `FILE_MUTATION` events stored with no Harness and no session and `network_class: manual`, so that manual work is never mistaken for a Harness's work.
64. As a maintainer, I want `schema_version` to stay `1`, so that 1a's Adapters, fixtures and tests keep working unchanged apart from the new required field.
65. As a reviewer of the code, I want no `todo!()`, stubbed Git or hardcoded routing results, so that the anti-mocking rule holds.

## Implementation Decisions

**`traction-core` schema changes (still `schema_version: 1`)**

- `cwd` becomes optional. When present it keeps 1a's rules: absolute, bounded, no control characters.
- `project_id` becomes optional on the wire. When present it must still match `git:<64 hex>` or `dir:<64 hex>`. The daemon's Router is authoritative: it replaces any Adapter-supplied value with its own result. Adapters should omit it.
- New required field `attribution_source`, a closed enum: `harness_hook`, `harness_plugin`, `harness_log`, `harness_db_poll`, `file_watcher`, `browser_extension`, `editor_extension`, `manual_pick`. Core accepts all of them. Bus policy (below) restricts them. `manual_pick` is reserved for a later user-driven attribution (for example, the user picking which Project or source a span belongs to). Its semantics are defined when it is turned on.
- `harness_id` and `session_id` become optional at the type level. Validation rules:
  - Every event type except `FILE_MUTATION` requires both.
  - A `FILE_MUTATION` has both (attributed to a Harness session) or neither (manual work).
  - A manual `FILE_MUTATION` has `network_class: manual`.
  - `attribution_source: file_watcher` is valid only on `FILE_MUTATION`. A `FILE_MUTATION` must have `attribution_source: file_watcher`.
- New optional `metrics_only.mutation_kind`, a closed enum (`created`, `modified`, `removed`, `renamed`). It is valid only on `FILE_MUTATION`, where it is required.
- `FILE_MUTATION` does not carry the mutated file's path or name. Its `cwd` is the Project Root.
- Pure Project ID derivation moves into core so the format and hashing live in one place (SHA-256; this also lays groundwork for the Step 3 hash-chain):
  - `git:` = SHA-256 over the root commit hex and the normalized origin URL (empty when no origin), with a fixed separator.
  - `dir:` = SHA-256 over the device secret and the canonical Project Root path, with a fixed separator.
  - Core takes these inputs as values. All file system and Git work stays in the daemon.
- Origin normalization: lowercase the host; strip the scheme, userinfo, port and a trailing `.git` or `/`; rewrite scp-style `git@host:owner/repo` to `host/owner/repo`. Golden tests pin the normalization.
- Validation errors stay value-free. Add reason codes (for example `missing_harness`, `invalid_attribution_source`, `missing_mutation_kind`, `unexpected_mutation_kind`).
- `docs/protocol.md` sections 2 and 2a are updated in the same change. They document the optional fields, `attribution_source`, `mutation_kind`, the derivation formulas, ID pinning and the fallback window constant.

**Local Ingestion Bus policy changes**

- After validation, the bus rejects with `400` and a distinct reason code:
  - `attribution_source: file_watcher` (daemon-internal only);
  - `browser_extension`, `editor_extension` and `manual_pick` (reserved in v1).
- The bus rejects any externally sent `FILE_MUTATION` in v1, because only the daemon's own watcher produces them.
- **Project registration endpoint.** `POST /v1/project` takes a JSON body with one absolute `path` field (unknown fields rejected, same size cap). It goes through the same `Host`, `Origin` and token checks as `/v1/event`, in the same order, on both transports. The daemon runs the path through the Router, so registering a subfolder of a repo registers the repo's Project Root.
  - It returns `200` with the resolved `project_id` when a Project was registered (or already existed), then starts watching it.
  - It returns `422` with a reason code when the path is excluded, under a user exclusion, missing or not a folder.

  This endpoint serves tests now and the 1e CLI later (`traction track <path>`). It is not automatic discovery of hand-coded folders, which stays in 2a.
- Accepted events pass through the Router before reaching the writer. The Router fills or replaces `project_id`, or drops the event when it falls under a user exclusion (still `202`, nothing stored).
- Routing must not block the bus response. The bus acknowledges once the event is validated and queued. Git queries run on a small blocking pool or the Router's own thread, and their results are cached.

**Project Router (daemon)**

- **Input:** an event's `cwd`, or the session's last known `cwd` when `cwd` is absent, plus the session's start folder. Every Harness session's start folder is recorded from its `SESSION_START` (or its first event), and its current `cwd` is updated on `CWD_CHANGE`. Both are persisted, so restarts keep the fallback.
- **Resolution order:**
  1. **Git root.** Find the nearest ancestor (lexically, by checking for a `.git` entry) that holds a `.git` folder or file. Then ask the hardened wrapper for `--git-common-dir` and `--show-toplevel`. If the common dir is a normal `.git` folder, the Project Root is the common dir's parent (the main worktree), which collapses linked worktrees. If the repo is bare (reached through a linked worktree's `.git` file, which `safe.bareRepository=explicit` still allows), the Project Root is the bare repo folder. A `cwd` inside a bare repo's own folder is refused by Git under `safe.bareRepository=explicit` and falls through to the next step. A repo whose root is exactly `$HOME` is skipped, and resolution continues as if no Git repo were there.
  2. **Nearest marker.** The nearest ancestor containing a project marker from a closed list: `package.json`, `Cargo.toml`, `pyproject.toml`, `go.mod`, `*.sln`, `.traction-project`, `deno.json`, `pom.xml`, `build.gradle`, `Gemfile`, `composer.json`, `CMakeLists.txt`, `Makefile`. The list is one constant.
  3. **Session start folder.**
  4. **None.** The event is stored with no `project_id`.
- **Exclusions** apply at every step. A candidate is skipped (resolution keeps walking up) when it:
  - is `$HOME` itself or a filesystem root;
  - is under an OS folder (per-OS list);
  - is under a cache or toolchain folder (per-OS list, including OS temp folders); or
  - has a path component (below `$HOME`, or below the filesystem root when outside `$HOME`) in the dependency list `node_modules`, `target`, `.git`, `.venv`, `venv`, `__pycache__`, `.tox`, `.gradle`, `.next`, `dist-newstyle`.

  Descendants of `$HOME` are never excluded by the OS-folder list. This keeps macOS homes and test homes under `/var/folders` or `/private` working. Walking never goes above `$HOME` for markers.
- **User exclusions.** A global ignore file in the data folder lists folders, given as absolute paths or `~/` paths. Events whose resolved `cwd` lies inside a listed folder are dropped, and those folders are never watched. Per-Project `.tractionignore` files (gitignore syntax, at any level inside the Project) affect only what the watcher reports.
- **Canonical paths.** Resolve symlinks (OS canonicalize). On Windows, strip the `\\?\` prefix and normalize the drive letter to uppercase. Compare case-insensitively on Windows and on case-insensitive macOS volumes, by normalizing through the canonical form the OS returns.
- **Caching.** Cache each folder → Project Root resolution with a short TTL (about 30 s), so a burst of events causes at most one set of Git calls. A `.git` entry appearing at a known `dir:` Project Root invalidates the cache entry immediately.
- **Project registry** (new SQLite tables, written only by the single writer):
  - `project`: `project_id`, `kind` (`git`/`dir`), `root_path`, `home_relative_path`, `name` (folder basename), `first_seen_ms`. The `os_username` and the rest of the protocol 2a metadata are synced in Step 4, so only what is needed locally is stored here.
  - `project_alias`: old ID → current ID.
  - `session`: `session_id`, `harness_id`, `start_folder`, `last_cwd`.
- **ID pinning.** The first time a Git Project Root is resolved with a commit, its `git:` ID is stored against the repo's canonical common dir. Later resolutions reuse it, even if `origin` changes. A different device may compute a different ID when the remote was added at a different time. The website merge (Step 4b) resolves that case, as ADR 0001 already anticipates.
- **Migration (alias-only; stored events are never rewritten).** When a resolution finds that a registered `dir:` Project Root now resolves as a Git repo with at least one commit, the Router asks the writer for a migration. In one transaction the writer:
  1. registers the `git:` Project;
  2. records `project_alias` (`alias_id` = old `dir:` ID → `project_id` = new `git:` ID, `migrated_at_ms`); and
  3. marks the `dir:` Project as merged into the `git:` one (its watcher moves to the `git:` Project).

  It does **not** touch any stored Traction Event. Events already stored keep the `dir:` ID they were written with. New events, including late ones carrying the old `dir:` ID, are stored under the `git:` ID, because the Router resolves IDs through the alias before insert.

  The migration is idempotent, so replaying it after a crash is a no-op. Alias chains (a `dir:` ID aliased to a `git:` ID that is later aliased again) are resolved to the final ID. Cycles are refused.
- **Alias-aware queries.** Every daemon read of "a Project's events" selects events whose `project_id` is the Project's ID or any `alias_id` that resolves to it. This goes through one query helper (or one SQLite view joining `traction_event` to `project_alias`) so no caller filters on raw `project_id`. The Step 2 Work Bucket aggregator and Step 3 hash-chain build on this helper.
- **Device secret.** 32 bytes from the OS CSPRNG, stored in the data folder. On Unix the file is created atomically with mode `0600`; on Windows it relies on the user-profile ACL, the same pattern as the bus token. It is never logged and never sent anywhere.

**Hardened Git wrapper (daemon, one module, the only place `std::process::Command` runs `git`)**

- The public surface is a closed set of typed queries:
  - `CommonDir` (`rev-parse --path-format=absolute --git-common-dir`);
  - `TopLevel` (`rev-parse --show-toplevel`);
  - `RootCommits` (`rev-list --max-parents=0 HEAD`, the lexicographically smallest is chosen);
  - `OriginUrl` (`config --get remote.origin.url`).

  No caller can pass free-form arguments.
- Every invocation:
  - runs `git --no-pager --no-optional-locks -c core.fsmonitor=false -c safe.bareRepository=explicit -c core.hooksPath=<null device>`, with `-C <folder>`;
  - uses `/dev/null` as the null device on Unix and `NUL` on Windows, chosen at compile time (`cfg(windows)`) as one constant, so Git for Windows never resolves a literal `dev/null` path;
  - clears inherited `GIT_*` environment variables;
  - sets `GIT_TERMINAL_PROMPT=0` and `GIT_OPTIONAL_LOCKS=0`;
  - closes stdin;
  - caps stdout at a small byte limit and discards stderr;
  - is killed (and reaped) after a **5-second hard timeout** (one public constant), which counts as "not a Git repo" for that resolution and is logged without output.
- Before any call, the wrapper refuses a repo whose top-level is exactly `$HOME`. The check is done lexically on the discovered `.git` parent first, then confirmed from `TopLevel`.
- If the `git` binary is missing, every query returns "unavailable", the Router falls back to markers and session folder, and this is logged once.
- No subcommand that touches the index, worktree, refs or network is reachable. A unit-level allowlist test asserts the exact argument vectors.

**File watcher (daemon)**

- Watches every non-merged, non-excluded Project in the registry.
  - **On startup**, after the database opens, the daemon reads the `project` table and re-attaches a watcher to every such Project whose root still exists. Missing roots are skipped and logged once, but stay registered, so a re-mounted drive resumes on the next start.
  - **New Projects** are watched as soon as they are registered, whether by a routed Harness event or by the registration endpoint.
  - Re-attaching starts fresh. Nothing that changed while the daemon was down is reported (live-only rule).
- Uses the `notify` crate's OS backends with no polling fallback:
  - **macOS (FSEvents) and Windows (ReadDirectoryChangesW):** recursive watches on the Project Root, with filtering in-process.
  - **Linux (inotify):** non-recursive watches per folder, added by a walk that prunes excluded and ignored folders, and extended when new folders appear. Hitting the inotify limit stops adding watches for that Project, logs once, and never panics.
- **Filtering:** dependency folders, `.git`, `.gitignore` and `.tractionignore` matches (gitignore semantics, via the `ignore` crate's matcher only), and user-excluded folders. Only file events count; folder-only events and metadata-only events (atime, permissions) are ignored.
- **Coalescing and hold:** each observed mutation is held for `LOG_TAIL_HOLD` (5 s), keyed by Project and file, so that late log-tailed events can still claim it. Repeated events for the same file within the hold collapse into one. The `mutation_kind` follows three rules: a file first seen `created` stays `created` (later modifications keep it `created`); `created` followed by `removed` drops the mutation; otherwise it is the last kind seen (so `removed` then `created` becomes `modified`). At decide time the file's existence wins: a missing file becomes `removed` (or is dropped if it was first `created`). Memory is bounded: once a per-Project cap of pending files is reached, the oldest pending mutations are decided early.
- **Emission:** after the hold, the watcher builds a `FILE_MUTATION` Traction Event and sends it to the writer through the same validated path (not over HTTP). The event has a daemon-minted UUIDv7 `event_id`, the timestamp of the first observation, `attribution_source: file_watcher`, `cwd` = Project Root, the router's `project_id`, `mutation_kind` and the attribution result. On graceful shutdown, pending mutations are decided with the windows known at that moment and flushed.
- **Live-only:** there is no startup scan, no mtime comparison and no replay. Only notifications received while the daemon runs produce events.

**Attribution windows (daemon, in memory, fed by accepted events)**

- A **hook window** opens on a `TOOL_PRE` with `attribution_source` `harness_hook` or `harness_plugin`, for a session whose turn is open (a `PROMPT_SUBMIT` with no later `TURN_END`, `STOP_FAILURE` or `SESSION_END`). It closes on the matching `TOOL_POST`/`TOOL_FAIL` for that session, or on turn end. Matching is by session: one open tool window per session, and a new `TOOL_PRE` replaces the old one.
- **Log-tailed sessions** are sessions whose events come from `harness_log` or `harness_db_poll`. They get one of two windows. Both are compared on event timestamps, not arrival time, because the hold lets late records arrive.
  - A **log-tailed turn window**, if the session has ever reported a turn boundary (`PROMPT_SUBMIT` or `TURN_END`). It covers a mutation whose timestamp falls between a `PROMPT_SUBMIT` and its closing `TURN_END`, `STOP_FAILURE` or `SESSION_END`, or after a `PROMPT_SUBMIT` that is still open. It is wider than a hook window on purpose: log-tailed sources don't give exact tool timing.
  - A **trailing fallback window**, if the session has never reported a turn boundary. It covers a mutation that falls within `LOG_TAIL_TRAILING_WINDOW` (one constant, 30 s) after that session's most recent event of any type.
- **Hold versus windows.** `LOG_TAIL_HOLD` (5 s) is a delivery-lag buffer, not an attribution window: it only delays the decision so records that arrive late can be counted. `LOG_TAIL_TRAILING_WINDOW` is the single "short fallback window" constant that AGENTS.md refers to. Both live in one place and are documented in `protocol.md`.
- Windows are scoped to the session's current Project. A mutation is attributed only to windows on its own Project.
- Precedence: hook window, then log-tailed turn window, then trailing fallback window. Among windows of the same kind, the most recently opened wins.
- An attributed mutation copies the winning session's `harness_id`, `session_id` and `network_class`. A mutation outside every window is manual work: no Harness, no session, `network_class: manual`.
- Window state is never persisted. A daemon restart starts with no open windows.

**SQLite**

- Migration 2 (appended; migration 1 is never edited):
  - rebuilds `traction_event` with nullable `harness_id`, `session_id`, `cwd` and `project_id`, a new non-null `attribution_source` column and a nullable `mutation_kind` column;
  - backfills existing rows' `attribution_source` as `harness_hook` (only development data exists);
  - creates `project`, `project_alias` and `session`;
  - adds an index on `project_id` and on `project_alias.alias_id`;
  - adds `BEFORE UPDATE` and `BEFORE DELETE` triggers on `traction_event` that abort. This makes the table append-only at the database level, so no code path (including a future bug) can rewrite history.

  The table rebuild in this same migration happens before the triggers exist. It is the last time stored events are copied, and it is a schema change, not a history change.
- Registry, alias, session and migration writes all go through the single writer thread. The Router reads the registry only through its own in-memory cache, which the writer updates after commit.

**Daemon config additions**

- `home_dir` (default: the OS home) and the global ignore file location, derived from the data folder. OS-folder and cache lists derive from the OS and `home_dir`. Tests inject a temp `home_dir`, so fixtures live under a fake home, never the developer's real one.
- The watcher's hold uses `LOG_TAIL_HOLD` and the trailing window uses `LOG_TAIL_TRAILING_WINDOW` in production. Test-only config overrides are allowed for speed, but each production value is a single public constant.

## Testing Decisions

**Seams (approved by the user).** These reuse 1a's two seams. No new seam is added. The registration endpoint is part of the bus, so it sits on Seam 2.

**Seam 1: `traction-core` public API.**

- Schema room:
  - an event with no `cwd` validates;
  - `attribution_source` round-trips for every value, and an unknown spelling is rejected;
  - manual `FILE_MUTATION` (no Harness, no session, `manual`) validates;
  - a `FILE_MUTATION` with a Harness but no session, a non-`FILE_MUTATION` with no Harness, `mutation_kind` on a non-`FILE_MUTATION`, a `FILE_MUTATION` without `mutation_kind`, and `file_watcher` on a non-`FILE_MUTATION` are each rejected with their reason code.
- Canonical JSON golden bytes are updated for the new field. The 1a example event plus `attribution_source` gives exact bytes.
- Project ID derivation:
  - golden IDs for fixed inputs (root commit, origin, secret, path);
  - origin-normalization goldens (`https://github.com/A/B.git`, `git@github.com:A/B`, `ssh://git@github.com:22/A/B/` all give the same ID);
  - different secrets or paths give different `dir:` IDs.

**Seam 2: a real in-process daemon** on a temp data folder and a temp fake `home_dir`, using 1a's shared test helper. Tests act only through:

- bus requests;
- real `git` commands run by the test to build fixtures (the test's own Git calls are not hardened, since it builds the hostile repo itself);
- real file edits in fixture folders;
- a read-only SQLite connection, polled with a timeout while the daemon runs (WAL allows concurrent readers) or opened after shutdown.

There are no mocks of Git, the file system or the watcher.

- **Routing gate (owns the Exit Gate item):**
  - build 3 Git repos with commits (one with `origin`), add a `git worktree` of one, and create 2 non-Git folders (one with `package.json`, one empty, used as a session start folder);
  - send interleaved events with `cwd`s in subfolders of each, including the worktree and a `CWD_CHANGE` into another Project;
  - assert that every row has the expected `project_id`, that the worktree rows share the main repo's ID, and that exactly 5 Projects are registered.
- **No `index.lock`:**
  - during the routing gate, a test thread polls every fixture repo's `.git/index.lock` (and the worktree's gitdir) and asserts it never appears;
  - each repo's `.git/index` mtime and bytes are unchanged afterwards.
- **Exclusions:**
  - events with `cwd` = fake `$HOME`, a folder in the OS list, and a `node_modules/<pkg>` (with its own `package.json`) inside a Project;
  - assert the first two are stored with no `project_id`, the third routes to the enclosing Project, and none of these folders is registered;
  - a dotfile repo initialised at the fake `$HOME` does not capture an unmarked subfolder;
  - a folder in the global ignore file gets `202` and stores nothing.
- **Migration:**
  - route events to an empty folder (a `dir:` ID), then `git init`, commit, and send another event;
  - assert that pre-migration rows still carry their original `dir:` ID, byte-for-byte unchanged (compare full rows captured before migration);
  - assert that an alias row links the `dir:` ID to the `git:` ID, and that post-migration rows (including a late event carrying the old `dir:` ID) are stored under the `git:` ID;
  - assert that the alias-aware Project view returns pre- and post-migration events together under the `git:` ID;
  - assert that all of this survives a restart.
- **Append-only events:** an `UPDATE` or `DELETE` on `traction_event` through a separate read-write connection, opened after shutdown, is aborted by the triggers.
- **ID pinning:** adding `origin` to a repo after first routing doesn't change its ID.
- **Red team (owns the AGENTS.md requirement):**
  - a fixture repo whose `.git/config` sets `core.fsmonitor` to a script that writes a sentinel file;
  - it also sets `core.hooksPath` to a folder of sentinel-writing hooks, and `.git/hooks/*` scripts that do the same;
  - route events into it, trigger migration checks and run the watcher over it;
  - assert no sentinel file ever exists, on all 3 OSes. On Windows the script is a `.bat`/`.cmd`, chosen per OS;
  - also assert that no `dev/null` file or folder is created anywhere in the fixture tree or the test's working folder (this matters on Windows);
  - a bare repo with a hostile config, planted inside a Project subfolder, is not treated as a Git repo when `cwd` is inside it, and its sentinel never appears.
- **Git timeout and missing Git:** with `PATH` pointing to no `git` (configured through the daemon's environment in the test), a repo folder still routes as a `dir:` Project and the daemon stays healthy. A timeout case is covered by the wrapper's own test (below).
- **Hand edits (owns the Exit Gate item):**
  - register a Project via the registration endpoint (no Harness event ever sent), then create, modify and remove files in the Project;
  - assert matching `FILE_MUTATION` rows with no Harness, `network_class: manual`, `attribution_source: file_watcher` and the right `mutation_kind`;
  - edits to `.gitignore`d, `.tractionignore`d, `node_modules` and `.git` paths produce no rows;
  - a burst of saves to one file produces one row.
- **Watcher persistence:**
  - register 2 Projects (one by a routed event, one by the endpoint) and restart the daemon with no new events;
  - hand edits in both produce `FILE_MUTATION` rows, which proves the watchers re-attached from SQLite;
  - a registered Project whose folder was deleted while the daemon was down doesn't stop startup;
  - edits made during the downtime produce no rows.
- **Registration endpoint:**
  - a subfolder of a repo registers the repo's Project Root;
  - `$HOME`, an OS folder, a `node_modules` folder, a user-excluded folder and a missing path each return `422`;
  - a bad token, `Host` or `Origin` gets the same `401`/`403` as `/v1/event`.
- **Attribution:**
  - hook window: `PROMPT_SUBMIT` then `TOOL_PRE` (`harness_hook`), edit, then `TOOL_POST`; the row is attributed to that session;
  - an edit after `TOOL_POST`, or after `TURN_END`, is manual;
  - log-tailed turn: a `harness_log` `PROMPT_SUBMIT`, an edit, then `TURN_END` gives an attributed row, and an edit after `TURN_END` is manual;
  - late arrival: a `harness_log` `PROMPT_SUBMIT` whose timestamp precedes the edit but which is delivered up to `LOG_TAIL_HOLD` after the edit still claims it, and one delivered after the hold does not;
  - trailing fallback: for a `harness_db_poll` session that never sends turn boundaries, an edit 10 s after its last event is attributed, and one more than `LOG_TAIL_TRAILING_WINDOW` after is manual (shortened via the test override, with the ratio kept);
  - a window in Project A doesn't claim an edit in Project B;
  - precedence: hook window beats log-tailed turn, which beats trailing fallback;
  - live-only: edit while the daemon is stopped, restart, and assert zero rows for that edit.
- **Bus policy:** externally sent `file_watcher`, `browser_extension`, `editor_extension`, `manual_pick` and any `FILE_MUTATION` get `400` with their reason codes. An event with no `project_id` and a valid `cwd` gets `202` and is routed. An Adapter-supplied wrong `project_id` is replaced.
- **No raw text:** 1a's sentinel scan is extended. A file whose contents and name contain a sentinel string is edited, and no table, column, database file or WAL file contains the sentinel.

**Narrow internal test, allowed by exception:** the hardened Git wrapper has unit tests inside the daemon crate. They assert:
- the exact argument vector and environment for each typed query, including `safe.bareRepository=explicit` and the per-OS hooks path (`/dev/null` on Unix, `NUL` on Windows);
- that a child process hanging past the 5-second timeout is killed and reaped. The wrapper is the security boundary, and Seam 2 cannot observe argv. These tests use the real `git` binary and a real sleeping child, with no mocks.

**What makes a good test here:** it asserts observable behaviour (rows, registered Projects, sentinel files that never appear, status codes) and survives a rewrite of the Router internals. Fixtures are built fresh in temp folders with real `git`. Tests are hermetic and parallel-safe, run on all 3 OSes in CI, and never touch the real home folder or `~/.traction`.

**Prior art:** 1a's `traction-core` tests (golden canonical bytes, rejection tables) and the daemon integration tests with their shared `TestDaemon` helper (start/restart on a temp data folder, raw HTTP caller, read-only SQLite inspection). Extend that helper with a fake `home_dir`, a fixture builder for Git repos and folders, and a poll-until helper for asynchronous watcher rows.

## Out of Scope

- Every Harness Adapter (1c, 1d). 1b tests send hand-built events over the bus, with each `attribution_source` value standing in for the real Adapters.
- Pause, the tray, hotkey, `traction pause`/`resume` and auto-start (1e). The 1e RSS/CPU measurement must include the watcher. 1b only avoids idle polling.
- Automatic discovery of folders that only ever get hand-coded, by mapping the focused IDE window to a Project (2a). In 1b, a folder becomes known when a Harness event routes to it or when it is registered through the bus endpoint. Known Projects are re-watched on every startup.
- The `traction track <path>` CLI command (1e, alongside `traction pause`/`resume`). It will be a thin client of the 1b registration endpoint.
- Semantics of the reserved `manual_pick` source.
- Struggle signals, diff line counting, the repo symbol index and shadow diff churn digests (Step 2). `FILE_MUTATION` carries no `diff_lines_delta` in 1b.
- Hash-chain, Anchors and server sync of Project metadata, including `os_username` (Steps 3 and 4).
- Merging Projects across devices or renames, and Linked Repos (Step 4b and the website).
- Opening the reserved browser/editor `attribution_source` values, and the Open Ingestion Spec (6d).
- `traction doctor`, `traction uninstall` and installer `--dry-run` (later polish per the roadmap).

## Further Notes

- **1c dependency:** the hook window needs a `TOOL_PRE`. The Claude Code hook list in `docs/steps.md` (Build item 4) has no `PreToolUse`. 1c must add `PreToolUse`, with the same `timeout: 1` and `onFailure: continue` rules. Without it, Claude Code edits get only fallback-window attribution. Record this in the 1c spec.
- **Step 3 compatibility:** stored Traction Events are append-only (enforced by triggers), and migration is alias-only. Events hashed under a `dir:` ID keep verifying after the Project becomes a `git:` Project. Step 3a still has to decide how a merged Project's chain continues: the `git:` Project could start its own chain, or could carry the `dir:` chain tip forward as its `H_0`. That decision is now additive, with no history to rewrite.
- **Protocol update:** `docs/protocol.md` currently shows `cwd` and `project_id` as required, with no `attribution_source` or `mutation_kind`. Update it in the first core ticket so docs and type never disagree. Also document `LOG_TAIL_HOLD` = 5 s, `LOG_TAIL_TRAILING_WINDOW` = 30 s, the Git timeout of 5 s, alias-only migration, the `git:` ID pinning rule and the `/v1/project` endpoint.
- **AGENTS.md wording:** the Git hardening invariant lists `core.hooksPath=/dev/null` and doesn't mention `safe.bareRepository=explicit`. Update it in the Git wrapper ticket to name the per-OS null device and the bare-repo flag, so the invariant matches the code.
- **Glossary:** consider adding **Attribution Window** (the hook or fallback window that attributes a `FILE_MUTATION` to a Harness) and **Manual Work** to `GLOSSARY.md` via `/domain-modeling`. Use Project, Project Root, Harness, Adapter and Traction Event in code. Avoid "repo" for Project and "workspace".
- **Device secret** is a second secret file next to the bus token. Keep the two separate, because rotating the token must never change every `dir:` ID.
- **New dependencies:** `notify` for the watcher, `ignore` (matcher only, or `globset` if it is lighter) for gitignore semantics, and `sha2` in core. Re-check RSS in 1e. If the watcher pushes RSS over budget, that is a 1e finding, not a reason to drop always-on watching.
- **1d follow-up (unbounded log-tailed turn):** a log-tailed turn window opened by a `PROMPT_SUBMIT` stays open until that session's next turn end or prompt. If the Adapter never sees the turn end (a crashed Harness, a truncated log), the window stays open and claims every later edit in that Project for the session. 1b keeps this behaviour; 1d should cap open log-tailed turns, for example by expiring one after a documented inactivity period with no event from its session.
- Suggested ticket order:
  1. Core schema room (`attribution_source`, optional fields, `mutation_kind`, validation, golden bytes) plus the `protocol.md` update.
  2. Core Project ID derivation and origin normalization.
  3. SQLite migration 2 (append-only triggers), the device secret and the bus policy for reserved/internal sources.
  4. Hardened Git wrapper, with its argv/timeout tests and the red-team fixture.
  5. Router: resolution order, exclusions, user ignore file, session start folder and the `/v1/project` registration endpoint (owns the routing gate).
  6. Alias-only `dir:` → `git:` migration, alias-aware queries and ID pinning.
  7. File watcher with ignore rules, coalescing, startup re-attach from SQLite and live-only emission (owns the hand-edit gate).
  8. Attribution windows (hook, fallback, precedence, Project scoping).
  9. Gate tests confirmed green on all 3 OSes in CI, then `/code-review`.
