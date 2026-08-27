<!-- Conventional Commit title: feat(scope): description -->

## What & why

<!-- What does this change and why? Link the issue: Closes #123 -->

## Changes

-

## Testing

- [ ] `cargo fmt --all -- --check` passes (`just fmt-check`)
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` is clean (`just clippy`)
- [ ] `cargo test --workspace --all-targets --locked` passes (`just test`)
- [ ] `actionlint -color` passes (`just actionlint`)

## Checklist

- [ ] Public CLI contracts unchanged (command names, JSON schemas, exit codes)
- [ ] Database migrations are append-only
- [ ] No secrets or local paths committed
