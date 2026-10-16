# Klaviyo sandbox

The image bakes in `/home/agent/.pip/pip.conf` with placeholder credentials.
k-repo's Pants bootstrap reads this file for Python and Go package access.

Configure two static secret replacements in the existing Centaur Console:

| Placeholder | Real value |
| --- | --- |
| `KREPO_ARTIFACTORY_USERNAME` | JFrog username |
| `KREPO_ARTIFACTORY_PASSWORD` | JFrog password/token |

Match the `Authorization` header on `klaviyo.jfrog.io`, scoped to GET/HEAD on
`/artifactory/api/pypi/pypi/*`, `/artifactory/pypi/*`, and
`/artifactory/api/go/go-virtual/*`. Grant both secrets to the intended sandbox
principals through an existing role. Iron-proxy replaces the placeholders
inside Basic Auth; real credentials stay outside the image and sandbox.
