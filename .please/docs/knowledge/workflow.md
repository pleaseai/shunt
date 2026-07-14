# Project Workflow

> Development workflow conventions for `shunt`.
> Configured during `/please:setup` (standard workflow). Referenced by `/please:implement`.

## Guiding Principles

1. **The Plan is the Source of Truth**: All work is tracked in the track's `plan.md`
2. **The Tech Stack is Deliberate**: Changes to the tech stack must be documented in `tech-stack.md` before implementation
3. **Test-Driven Development**: Write tests before implementing functionality
4. **High Code Coverage**: Aim for >80% code coverage for new code (matches the SonarCloud `new_coverage` gate)
5. **Non-Interactive & CI-Aware**: Prefer non-interactive commands. CI runs with `RUSTFLAGS=-D warnings`

## Task Workflow

All tasks follow a strict lifecycle within `/please:implement`:

### Standard Task Lifecycle

1. **Select Task**: Choose the next available task from `plan.md`
2. **Mark In Progress**: Update task status from `[ ]` to `[~]`
3. **Write Failing Tests (Red Phase)**:
   - Add unit tests (in-module `#[cfg(test)]`) or integration tests under `tests/`
   - Write tests defining expected behavior
   - Run tests and confirm they fail as expected
4. **Implement to Pass Tests (Green Phase)**:
   - Write minimum code to make failing tests pass
   - Run the test suite and confirm all tests pass
5. **Refactor (Optional)**:
   - Improve clarity, remove duplication, keep files under ~500 lines
   - Rerun tests to ensure they still pass
6. **Verify Coverage**: Target >80% for new code
7. **Document Deviations**: If implementation differs from the tech stack, update `tech-stack.md` first; update affected docs (`README.md` / `docs/` / `site/`) in the same change
8. **Commit**: Stage and commit with a conventional commit message (one commit per task)
9. **Update Progress**: Mark the task complete in `## Progress` with a timestamp

### Phase Completion Protocol

Executed when all tasks in a phase are complete:

1. **Verify Test Coverage**: Identify all files changed in the phase, ensure test coverage
2. **Run Full Test Suite**: `cargo test --all-features --workspace` (max 2 fix attempts)
3. **Manual Verification Plan**: Generate step-by-step verification instructions for the user
4. **User Confirmation**: Wait for explicit user approval before proceeding
5. **Create Checkpoint**: Commit `chore(checkpoint): complete phase {name}`
6. **Update Plan**: Mark the phase complete in `plan.md`

## Quality Gates

Before marking any task complete:

- [ ] All tests pass (`cargo test --all-features --workspace`)
- [ ] `cargo fmt --all --check` clean
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] Code coverage meets requirements (>80% for new code)
- [ ] Streaming semantics preserved; gateway errors in Anthropic shape
- [ ] No security vulnerabilities introduced; no secrets committed
- [ ] Affected docs (`README.md` / `docs/` / `site/`) updated in the same PR

## Development Commands

### Setup

```bash
cargo build                 # debug build
cargo build --release       # release build
```

### Daily Development

```bash
cargo run -- run            # run the gateway
cargo run -- check          # validate config
cargo run -- token          # token helper
./target/release/shunt run  # run the release binary
```

### Testing

```bash
cargo test --all-features --workspace
```

### Before Committing

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --workspace
```

## Testing Requirements

### Unit Testing

- Every module should have corresponding tests (in-module `#[cfg(test)]`)
- Mock external providers with `wiremock`
- Test both success and failure cases (including Anthropic-shaped error paths)

### Integration Testing

- Protocol + translation flows live in `tests/`
- Verify streaming (SSE) and non-streaming paths
- Test auth/credential and multi-account pooling behavior

## Commit Guidelines

Follow conventional commits. See `Skill("standards:commit-convention")` for details.

### Types

- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation only
- `style`: Formatting changes
- `refactor`: Code change without behavior change
- `test`: Adding or updating tests
- `chore`: Maintenance tasks

## Definition of Done

A task is complete when:

1. All code implemented to specification
2. Unit/integration tests written and passing
3. Code coverage meets project requirements (>80% new code)
4. `cargo fmt`, `cargo clippy -D warnings`, and `cargo test` all pass
5. Affected docs updated in the same change
6. Progress updated in `plan.md`
7. Changes committed with a proper conventional-commit message
