# Repository agent instructions

- Do not run `cargo fmt` or `cargo fmt --check` in this repository.
- Do not run `rustfmt` directly or use any automated source formatter.
- Preserve the existing formatting and make manual, targeted formatting edits only when needed.
- Put implementation plan documents in `docs/plan/`. After the code described by a plan has been implemented, preserve any lasting design decisions in `docs/design/` and remove the completed plan document.
- In Plan mode, when the user asks to plan a change or says "implement the plan", write the agreed implementation plan to a document only; do not change implementation code. Treat "implement the plan" in Plan mode as a request to save the plan, not to build the feature. In other modes, treat "implement the plan" as a request to implement the code described by the plan. Change code only when the user explicitly asks for code implementation or another code change.
- If the current mode prohibits file edits, provide the complete plan in the response and say that no file was written. Save it in `docs/plan/` when file edits are permitted and the user requests the document.
