# Track every Project folder, Git or not, and sync its full path

Traction tracks any folder the user works in, through an AI Harness or by hand, as a Project, even when it isn't a Git repo. This matches Hackatime, where local-only and never-pushed work still shows up, and the user can link a GitHub repo later on the website. The original design tracked only Git repos and sent the server just an opaque repo ID. We rejected that: users want credit and motivation for every folder they build in, and reviewers want the real location. The server therefore receives the Project's absolute path, its home-relative path and the OS username. Owners and reviewers can see these; public profiles show only the Project name.

## Consequences

- Project identity needs a non-Git form (`dir:` ID). A folder that later gains its first commit migrates to the `git:` ID, and renamed or multi-device duplicates are merged on the site.
- The daemon must never create Projects for system, home-root, cache or dependency folders.
- Paths and usernames are personal data. They must never appear on public profiles, embeds or badges.
