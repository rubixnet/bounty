# Traction

Traction measures the hours of productive struggle a person spends building projects with AI coding tools, passively and verifiably.

## Activity & Time

**Productive Struggle**:
Time in which the human is actively steering through uncertainty, testing hypotheses against reality, and adapting to feedback.
_Avoid_: Coding time, active time, effort

**Human Active Time**:
Time in which real human hardware input is directed at project work (editing, reviewing diffs, testing, reading docs, composing or approving prompts).
_Avoid_: Active time, coding time

**Agent Compute Time**:
Time in which a Harness is working on a turn (inference or tool execution) between a prompt being submitted and the turn ending.
_Avoid_: AI time, agent time, AI Leverage (that is the profile label, not the concept)

**Unverified Autonomous Time**:
Agent Compute Time that ran for more than 90 seconds with no verified human presence; it counts as zero productive struggle and is kept only for audit.
_Avoid_: Idle time, idle autonomous, AFK time

**Work Bucket**:
One 60-second slice of a Project's timeline holding the three times and the struggle signals for that minute.
_Avoid_: Heartbeat, interval, slot

## Tracking Sources

**Harness**:
An AI coding tool that runs a human-prompted agent loop (e.g. Claude Code, OpenCode, Codex CLI, Codex Desktop).
_Avoid_: Client, agent, IDE, editor

**Adapter**:
The Traction component that turns one Harness's hooks, plugin calls, or logs into Traction Events.
_Avoid_: Integration, plugin (unless it is literally a Harness plugin)

**Traction Event**:
A normalized, content-free record of one Harness lifecycle moment.
_Avoid_: Heartbeat, log line

**Project**:
The root folder of a piece of work the user builds, with an AI Harness or by hand, whether or not it is a Git repository.
_Avoid_: Repo, workspace, directory

**Project Root**:
The folder that identifies a Project: its Git repo root if it has one, otherwise the folder the work was created in.
_Avoid_: cwd, working directory

**Attribution Window**:
A span of event timestamps in which one Harness session claims the `FILE_MUTATION`s in its current Project: a hook window (tool call inside an open turn), a log-tailed turn window, or the trailing fallback window after a log-tailed session's events.
_Avoid_: Fallback window (that is only one kind), grace period, hold

**Manual Work**:
A `FILE_MUTATION` in a Project that falls outside every Attribution Window, so it carries no Harness, no session and `network_class: manual`.
_Avoid_: Unattributed edit, human edit, hand-written code

**Linked Repo**:
The hosted GitHub repository a user has attached to a Project from the website.
_Avoid_: Remote, origin, connected repo

## Proof & Review

**Logged Hours**:
All time recorded locally on the user's device.
_Avoid_: Tracked hours, total hours

**Submitted Hours**:
Logged Hours the user has chosen to put forward for review inside an Audit Bundle.
_Avoid_: Claimed hours, approved hours

**Submission Eligibility**:
Whether a Project's hours may be submitted to a program; true only when it has a public Linked Repo.
_Avoid_: Qualification, approval

**Anchor**:
A chain tip sent to the server every two minutes, timestamped by the server on receipt.
_Avoid_: Heartbeat, checkpoint, sync

**Audit Bundle**:
The user-reviewed, optionally redacted export of a Project's buckets and snapshot frames that reviewers verify against Anchors.
_Avoid_: Export, archive, proof pack

**Redaction**:
Removing a snapshot frame's pixels and salt from an Audit Bundle while keeping its hash so the chain still verifies.
_Avoid_: Deletion, blur
