# Sanity

Shared Content Lake access through iron-proxy. Supports GROQ queries, mutations,
document and release actions (including publication), and local asset uploads.
This is independent of personal Runlayer OAuth connections.

## Configuration

Set `SANITY_PROJECT_ID` and `SANITY_DATASET` in `sandbox.extraEnv`. They are
non-secret defaults, not authorization boundaries. The client validates them
before constructing a fixed HTTPS URL and never follows redirects.

An administrator must enroll `SANITY_API_TOKEN` as a static **replace** secret
matching the `Authorization` header, with request rules restricted to the
approved project host and dataset paths. Grant it through a dedicated role,
make the role a default for new principals, and explicitly assign existing
principals. Do not grant the warm-pool bootstrap principal. The tool deliberately
does not auto-register a credential source or add credentials to the infra role.
Keep the real value in an external secret provider or an encrypted Console
`control_plane` source; the sandbox receives only the placeholder.

Restrict the robot's role in Sanity itself to the approved dataset and operations.
Having publication capability does not replace the user's publication approval.

## Usage

Put JSON arguments in a local file, then invoke:

```sh
sanity call query --input /tmp/query.json
sanity call mutate --input /tmp/mutations.json
sanity call actions --input /tmp/actions.json
sanity call upload_asset --input /tmp/asset.json
```

For example, query arguments are `{"query":"count(*)","params":{}}`.
Mutation arguments are `{"mutations":[{"patch":{"id":"article-id",
"set":{"title":"Updated title"}}}],"dry_run":true}`.
Publication arguments are `{"actions":[{"actionType":"sanity.action.document.publish",
"draftId":"drafts.article-id","publishedId":"article-id"}]}`.
Asset arguments are `{"file_path":"/tmp/image.png","kind":"images"}`.

Workflow callers can use `centaur-tools call sanity query '{"query":"count(*)"}'`.
For CLI calls use input files: the generic shim records command arguments, and
putting article text in arguments would copy that text into CLI analytics.

## Audit trail

Each HTTP operation appends `sanity_operation` events before and after the
request to `CENTAUR_TOOL_ANALYTICS_LOG_PATH` (default `/proc/1/fd/2`, the sandbox
container log sink). A missing sink prevents a new request. A failed completion
log after an upstream write is an ambiguous outcome, not permission to retry.
Timeouts and upstream 5xx responses are also ambiguous; writes are never replayed
automatically.

Events contain timestamp, operation ID, project, dataset, operation name/types, thread
key, explicit document IDs, returned mutation/asset IDs and transaction ID,
status, duration, dry-run flag, and outcome. They exclude credentials, request
headers, GROQ/parameters, mutation content, file names, and response bodies.
`started` without a matching completion requires investigation. Query results
and query-selected document IDs are not recorded.

Find operations with `event:sanity_operation`. Join `thread_key` to the durable
session/execution history for the human requester or workflow trigger. A channel
principal is shared and must not be treated as a unique human identity. These
are operational logs written inside the sandbox, not a tamper-proof audit ledger.
The independent iron-proxy request logs are needed to observe direct HTTP calls
that bypass this tool. Configure collection/retention outside the sandbox and
verify both paths before granting broad access. No raw-body capture is required.

## Validation

Run the focused tests and build the package, then from a local sandbox verify
catalog discovery, `sanity --help`, a real proxied query and both audit events.
Use a disposable dataset for edit/publication tests. Test a denied project and
dataset and confirm that the credential is never injected there. Revoke the
role and verify denial on a new turn. Package tests alone do not establish a
working deployed credential or retained logs.

Focused checks (from the repository root):

```sh
uv run --with pytest --with httpx --with typer --with-editable ./centaur_sdk python -m pytest tools/productivity/sanity/tests -q
uv run --with ruff ruff check tools/productivity/sanity
uv build tools/productivity/sanity
```
