# Repository Guidelines

## Project Structure & Module Organization
`src/main.rs` wires the CLI to the async runtime and hands traffic to the reverse proxy. Shared logic sits in `src/lib.rs`, while `src/proxy_service.rs` handles Hyper routing and WebSocket upgrades. Scaling orchestration with the ECS APIs lives in `src/ecs_service_scaler.rs`. The `src/waiting.html` asset is served while tasks warm up; keep it lightweight. Release artifacts land in `target/`, and the `Dockerfile` builds the minimal runtime image used in production.

## Build, Test, and Development Commands
Run `cargo check` early for fast type validation. Format with `cargo fmt` and lint via `cargo clippy --all-targets --all-features`. Execute the suite with `cargo test`; add `-- --nocapture` when debugging logs. `cargo run -- --cluster demo --service demo --target-port 3000` runs the proxy locally against AWS. Produce deployable binaries using `cargo build --release` or build the container image with `docker build -t manini .`.

## Coding Style & Naming Conventions
We target Rust 2024 with 4-space indentation. Follow idiomatic `snake_case` for modules and functions, `CamelCase` for types, and `SCREAMING_SNAKE_CASE` for env vars. Keep public interfaces doc-commented and avoid panics outside `main`. Always run `cargo fmt` and `cargo clippy` before opening a PR; treat clippy warnings as bugs unless documented otherwise.

## Testing Guidelines
Use `cargo test` for unit coverage and prefer focused modules over broad integration tests. Asynchronous code should rely on `#[tokio::test]`. When touching scaling logic, add regressions around idle timers and ECS state transitions. Capture AWS interactions behind traits so they can be mocked with lightweight stubs; reuse existing patterns in `src/lib.rs`. Document notable test scenarios in PR descriptions when they exercise edge cases.

Always set a timeout when running tests, e.g., `timeout 120 cargo test`.

## Commit & Pull Request Guidelines
The history follows Conventional Commits (`feat`, `refactor`, `chore`, etc.)—continue the pattern with imperative summaries under 72 characters. Each PR should describe the motivation, outline testing performed, and link related issues or runbooks. Include configuration updates (environment variables, HTML copy) in the same change with context. Request review when CI is green and attach screenshots or curl transcripts if the waiting page or proxy behaviour changed.

## AI Agent Execution Policy
AI agent-driven debug runs are prohibited. Agents may automate tasks through `cargo test`, but humans must handle runtime validation and any live debugging.
