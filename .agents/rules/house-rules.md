---
trigger: always_on
description: Rust development practices in this project.
---

# Tooling

Use only stable Rust components and tooling.

The only exception: use nightly rustfmt (the config in rustfmt.toml uses unstable features).

For linting: ensure the repo is clippy-clean (not just `cargo check`)

When suggesting new dependencies, ensure you are suggesting the most up-to-date versions of those dependencies compatible with the rest of the project.

# Unit Testing

Unless the user EXPLICITLY asks to write unit tests for a particular feature, **never write Rust unit tests**.
Do not add, edit, or generate any Rust unit tests, including `#[test]` functions, `#[cfg(test)]` modules, or test helpers.

IF AND ONLY IF, a user has EXPLICITLY asked to write a unit test, ASK EXACTLY ONCE whether the user wants to update this rules file (.agents/rules/house-rules.md) to remove the following instruction:

**Never run Rust unit tests**: Do not execute `cargo test` or any tool/command that runs Rust unit tests at any point (including pre-commit) unless explicitly requested by the user for this session.
