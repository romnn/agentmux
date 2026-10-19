# 0017. Billing-sensitive environment variables require machine-file opt-in

Status: Accepted

## Context

Environment credentials and routing overrides can select metered APIs or another endpoint, billed
per token. Their precedence over a CLI's subscription login depends on the vendor and the login.
Without a guard, a key the launching shell happened to export — or one a project needs for its own
backend — could quietly turn a consultation into a paid one.

## Decision

`Billing::Subscription` is the default and needs no configuration. agentmux withholds classified
variables unless the account permits them:

- `delegate::scrub_billing_env` runs over the child's environment when the identity is resolved and
  again at the spawn (`ProcessLauncher::launch`, and the app-server driver for the server it
  starts). Under the subscription policy, no layer — host, `[launch]`, account `env`, request `env`
  — can supply an environment variable matched by the billing-name rule. The quota probes apply the
  same scrub.
- Under the subscription policy, `apply_request_env` refuses a billing-sensitive name even when
  allowlisted.
- `apply_account` refuses an account whose `Account::api_only_field` is set under a subscription,
  and an account a project file chose is subscription whatever it says
  ([0011](0011-machine-files-define-project-files-select.md)).

`delegate::is_billing_env_name` is a rule, not a list: any `*_API_KEY` or `*_API_KEY_FILE` name,
every vendor namespace's names with a `URL` segment, `*_ISSUER` and `*_AUTH_TOKEN` names (a token
endpoint override would send a subscription's refresh token elsewhere), plus a few named switches. A
subscription token such as `CLAUDE_CODE_OAUTH_TOKEN` is not among them.

The rule is about the environment only. What a CLI's own configuration or login says — a provider in
`~/.codex/config.toml`, a Codex API-key login, a key in Claude's `settings.json` — is the user's
choice for that CLI and is left alone, so no `-c` pin, login check or protocol check stands in for
it.

`billing = "api"` on an account in a machine file lifts exactly these restrictions for that account.
A caller may pin the pricing (`StartRequest::billing`, `--billing`, the MCP `billing` parameter):
`Api` only within the account's opt-in. A caller may always request the subscription restriction,
but incompatible API-only account fields cause launch refusal. `start` pins whatever the first turn
decides in `Meta::billing`, which every follow-up asks for again, so a project-selected account
cannot drift onto API pricing when the files change.

## Consequences

- The policy surfaces as `RunStatus::billing`, `RunSummary::billing`, `billing=api` in
  `report::delegate_line` and `list`, and a note under the question. Reports expose the permitted
  billing policy, not verified charges or the CLI's effective authentication mode.
- An account that names a key or an endpoint needs `billing = "api"` in the machine file, or its
  launches are refused naming the field.
