# Hydrogen long-request correction

User report: detailed first-year series/convergence notes fail with upstream 5xx and request timeouts on Hydrogen high.

## Evidence
- Production logs show Hydrogen HTTP-request timeouts and 5xx across retries.
- Existing HTTP deadline: 60 seconds; generation deadline: 180 seconds; output cap: 8192, shared with reasoning.
- Direct API metadata reports Hydrogen outputTokenLimit=65536. Direct short/high request timed out at 60.2 seconds, so failure is not specific to the math prompt.
- Existing system rules prohibit LaTeX/tables and one-line processing strips paragraph structure despite explicit requests.

## Correction and checks
- [x] Regression tests first: model-specific request deadlines/output cap; selection-specific total budget; preservation of requested structure.
- [x] Hydrogen HTTP deadline 300 seconds, total generation 660 seconds including retries; Flash-Lite remains 60/180. Existing outer classifier/ball bounds remain 10/12 seconds.
- [x] Hydrogen maxOutputTokens=32768; other models retain 8192. No silent model downgrade, quota behavior unchanged.
- [x] Permit explicitly requested LaTeX/tables; preserve structure for detailed/structured questions even with one-line preference.
- [x] Run all Rust tests and read-only review: 209 passed, 6 ignored. Reviewer requested English/section wording support; actual formatting functions passed an isolated red/green Rust regression harness after correction.
- [x] Record long probe result; report upstream availability separately from local fixes.

## Live reproduction
- Original 8192-token long/high request: HTTP 200 after 127.1 seconds; 9231 text characters; finishReason=MAX_TOKENS. Output 3517 tokens plus 4675 thinking tokens exhausted the cap exactly.
- Two long/high requests with 32768 tokens and 300-second deadline both returned HTTP 503 (5.7–6.3 seconds), explicitly reporting high demand. Full completion with the corrected limits is unverified because of provider overload, not claimed fixed upstream.

No production deployment or GitHub push without a new explicit user request. Server diagnostics use the running process key only in memory, never print it, and never start a second bot.
