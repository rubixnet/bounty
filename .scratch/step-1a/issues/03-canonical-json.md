# 03: Canonical JSON (traction-core)

**What to build:** A public canonical JSON function in `traction-core` that turns a Traction Event into the same bytes on every OS. The Step 3 hash-chain and the Step 4 server will hash exactly these bytes. The output has object keys sorted by UTF-8 bytes (recursively), no extra whitespace, integers without exponent or fraction, no floats, and `None` written as an explicit `null`. See the canonical JSON decision in `.scratch/step-1a/spec.md`.

**Blocked by:** 02

**Status:** resolved

- [x] Golden-bytes test: the `docs/protocol.md` example event produces an exact, checked-in canonical byte string.
- [x] Nested key order, explicit `null` for absent optional fields and the absence of whitespace are all asserted on bytes.
- [x] Building equal events with fields set in different orders gives identical bytes.
- [x] Serializing an event to canonical JSON and deserializing it back gives the original event, for every `event_type`.
- [x] Float values are rejected or cannot be represented.
- [x] No hashing is implemented (that is Step 3).
