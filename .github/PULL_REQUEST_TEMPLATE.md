Fixes #

## Affected Subsystem(s)

- [ ] `protocol-and-transport` (`src/protocol.rs`, `src/transport.rs`)
- [ ] `worker-and-pty` (`src/worker.rs`, `src/session.rs`)
- [ ] `hooks-and-installer` (`src/hooks.rs`, `src/install.rs`, `hooks/`, `skills/`)
- [ ] `server-and-webui` (`src/server.rs`, `src/terminal_gui.rs`, `webui/`)
- [ ] `loop-and-release` (`scripts/`, `.github/`, `docs/`)

## Semantic Change (Before / After)

- **Trigger / Condition**:
- **Before**:
- **After**:

## Verification Commands Run

- [ ] `node scripts/check-governance.mjs`
- [ ] `node --test test/governance.test.js`
- [ ] `cargo fmt --check`
- [ ] `cargo test --all-targets --locked`
- [ ] `cargo clippy --all-targets --all-features --locked -- -D warnings`
- [ ] `node test/launcher.test.js`
- [ ] `cd webui && npm test && npm run build` (if `webui/` changed)

<!-- Optional governance waivers (uncomment only when applicable):
No-Test-Reason: <explain why src/*.rs change did not add a new test>
Security-Boundary: <explain trust/permission boundary if .github/workflows/* changed>
-->
