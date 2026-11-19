// Linear delta (no slackbotv2 analog): a Linear turn runs as the person who
// triggered it, but only once they have connected their Linear account in
// Console. These notes tell an unlinked person how to fix that.

/** Where a person connects their Linear account, when the Console URL is known. */
function integrationsUrl(
  consolePublicUrl: string | undefined,
): string | undefined {
  if (!consolePublicUrl) return undefined;
  return new URL("/console/integrations", consolePublicUrl).toString();
}

function connectInstruction(consolePublicUrl: string | undefined): string {
  const url = integrationsUrl(consolePublicUrl);
  return url
    ? `Connect Linear at ${url}`
    : "Connect Linear under Integrations in Centaur Console";
}

/**
 * Appended once per person per thread to an answer that ran without that
 * person's own permissions.
 */
export function requesterUnlinkedNote(
  consolePublicUrl: string | undefined,
): string {
  return `_This ran without your personal permissions because your Linear account isn't connected to Centaur. ${connectInstruction(consolePublicUrl)} so future work runs as you._`;
}

/** The whole reply when the deployment refuses turns from unlinked people. */
export function requesterRefusedReply(
  consolePublicUrl: string | undefined,
): string {
  return `I can't work on this yet: your Linear account isn't connected to Centaur, and Linear work here only runs with the requester's own permissions. ${connectInstruction(consolePublicUrl)}, then comment again.`;
}
