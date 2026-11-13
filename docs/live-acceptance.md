# Live cross-account resume acceptance

Automated fixtures verify Relay's plumbing. This procedure verifies the actual Claude release. Run only with accounts the tester is authorized to use; model requests consume their subscription allowance.

1. Record OS, Relay commit, and `claude --version`. Use a separate test project and a fresh `AGENT_RELAY_HOME`; keep its path stable afterward.
2. Add two distinct subscription accounts through `account add`. Check account names, email identities, and plans using `account list` and `doctor`.
3. Under account A, start a conversation in the test project. Supply a unique phrase and ask Claude to read a harmless fixture file, producing a tool result. Note the session ID. Exit Claude normally.
4. Under account B, use `--continue` in the same project. Run `/status` to verify B is the active subscription. Verify the transcript contains the phrase, tool call, and result. Ask a question that relies on that prior context. Exit normally.
5. Resume the explicit session ID under A, then use the picker under B. Confirm each launches the intended account and resumes the intended conversation, rather than starting a new one.
6. Switch the default account and repeat a default `run -- --continue`. Confirm a running session retains its original account when another terminal changes the default.
7. While a Relay session is open, try another Relay launch from the same project and a different project. Both must be refused. Confirm login renewal for the active account and history import are also refused.
8. Close the session and any descendants retaining lock descriptors. Confirm launching resumes normally without deleting lock files. Test Ctrl-C and terminal closure; inspect recovery behavior after interruption.
9. Export harmless historical fixture transcripts to another directory. Import that directory, inspect the retained snapshot, and resume an imported conversation. Repeat with a deliberately conflicting incoming file; existing content must remain unchanged and the incoming conflict must be retained.
10. Confirm known API credential/provider overrides are refused before model requests. Do not put real secrets in test output. Test account and project settings as well as shell variables.
11. Leave one account unused until its access token expires while its saved refresh credential remains valid, without logging out or changing credentials manually. Switch back and resume its conversation. It must refresh through the official CLI without another browser login. Record the local auth-status result before launch: if Relay refuses a login Claude can refresh, treat this as a release-blocking compatibility issue. Distinguish a temporary access-token expiry from a genuinely expired or revoked refresh credential.

Record results and limitations, including compaction, model availability, project memory, file rewind/checkpoints, plans, MCP authentication, and background tasks. File rewind and running-task restoration are outside the initial guarantee. Do not mark a platform/version accepted if authentication or transcript identity cannot be established.

Test artifacts can contain account identifiers and conversation contents. Keep acceptance records private or sanitize them before sharing. Do not publish credentials.
