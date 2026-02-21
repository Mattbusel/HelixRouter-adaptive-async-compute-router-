# Agent Coordination

## Roles
- Builder Agent: implements features, owns specific modules
- Test Agent: writes tests only

## Module Ownership
- src/main.rs — router core, strategies, config
- src/metrics.rs — Prometheus, JSON, latency tracking
- src/web.rs — browser UI, HTTP endpoints
- tests/ — all test coverage

## Protocol
- Claim your module at start:
  "PROTOCOL ACKNOWLEDGED — claiming [module]"
- Do not edit modules owned by another active agent
- Run cargo test before committing
- Commit format: [feat/fix/docs] description
- Push to both: git push origin main && git push origin master
