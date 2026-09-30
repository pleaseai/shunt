---
name: mirrored-validation-over-validates
description: shunt's config/router/validate.rs mirrors libsy's rules — a rule copied from a sibling form over-validates; check the pinned upstream source before keeping or removing one
metadata:
  type: project
---

`src/config/router/validate.rs` exists to restate *upstream libsy's* rules with
shunt key names (its module docs say so). Rules therefore drift by being copied
between forms: the standalone classifier's `message_hash_fallback` +
`new_session` pairing was copied into `validate_composite`, but upstream's
`CompositeRouter::new` refuses only `every_request` and supports the hash key
under `user_turn`.

**Why:** an over-validating rule here is a refusal of a config upstream accepts,
and the only visible symptom is a `shunt check` error nobody can explain.

**How to apply:** before fixing or defending a rule in this module, read the
pinned upstream at `~/.cargo/git/checkouts/switchyard-*/<rev>/crates/libsy/src/algorithms/`
(rev is in `Cargo.toml`). When removing a rule, invert its test into a positive
twin rather than deleting it — `Config::validate` runs `check_buildable`, which
constructs the real upstream algorithm, so "accepted by validation" is
simultaneously proof that upstream accepts it. See also
[[validate-and-resolve-must-normalize-alike]].
