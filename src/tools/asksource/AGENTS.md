# ASK-owned tooling

`asksource` is ASK-only tooling and is not intended for submission to Rust
upstream. The user explicitly overrides inherited upstream-contribution
constraints for this tool and its ASK integration. Follow the ASK root
engineering and verification instructions. The canonical scope is recorded in
the ASK repository's `docs/rust-toolchain.md` and its testing history.

**HIGHEST PRIORITY — Optional, explicitly authorized long analysis only.**
Follow the ASK root `docs/development-workflow.md` authorization procedure.
Never invoke asksource, its readers or helper-invoking tests during routine
implementation. Only long analysis after multiple completed features form a
substantial integrated surface is eligible; prepare the named commands, scope,
cost/resources and failure handling, then receive explicit user approval before
execution. Syntax-only, diagnostic, indirect preflight and SDK/bootstrap calls
have no exemption. A general implementation/test request is not consent. New
batches and unapproved retries need fresh approval. Analysis is an enhancement,
never an implementation dependency or exit; continue independent verification
when facts or approval are unavailable. Existing implicit helper paths are
implementation gaps to remove, not exceptions. After batch approval and before
invocation, establish an exclusive host/session window: pause all other agents
sharing the host, including disjoint work, and prevent new task activities
until all analysis descendants exit and the coordinator releases the hold.
No other agent work or new sub-agents are permitted. If the host cannot enforce
exclusivity or verify all pauses, release acquired pauses and defer analysis,
not implementation. Let existing Cargo finish naturally; the diary and budget
lock are not an enforced no-new-activity hold. Preserve required security
checks and the approved batch's containment, unrestricted CPU and no Cargo deadlines.
