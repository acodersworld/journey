# Repository agent instructions

- The website is live. Preserve deployed data and account for existing deployments when changing interfaces or persisted data formats. Do not assume breaking changes are acceptable.
- Do not run `cargo fmt` or `cargo fmt --check` in this repository.
- Do not run `rustfmt` directly or use any automated source formatter.
- Preserve the existing formatting and make manual, targeted formatting edits only when needed.
- Put implementation plan documents in `docs/plans/`. After the code described by a plan has been implemented, preserve any lasting design decisions in `docs/design/` and remove the completed plan document.
- In Plan mode, agree on the implementation plan without changing files. If the current mode prohibits file edits, provide the complete plan in the response and say that no file was written.
- When a plan was agreed in Plan mode but has not yet been saved, the first request after switching to Default mode to "implement the plan" means save that plan in `docs/plans/` only. Do not change implementation code in that turn. A later, separate explicit request to implement the code may then build the feature.
- Outside that Plan-to-Default handoff, treat "implement the plan" in Default mode as a request to implement the code described by an already saved plan. Change code only when the user explicitly asks for code implementation or another code change.
- Database schema changes must be backwards compatible with the deployed schema or include an explicit, tested migration path that preserves existing data. Version schema changes and verify upgrades from the deployed version. Do not require deleting or recreating the live SQLite database or running the destructive importer as an upgrade procedure.
