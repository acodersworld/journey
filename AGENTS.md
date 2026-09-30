# Repository agent instructions

- This project is a work in progress and has not been deployed. Breaking changes are acceptable; do not preserve compatibility solely for existing interfaces or data formats.
- Do not run `cargo fmt` or `cargo fmt --check` in this repository.
- Do not run `rustfmt` directly or use any automated source formatter.
- Preserve the existing formatting and make manual, targeted formatting edits only when needed.
- Put implementation plan documents in `docs/plans/`. After the code described by a plan has been implemented, preserve any lasting design decisions in `docs/design/` and remove the completed plan document.
- In Plan mode, agree on the implementation plan without changing files. If the current mode prohibits file edits, provide the complete plan in the response and say that no file was written.
- When a plan was agreed in Plan mode but has not yet been saved, the first request after switching to Default mode to "implement the plan" means save that plan in `docs/plans/` only. Do not change implementation code in that turn. A later, separate explicit request to implement the code may then build the feature.
- Outside that Plan-to-Default handoff, treat "implement the plan" in Default mode as a request to implement the code described by an already saved plan. Change code only when the user explicitly asks for code implementation or another code change.
- This development site has no schema migration process. Incompatible site database changes require recreating the SQLite database and running the destructive importer again. Reject outdated databases with a clear rebuild instruction; do not add legacy-row migrations solely to preserve local development data.
