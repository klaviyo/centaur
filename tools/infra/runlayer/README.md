# Personal Runlayer Plugin access

Connect Runlayer once in Console **Integrations**. Centaur uses the connection
of the person requesting each turn, including Slack channel turns when the
integration is marked **always available**. Sign into Console with Slack SSO to
link the verified Slack user and team; email/profile text is not used to select
a Runlayer connection.

```sh
runlayer discover
runlayer search "find recent build failures"
runlayer call EXACT_ADVERTISED_NAME '{"argument":"value"}'
```

Discovery and execution use one unified Runlayer Plugin MCP connection. Exact
names and schemas come from Runlayer; there are no per-service adapters. The
client supports paginated tools/list, JSON and SSE replies, flattened tools,
and the nested search_tools/execute_tool presentation. Connector consent and
approval responses remain visible. Calls are never automatically replayed.
MCP sessions and tool definitions are not persisted between CLI invocations.

## Deployment

Configure the integration in Console using the exact HTTPS Plugin MCP URL supplied
by Runlayer. This URL lives on the OAuth App in its `mcp_url` label, alongside
the registered OAuth client; it is not a deployment-wide setting. The supported authorization server endpoints under the same origin
are `/api/v1/oauth/{authorize,token,register,revoke}`. The resource scope is
`mcp:proxy`. Confirm these against the tenant's public OAuth metadata before
rollout. The implementation uses authorization code + S256 PKCE with
`client_secret_post`, and sends the resource on authorization, code exchange,
and refresh. It does not require an agent account or OBO delegation.

An administrator can enter the URL on the Runlayer preset in Integrations and
click Connect to register the OAuth client through DCR. Alternatively create a personal OAuth App manually
with provider `runlayer`, the registered client ID/secret, and scope
`mcp:proxy`, adding the `mcp_url` label in the existing label editor; register
`<Console public URL>/oauth/<app-slug>/callback` exactly.
Enable **always available** for requester credentials in shared channels.
Every person then connects their own Runlayer account through that app.

The gateway obtains its endpoint and connect link from Console's authenticated
sandbox OAuth-app catalog. Tokens are encrypted in the existing credential
broker and injected by iron-proxy only on GET/POST/DELETE to the configured
host and path. The gateway carries no shared token, user ID argument, client
secret, refresh token, or real access token. The integration's exact resource
URL determines the credential's request rules; no wildcard host grant or
Runlayer-specific deployment setting is needed. Centaur's existing sandbox and
proxy network policies apply.

A connection is keyed by its Centaur owner. MCP OAuth does not require an
OIDC/userinfo response, so Centaur does not decode an opaque access token or
claim that its local connection key is a Runlayer account ID. Reconnect replaces
that owner's grant. The Runlayer login/consent screen selects the external
account, whose access Runlayer enforces on discovery and execution.

Disconnect removes local access and requests access- and refresh-token revocation from
Runlayer. A failed remote revocation is reported, while local access remains
disabled. Dead/expired grants, disabled apps/users, role grants and grants to a
different person cannot supply a personal Runlayer token. Configuration changes
require reconnecting. Missing connector authorization remains a separate
Runlayer consent step and is surfaced through its response/link.

## Validation

Run the Python package tests and the Console personal_runlayer integration
tests, plus the proxy-sync database integration suite. The latter exercises
A → B → A requester changes, shared/cross-owner grant rejection, revocation and
expiry against the same database schema used by Console. Before rollout, two
real users must complete Runlayer consent and verify different discovery and
execution permissions, revocation, and connector-specific consent.
