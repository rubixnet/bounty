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

**Presence Minute**:
One wall-clock minute of device-wide presence: counts, ratios and closed enums only (never a key, a pointer position, a window title or a URL), emitted by the Presence Sensor and later assigned to a Project.
_Avoid_: Heartbeat, input sample, activity log

**Activity Rollup**:
A 120-second, device-wide audit summary of Presence Minutes (category shares, switch rate, cadence entropy, injected ratio, tier), aligned to the 2-minute Anchor cadence and kept beside the 60-second Work Bucket.
_Avoid_: Audit bucket, 2-minute bucket

**Telemetry Tier**:
How much a platform backend could observe in a Presence Minute: `full`, `reduced` (idle + category), `idle_only` or `none`. A weaker tier weighs less as evidence for the user and never counts as evidence against them.
_Avoid_: Permission level, quality

**Ruleset**:
One versioned set of every scoring constant (`traction-rules/1`), recorded on each Work Bucket and Activity Rollup; changing any number means a new ID once buckets leave the device.
_Avoid_: Config, thresholds, settings

**Outcome Anchor**:
A moment that proves a real outcome nearby (Manual Work, a Harness edit the human's specific prompt asked for, a human commit, a closed fail-to-pass loop, an approved permission with real input); Human Active Time with no anchor in range earns no Credited Time.
_Avoid_: Checkpoint, proof point

**Credited Time**:
The part of Human Active Time that counts toward Productive Struggle (`struggle_credit_sec`): Human Active Time scaled by anchor, comfort and tier weights, never above Human Active Time itself.
_Avoid_: Approved time, paid time, score

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
A span of event timestamps in which one Harness session claims the `FILE_MUTATION`s in its current Project: a hook window (tool call inside an open turn), a log-tailed turn window, or the trailing fallback window after a log-tailed session's events. A closed hook or turn window reaches `WINDOW_CLOSE_GRACE` past its closing event (notification latency). An open hook or turn window goes stale once its session has produced no event for `STALE_TURN_TIMEOUT` (10 min) before the mutation, so a crashed Harness stops claiming edits. The watcher's `LOG_TAIL_HOLD` is not an Attribution Window: it is a delivery-lag buffer that only delays the decision so late events can still be counted.
_Avoid_: Fallback window (that is only one kind), grace period, hold (the hold is `LOG_TAIL_HOLD`, a delay, not a window)

**Manual Work**:
A `FILE_MUTATION` in a Project that falls outside every Attribution Window, so it carries no Harness, no session and `network_class: manual`.
_Avoid_: Unattributed edit, human edit, hand-written code

**Session Pause**:
A span during which one Harness session is not tracked, opened by the user typing exactly `/stop-track` in that session and closed by `/resume-track`. Its events are not stored but still feed the Attribution Windows, so a `FILE_MUTATION` it claims is dropped instead of counted as Manual Work. Per session, kept across daemon restarts.
_Avoid_: Stop tracking (ambiguous with stopping the daemon), mute, tray Pause (that one is device-wide, Step 1e)

**Linked Repo**:
The hosted GitHub repository a user has attached to a Project from the website.
_Avoid_: Remote, origin, connected repo

## Proof & Review

**Logged Hours**:
All time recorded on the user's device, and the live totals the server builds from the Work Buckets the device has synced. Logged Hours are never proof: they are synced but not verified, so a live total can be wrong or forged. Only a verified Audit Bundle (checked against the Anchors the server timestamped) is a verified stat. Live pages say "Logged"; they never say "verified".
_Avoid_: Tracked hours, total hours, verified hours (for a live total)

**Submitted Hours**:
Logged Hours the user has chosen to put forward for review inside an Audit Bundle.
_Avoid_: Claimed hours, approved hours

**Submission Eligibility**:
Whether a Project's hours may be submitted to a program; true only when it has a public Linked Repo.
_Avoid_: Qualification, approval

**Anchor**:
A chain tip sent to the server every two minutes, timestamped by the server on receipt. It carries a Chain Link's `entry_hash`.
_Avoid_: Heartbeat, checkpoint, sync

**Pairing**:
Enrolling a device with the server (`traction login`): the user approves it in a browser on an explicit, CSRF-protected page, the server issues the device token and pins the device's Device Signing Key. Re-pairing a Successor Chain adds the new key and keeps every earlier key trusted.
_Avoid_: Login (that is only the command), registration, linking (linking attaches a Linked Repo)

**Timeline Mark**:
An unsigned, content-free per-Project, per-minute count record (file mutations by origin, turns started, tool failures, the Harness IDs seen) synced beside the Work Buckets but outside the Chain Link. Always labelled `unsigned`: it helps a reviewer line up activity and is never proof.
_Avoid_: Event, heartbeat, activity log

**Triage**:
A `DecisionEngine`'s routing of a submitted Audit Bundle: Green (fast-track human confirmation), Yellow (fail-closed human review) or Red (anomaly pin). Never a ban and never an automatic approval.
_Avoid_: Verdict (that is the human reviewer's), score, ban

**Chain Link**:
The local, signed, hash-chained record of one sealed 120-second window: a header (sequence number, previous link's hash, key and window) and a payload of per-Project row digests, signed with the Device Signing Key. Not an Anchor (that is the tip sent to the server).
_Avoid_: Block, ledger entry, checkpoint

**Seal**:
To compute a finalized window's payload, link it to the previous Chain Link, sign it and commit it in the same transaction as the window's rows.
_Avoid_: Sign, anchor, commit (a commit is a Git commit)

**Sealed Window**:
A 120-second window that holds a Chain Link; its Work Buckets, Commit Observations and Activity Rollup can no longer change without verification failing.
_Avoid_: Closed window, finalized window (finalized only means computed, not yet sealed)

**Chain Tip**:
The highest Chain Link of a Device Signing Key's chain; its `entry_hash` is what an Anchor carries.
_Avoid_: Head, latest hash

**Device Signing Key**:
The per-device Ed25519 key in `signing.key` that signs every Chain Link and Audit Bundle manifest. Different from the device secret (`device.secret`, which keys `dir:` Project IDs).
_Avoid_: Device key, secret key, private key

**Successor Chain**:
A new chain, under a new Device Signing Key, started after `signing.key` was lost; its genesis link names the previous chain's tip. Verification always reports it as a key discontinuity (two chains), never as one unbroken chain.
_Avoid_: Fork, rotated chain, reset

**Audit Bundle**:
The export of one Project's buckets (and, from Step 3c, user-reviewed, optionally redacted snapshot frames) together with every Chain Link the device holds and a signed manifest, which reviewers verify offline and against Anchors. Frame-less until Step 3c.
_Avoid_: Export, archive, proof pack

**Redaction**:
Removing a snapshot frame's pixels and salt from an Audit Bundle while keeping its hash so the chain still verifies.
_Avoid_: Deletion, blur
