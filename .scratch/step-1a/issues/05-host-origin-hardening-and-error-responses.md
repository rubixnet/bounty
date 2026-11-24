# 05: `Host`/`Origin` checks and error responses

**What to build:** Websites and other local callers can no longer reach the Local Ingestion Bus through DNS rebinding or browser CSRF. Every request goes through the final, fixed check order: `Host` → `Origin` → token → body size → validation → enqueue. Each kind of failure gets its own documented status code, and the response never repeats the input. This ticket owns the Step 1 Exit Gate item "Local ingestion bus rejects missing/wrong bearer tokens, foreign `Host` headers, and foreign `Origin` headers." See the Local Ingestion Bus decision in `.scratch/step-1a/spec.md`.

**Blocked by:** 04

**Status:** ready-for-agent

- [ ] The allowed `Host` values are `127.0.0.1:<port>` and `localhost:<port>`, where the port comes from the bound port (so production is exactly `41937`, and the port-0 tests still work).
- [ ] `Host: evil.example:<port>`, `Host: 127.0.0.1:9999` and a missing `Host` each return `403`, even when the token is valid.
- [ ] A request with no `Origin` header is allowed. The daemon's own origin (`http://127.0.0.1:<port>` or `http://localhost:<port>`) is allowed.
- [ ] `Origin: https://evil.example` and `Origin: null` each return `403`, even when the token is valid.
- [ ] Remaining status codes:
  - a body over the cap (about 16 KiB) returns `413`;
  - malformed JSON or a failed validation returns `400` with a short machine-readable reason code, and the response body never contains the input;
  - unknown paths and methods return `404`/`405` only after the `Host`, `Origin` and token checks pass.
- [ ] Every case above is covered by a black-box integration test using the shared daemon helper.
