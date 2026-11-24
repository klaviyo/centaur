import type { GitHubAdapter } from "@chat-adapter/github";
import type { StateAdapter } from "chat";
import { backgroundWaitUntil } from "./context";
import { resolveAllowedAuthorAssociations } from "./authorization";
import { reactWorkingOnReview, settleReviewReaction } from "./reactions";
import { runTurnStream, turnOutputChars } from "./turn";
import {
  fetchCiEvaluation,
  maybeEmitReviewSubmitted,
  prepareCiCompleted,
  type CiEvaluation,
} from "./workflow-events";
import {
  getPrBinding,
  isRetryableSessionApiError,
  reportPrCiFailure,
  updatePrBindingState,
  type PrBindingRef,
} from "./session-api";
import type {
  ForwardSessionInput,
  GithubbotApiMessage,
  GithubbotOptions,
  GithubbotTrace,
  PrBinding,
  PrBindingStateUpdate,
} from "./types";
import { errorMessage, noopLogger, nowMs, stringValue, traceLog } from "./utils";

/**
 * v2: PR self-management for PRs the bot owns (it authored them, or they carry
 * the managed label). Reacts to PR/review/CI lifecycle webhooks to drive an
 * owned PR toward merge:
 *  - Fix CI    — once *all* checks for a head SHA are settled and red, run a
 *                bounded fix turn (the agent diagnoses + pushes via gh).
 *  - Address review — one holistic turn per submitted review.
 *  - Merge     — deterministic: when GitHub reports the PR mergeable (clean),
 *                the bot merges it directly (no agent — branch protection is the
 *                source of truth). dirty -> conflict turn; behind -> update.
 * Escalation tags a human and stops; the bot backs off human-authored commits.
 */
/** The Octokit instance the GitHub adapter exposes (its `.octokit` getter). */
type Octokit = GitHubAdapter["octokit"];

export type PrManagerContext = {
  octokit: Octokit;
  options: GithubbotOptions;
  state: StateAdapter;
  userName: string;
  /** Delays between in-process attempts at a critical state write; tests inject their own. */
  stateWriteRetryDelaysMs?: number[];
  /** Timer for those delays; tests inject an instant one. */
  sleep?: (ms: number) => Promise<void>;
};

const STATE_TTL_MS = 90 * 24 * 60 * 60 * 1000;
const CLAIM_TTL_MS = 7 * 24 * 60 * 60 * 1000;
const DEFAULT_CI_FIX_MAX_ATTEMPTS = 3;
// Close, unassign and reopen writes invalidate a notice, so they get 3 attempts
// over about 30s (10s then 20s apart) before githubbot gives up and logs.
const DEFAULT_STATE_WRITE_RETRY_DELAYS_MS = [10_000, 20_000];

// ---------------------------------------------------------------------------
// Pure decision helpers (unit-tested without GitHub).
// ---------------------------------------------------------------------------

/**
 * A PR is bot-owned when the bot is one of its assignees. Ownership is purely an
 * assignment mechanism: assign the PR to the bot to have it manage the PR toward
 * merge (and unassign to hand it back).
 */
export function isOwnedPr(input: {
  assignees: string[];
  userName: string;
}): boolean {
  const target = input.userName.toLowerCase();
  return input.assignees.some((login) => login.toLowerCase() === target);
}

export type MergeDecision =
  | "merge"
  | "resolve_conflict"
  | "update_branch"
  | "wait"
  | "skip_disabled"
  | "skip_hold"
  | "skip_draft"
  | "skip_closed";

/**
 * Whether (and how) to act on merge-readiness. Branch protection is the source
 * of truth, surfaced as mergeable_state: only "clean" merges; "dirty" needs a
 * conflict turn; "behind" needs a branch update; everything else waits.
 */
export function decideMerge(input: {
  autoMerge: boolean;
  draft: boolean;
  holdLabel: string;
  labels: string[];
  merged: boolean;
  mergeableState: string;
  state: string;
}): MergeDecision {
  if (!input.autoMerge) return "skip_disabled";
  if (input.merged || input.state !== "open") return "skip_closed";
  if (input.draft) return "skip_draft";
  if (input.labels.map((l) => l.toLowerCase()).includes(input.holdLabel.toLowerCase())) {
    return "skip_hold";
  }
  if (input.mergeableState === "dirty") return "resolve_conflict";
  if (input.mergeableState === "behind") return "update_branch";
  if (input.mergeableState === "clean") return "merge";
  // blocked / unstable / unknown / has_hooks -> not cleanly mergeable yet.
  return "wait";
}

// ---------------------------------------------------------------------------
// Per-PR state (stored as a JSON blob in the shared KV).
// ---------------------------------------------------------------------------

type PrState = {
  consecutiveCiFixes?: number;
};

function prKey(ctx: PrManagerContext, owner: string, repo: string, n: number): string {
  return `${ctx.options.stateKeyPrefix ?? "centaur-githubbot"}:pr:${owner}/${repo}#${n}`;
}

export function managementThreadKey(
  owner: string,
  repo: string,
  n: number,
): string {
  return `github-manage:${owner}/${repo}:${n}`;
}

async function loadState(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  n: number,
): Promise<PrState> {
  try {
    return (await ctx.state.get<PrState>(prKey(ctx, owner, repo, n))) ?? {};
  } catch {
    return {};
  }
}

async function saveState(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  n: number,
  value: PrState,
): Promise<void> {
  try {
    await ctx.state.set(prKey(ctx, owner, repo, n), value, STATE_TTL_MS);
  } catch (error) {
    logger(ctx).debug("githubbot_pr_state_save_failed", {
      error: errorMessage(error),
    });
  }
}

async function claim(ctx: PrManagerContext, key: string): Promise<boolean> {
  try {
    return await ctx.state.setIfNotExists(key, "1", CLAIM_TTL_MS);
  } catch {
    // If the claim store is unavailable, proceed (better to act than to silently
    // drop work); the in-turn idempotency keys still guard double execution.
    return true;
  }
}

/**
 * Release a claim so the action it guarded can be retried on a later event.
 * Used when an irreversible side effect (the merge) fails after the claim is
 * taken — otherwise the stale claim would suppress every future attempt.
 */
async function release(ctx: PrManagerContext, key: string): Promise<void> {
  try {
    await ctx.state.delete(key);
  } catch {
    // best-effort; the claim's TTL eventually expires if delete fails.
  }
}

function logger(ctx: PrManagerContext) {
  return ctx.options.logger ?? noopLogger;
}

// ---------------------------------------------------------------------------
// Webhook handlers.
// ---------------------------------------------------------------------------

type PullRequestSummary = {
  assignees: string[];
  draft: boolean;
  headRef: string;
  headRepoFullName: string | null;
  headSha: string;
  htmlUrl: string | null;
  labels: string[];
  mergeableState: string;
  merged: boolean;
  nodeId: string | null;
  number: number;
  state: string;
  title: string;
  updatedAt: string | null;
};

function assigneeLogins(
  value: ({ login?: string } | null)[] | null | undefined,
): string[] {
  if (!value) return [];
  return value.map((a) => a?.login ?? "").filter(Boolean);
}

function summarizePr(pr: {
  draft?: boolean | null;
  head: { ref: string; repo?: { full_name?: string | null } | null; sha: string };
  html_url?: string | null;
  labels: { name?: string }[];
  mergeable_state?: string;
  merged?: boolean;
  node_id?: string | null;
  number: number;
  state: string;
  title: string;
  updated_at?: string | null;
  assignees?: ({ login?: string } | null)[] | null;
}): PullRequestSummary {
  return {
    assignees: assigneeLogins(pr.assignees),
    draft: pr.draft === true,
    headRef: pr.head.ref,
    headRepoFullName: pr.head.repo?.full_name ?? null,
    headSha: pr.head.sha,
    htmlUrl: pr.html_url ?? null,
    labels: pr.labels.map((l) => l.name ?? "").filter(Boolean),
    mergeableState: pr.mergeable_state ?? "unknown",
    merged: pr.merged === true,
    nodeId: pr.node_id ?? null,
    number: pr.number,
    state: pr.state,
    title: pr.title,
    updatedAt: pr.updated_at ?? null,
  };
}

async function fetchPr(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  n: number,
): Promise<PullRequestSummary | null> {
  try {
    const { data } = await ctx.octokit.rest.pulls.get({
      owner,
      repo,
      pull_number: n,
    });
    return summarizePr(data as Parameters<typeof summarizePr>[0]);
  } catch (error) {
    logger(ctx).warn("githubbot_pr_fetch_failed", {
      error: errorMessage(error),
      pr: `${owner}/${repo}#${n}`,
    });
    return null;
  }
}

const OWNED_CACHE_TTL_MS = 10 * 60 * 1000;

/**
 * Whether a PR is bot-owned, cached briefly so the conversational path doesn't
 * hit the API on every comment. Ownership rarely changes, and a stale "owned"
 * only affects which session a reply shares context with — low stakes.
 */
export async function isPrOwned(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  number: number,
): Promise<boolean> {
  const cacheKey = `${ctx.options.stateKeyPrefix ?? "centaur-githubbot"}:owned-cache:${owner}/${repo}#${number}`;
  try {
    const cached = await ctx.state.get<string>(cacheKey);
    if (cached === "1") return true;
    if (cached === "0") return false;
  } catch {
    // fall through to a live lookup
  }
  const pr = await fetchPr(ctx, owner, repo, number);
  const owned = pr ? owns(ctx, pr) : false;
  try {
    await ctx.state.set(cacheKey, owned ? "1" : "0", OWNED_CACHE_TTL_MS);
  } catch {
    // best-effort cache
  }
  return owned;
}

function owns(ctx: PrManagerContext, pr: PullRequestSummary): boolean {
  return isOwnedPr({ assignees: pr.assignees, userName: ctx.userName });
}

/** `pull_request` lifecycle (non-review_requested actions). */
export async function handlePullRequestEvent(
  ctx: PrManagerContext,
  rawBody: string,
): Promise<void> {
  const payload = parseJson(rawBody);
  if (!payload) return;
  const action = stringValue(payload.action);
  if (!action || action === "review_requested") return; // review_requested is v1's.
  const repo = repoFromPayload(payload);
  const prNode = payload.pull_request;
  if (!repo || !isRecord(prNode)) return;
  const number = numberValue(prNode.number);
  if (number === undefined) return;
  // A PR opened through a session binding keeps its lifecycle state in api-rs,
  // recorded before (and regardless of) the unbound handling below.
  const lookup = await lookupBinding(ctx, repo.owner, repo.repo, number);
  // A failed lookup is not "unbound": the invalidating writes (close, unassign,
  // reopen) are still attempted on the signed payload's node id, which api-rs
  // checks against the binding.
  if (!lookup.ok || lookup.binding) {
    await recordBindingState(
      ctx,
      bindingRef(repo.owner, repo.repo, number),
      lookup.ok ? lookup.binding : null,
      action,
      payload,
      prNode,
    );
  }
  if (action === "closed") return; // nothing to drive once closed/merged.

  const pr = await fetchPr(ctx, repo.owner, repo.repo, number);
  if (!pr || !owns(ctx, pr)) return;
  // Being assigned the PR is the explicit signal to take it over: evaluate CI now
  // (forcing past the human-commit back-off — the assignment is a human handing
  // it to us) so an already-red or already-green PR is acted on immediately,
  // rather than only on the next lifecycle event. processCi fixes red CI or merges
  // when green.
  if (action === "assigned") {
    await processCi(ctx, repo.owner, repo.repo, number, pr.headSha, true);
    return;
  }
  // Any other state change that could flip mergeability re-evaluates the merge
  // gate; it's deterministic and idempotent, so over-calling is harmless.
  await tryMerge(ctx, repo.owner, repo.repo, number);
}

function bindingRef(owner: string, repo: string, number: number): PrBindingRef {
  return { repository: `${owner}/${repo}`.toLowerCase(), number };
}

type BindingLookup = { ok: true; binding: PrBinding | null } | { ok: false; error: unknown };

/**
 * The PR's session binding. `binding: null` is an explicit 404 (unbound);
 * `ok: false` means api-rs could not say, which is never the same as unbound.
 */
async function lookupBinding(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  number: number,
): Promise<BindingLookup> {
  try {
    return { ok: true, binding: await getPrBinding(ctx.options, bindingRef(owner, repo, number)) };
  } catch (error) {
    logger(ctx).warn("githubbot_pr_binding_lookup_failed", {
      error: errorMessage(error),
      pr: `${owner}/${repo}#${number}`,
    });
    return { ok: false, error };
  }
}

/**
 * Record a lifecycle event on the binding, only for the PR the binding was
 * created for (`pr_node_id`). Every write is derived from a fresh fetch, never
 * the payload, and carries that fetch's `updated_at` so api-rs can drop a write
 * a later one has superseded: reordered or concurrent deliveries converge.
 * Management is enabled only by an explicit assignment: the event's own
 * `assignee` is the bot and the bot is still assigned on the fresh read.
 *
 * `binding` is null when the binding lookup failed; only the invalidating
 * actions are attempted then, on the signed payload's node id.
 */
async function recordBindingState(
  ctx: PrManagerContext,
  ref: PrBindingRef,
  binding: PrBinding | null,
  action: string,
  payload: Record<string, unknown>,
  prNode: Record<string, unknown>,
): Promise<void> {
  const prLabel = `${ref.repository}#${ref.number}`;
  const assigneeIsBot =
    isRecord(payload.assignee) &&
    stringValue(payload.assignee.login)?.toLowerCase() === ctx.userName.toLowerCase();
  const botAssignment = assigneeIsBot && (action === "assigned" || action === "unassigned");
  if (!["closed", "reopened", "synchronize"].includes(action) && !botAssignment) return;
  const invalidating = action === "closed" || action === "reopened" || action === "unassigned";
  if (!binding && !invalidating) return;

  const [owner = "", repo = ""] = ref.repository.split("/", 2);
  const fresh = await fetchPr(ctx, owner, repo, ref.number);
  if (!fresh?.updatedAt) {
    logger(ctx).warn("githubbot_pr_binding_no_fresh_state", { action, pr: prLabel });
    return;
  }
  // The binding only speaks for the PR it was created for.
  const payloadNodeId = stringValue(prNode.node_id);
  const expectedNodeId = binding?.pr_node_id ?? payloadNodeId;
  if (!expectedNodeId || (payloadNodeId && payloadNodeId !== expectedNodeId) || fresh.nodeId !== expectedNodeId) {
    logger(ctx).warn("githubbot_pr_binding_identity_mismatch", { action, pr: prLabel });
    return;
  }

  let update: Partial<PrBindingStateUpdate> | undefined;
  if (action === "closed" || action === "reopened") {
    update = { closed: fresh.state !== "open" || fresh.merged };
  } else if (action === "synchronize") update = { head_sha: fresh.headSha };
  else if (action === "assigned") {
    update = owns(ctx, fresh) ? { managed: true, head_sha: fresh.headSha } : undefined;
  } else if (action === "unassigned") {
    update = owns(ctx, fresh) ? undefined : { managed: false };
  }
  if (!update) return;
  await writeBindingState(
    ctx,
    ref,
    { ...update, pr_node_id: expectedNodeId, observed_updated_at: fresh.updatedAt },
    invalidating,
    action,
  );
}

/**
 * Write binding state; with `retry`, up to three attempts with bounded backoff
 * for retryable and network errors. Resolves true when api-rs accepted the call
 * (an unapplied, superseded write counts), false after giving up.
 */
async function writeBindingState(
  ctx: PrManagerContext,
  ref: PrBindingRef,
  update: PrBindingStateUpdate,
  retry: boolean,
  action: string,
): Promise<boolean> {
  const delays = retry ? (ctx.stateWriteRetryDelaysMs ?? DEFAULT_STATE_WRITE_RETRY_DELAYS_MS) : [];
  const sleep = ctx.sleep ?? ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)));
  for (let attempt = 0; ; attempt += 1) {
    try {
      const { applied } = await updatePrBindingState(ctx.options, ref, update);
      if (!applied) {
        logger(ctx).debug("githubbot_pr_binding_state_superseded", {
          action,
          pr: `${ref.repository}#${ref.number}`,
        });
      }
      return true;
    } catch (error) {
      const last = attempt >= delays.length || !isRetryableSessionApiError(error);
      logger(ctx).warn(
        last ? "githubbot_pr_binding_state_failed" : "githubbot_pr_binding_state_retrying",
        {
          action,
          attempt: attempt + 1,
          error: errorMessage(error),
          pr: `${ref.repository}#${ref.number}`,
        },
      );
      if (last) return false;
      await sleep(delays[attempt] ?? 0);
    }
  }
}

/** `pull_request_review` submitted -> address review, or merge on approval. */
export async function handleReviewEvent(
  ctx: PrManagerContext,
  rawBody: string,
): Promise<void> {
  const payload = parseJson(rawBody);
  if (!payload) return;
  if (stringValue(payload.action) !== "submitted") return;
  const repo = repoFromPayload(payload);
  const prNode = payload.pull_request;
  const reviewNode = payload.review;
  if (!repo || !isRecord(prNode) || !isRecord(reviewNode)) return;
  const number = numberValue(prNode.number);
  const reviewId = numberValue(reviewNode.id);
  if (number === undefined || reviewId === undefined) return;
  const reviewer = stringValue(isRecord(reviewNode.user) ? reviewNode.user.login : undefined);
  const reviewState = stringValue(reviewNode.state)?.toLowerCase();

  // A review is tied to review.commit_id. The PR head can advance before this
  // webhook is handled, so a live PR lookup would correlate the review to the
  // wrong push.
  const reviewedHeadSha =
    stringValue(reviewNode.commit_id) ??
    stringValue(isRecord(prNode.head) ? prNode.head.sha : undefined);
  if (reviewedHeadSha) {
    backgroundWaitUntil(
      maybeEmitReviewSubmitted(
        ctx,
        repo,
        number,
        reviewedHeadSha,
        reviewer,
        reviewState,
        reviewId,
      ),
    );
  }

  const pr = await fetchPr(ctx, repo.owner, repo.repo, number);
  if (!pr || !owns(ctx, pr)) return;
  // Never act on the bot's own review (it shouldn't review its own PRs anyway).
  if (reviewer && reviewer.toLowerCase() === ctx.userName.toLowerCase()) return;

  // Same trust gate as the comment path: an untrusted reviewer must not steer
  // a write-capable turn or drive the merge signal on a bot-owned PR.
  const allowed = resolveAllowedAuthorAssociations(
    ctx.options.allowedAuthorAssociations,
  );
  const association = stringValue(reviewNode.author_association)?.toUpperCase();
  if (
    !allowed.includes("*") &&
    (!association || !allowed.includes(association))
  ) {
    return;
  }

  if (
    !(await claim(
      ctx,
      `${ctx.options.stateKeyPrefix ?? "centaur-githubbot"}:review-handled:${repo.owner}/${repo.repo}#${number}:${reviewId}`,
    ))
  ) {
    return;
  }

  if (reviewState === "approved") {
    await tryMerge(ctx, repo.owner, repo.repo, number);
    return;
  }
  if (reviewState === "changes_requested" || reviewState === "commented") {
    fireAddressReviewTurn(ctx, repo.owner, repo.repo, pr, {
      reviewer: reviewer ?? "the reviewer",
      reviewId,
      reviewNodeId: stringValue(reviewNode.node_id),
    });
  }
}

/** check_run / check_suite / workflow_run completed -> CI-settled gate. */
export async function handleCiEvent(
  ctx: PrManagerContext,
  eventType: string,
  rawBody: string,
): Promise<void> {
  const payload = parseJson(rawBody);
  if (!payload) return;
  const repo = repoFromPayload(payload);
  if (!repo) return;
  const target = ciTarget(eventType, payload);
  if (!target) return;
  const { emission, evaluation } = await prepareCiCompleted(
    ctx,
    eventType,
    repo,
    payload,
    target.headSha,
  );
  if (emission) backgroundWaitUntil(emission);
  const prNumbers =
    target.prNumbers.length > 0
      ? target.prNumbers
      : await fetchPrNumbersForCommit(ctx, repo.owner, repo.repo, target.headSha);
  await Promise.all(
    prNumbers.map((number) =>
      processCi(ctx, repo.owner, repo.repo, number, target.headSha, false, evaluation),
    ),
  );
}

async function processCi(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  number: number,
  headSha: string,
  force = false,
  knownEvaluation?: CiEvaluation | null,
): Promise<void> {
  const pr = await fetchPr(ctx, owner, repo, number);
  if (!pr || !owns(ctx, pr)) return;
  // Ignore CI for a SHA that's already been superseded by a newer push.
  if (pr.headSha !== headSha) return;

  const evaluation =
    knownEvaluation ?? (await fetchCiEvaluation(ctx, owner, repo, headSha));
  if (!evaluation?.settled) return; // wait until *all* checks are done (and readable).
  const prefix = ctx.options.stateKeyPrefix ?? "centaur-githubbot";
  const trace = makeTrace(managementThreadKey(owner, repo, number), `ci-${headSha}`);

  // A red PR bound to a Slack session never gets an automated CI turn, and its
  // report must not depend on the shared settled claim (a green result or an
  // early skip may already have consumed it). Only an explicit 404 means
  // unbound: if api-rs can't say, do nothing at all, and take no claim.
  if (evaluation.failed) {
    const lookup = await lookupBinding(ctx, owner, repo, number);
    if (!lookup.ok) return;
    if (lookup.binding) {
      await reportBoundCiFailure(ctx, owner, repo, number, headSha, lookup.binding, evaluation, trace);
      return;
    }
  }

  // Act once per fully-settled SHA (the last-arriving check event wins).
  const firstSettle = await claim(ctx, `${prefix}:ci-settled:${owner}/${repo}#${number}:${headSha}`);
  if (!evaluation.failed) {
    // Green: reset the fix counter and consider merging. The merge gate is
    // deterministic and has its own claim, so it runs on every green settle:
    // a check that finishes later on the same SHA (a rerun, or one triggered
    // by an approval) or a re-assignment can be what makes the PR mergeable.
    if (firstSettle) {
      const state = await loadState(ctx, owner, repo, number);
      if (state.consecutiveCiFixes) {
        await saveState(ctx, owner, repo, number, { ...state, consecutiveCiFixes: 0 });
      }
      traceLog(ctx.options, "githubbot_ci_green", trace, { pr: `${owner}/${repo}#${number}` });
    }
    await tryMerge(ctx, owner, repo, number);
    return;
  }
  if (!firstSettle) return;

  // Red: back off if a human pushed the failing commit (don't step on them) —
  // unless this is a forced takeover (the PR was just assigned to us, so the
  // human has explicitly handed it over and we fix it regardless of who pushed).
  if (!force) {
    const headAuthor = await commitAuthor(ctx, owner, repo, headSha);
    if (headAuthor && headAuthor.toLowerCase() !== ctx.userName.toLowerCase()) {
      traceLog(ctx.options, "githubbot_ci_human_commit_skipped", trace, {
        author: headAuthor,
      });
      return;
    }
  }

  const maxAttempts = ctx.options.ciFixMaxAttempts ?? DEFAULT_CI_FIX_MAX_ATTEMPTS;
  const state = await loadState(ctx, owner, repo, number);
  const attempts = state.consecutiveCiFixes ?? 0;
  if (attempts >= maxAttempts) {
    await escalate(ctx, owner, repo, number, evaluation.failingNames, maxAttempts);
    return;
  }
  await saveState(ctx, owner, repo, number, {
    ...state,
    consecutiveCiFixes: attempts + 1,
  });
  fireCiFixTurn(ctx, owner, repo, pr, evaluation.failingNames, attempts + 1, maxAttempts);
}

/**
 * Report a red head on a bound PR. Eligibility comes first: the binding is for
 * this exact PR and managed and not closed, and a fresh read still shows the
 * failing head. Nothing is claimed locally; api-rs dedupes the notice.
 */
async function reportBoundCiFailure(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  number: number,
  headSha: string,
  binding: PrBinding,
  evaluation: CiEvaluation,
  trace: GithubbotTrace,
): Promise<void> {
  const prLabel = `${owner}/${repo}#${number}`;
  const skip = (reason: string) =>
    traceLog(ctx.options, "githubbot_bound_ci_skipped", trace, { pr: prLabel, reason });
  if (!binding.managed) return skip("not_managed");
  if (binding.closed) return skip("closed");
  // api-rs revalidates and posts inside the report request, so its inputs must
  // be fresh: this is the last GitHub read before the report, and it fails
  // closed (no read, no report, and no claim has been taken yet).
  const fresh = await fetchPr(ctx, owner, repo, number);
  if (!fresh) return skip("pr_unreadable");
  if (!fresh.nodeId || fresh.nodeId !== binding.pr_node_id) return skip("identity_mismatch");
  if (!fresh.updatedAt) return skip("pr_unreadable");
  if (fresh.headSha !== headSha) return skip("stale_head");

  // The failing head is confirmed current. If the binding still records an
  // older one (CI beat the synchronize event), record it first so api-rs
  // doesn't refuse the report as stale.
  if (binding.head_sha !== fresh.headSha) {
    const recorded = await writeBindingState(
      ctx,
      bindingRef(owner, repo, number),
      {
        pr_node_id: fresh.nodeId,
        observed_updated_at: fresh.updatedAt,
        head_sha: fresh.headSha,
      },
      false,
      "ci_head_sync",
    );
    if (!recorded) return skip("head_not_recorded");
  }

  // open and assigned come from the fresh read; api-rs decides and records. It
  // is also the only dedupe authority, keyed by (pr_node_id, head_sha): a
  // repeat delivery is answered `duplicate`, so githubbot keeps no claim here.
  try {
    const result = await reportPrCiFailure(ctx.options, bindingRef(owner, repo, number), {
      pr_node_id: fresh.nodeId,
      observed_updated_at: fresh.updatedAt,
      head_sha: headSha,
      current_head_sha: fresh.headSha,
      open: fresh.state === "open" && !fresh.merged,
      assigned: owns(ctx, fresh),
      check_name: evaluation.failingNames[0] ?? "CI checks",
      ...(evaluation.firstFailingUrl ? { details_url: evaluation.firstFailingUrl } : {}),
      html_url: fresh.htmlUrl ?? `https://github.com/${owner}/${repo}/pull/${number}`,
    });
    // `posted` and `duplicate` are both done; a skip is api-rs's decision.
    traceLog(ctx.options, "githubbot_bound_ci_reported", trace, {
      notice: result.notice,
      pr: prLabel,
      ...(result.reason ? { reason: result.reason } : {}),
    });
  } catch (error) {
    logger(ctx).warn("githubbot_bound_ci_report_failed", {
      error: errorMessage(error),
      pr: prLabel,
    });
  }
}

/** Deterministic merge gate — no agent; GitHub's mergeable_state decides. */
async function tryMerge(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  number: number,
): Promise<void> {
  const pr = await fetchPr(ctx, owner, repo, number);
  if (!pr || !owns(ctx, pr)) return;
  const decision = decideMerge({
    autoMerge: ctx.options.autoMerge !== false,
    draft: pr.draft,
    holdLabel: ctx.options.holdLabel ?? "do-not-merge",
    labels: pr.labels,
    merged: pr.merged,
    mergeableState: pr.mergeableState,
    state: pr.state,
  });
  const trace = makeTrace(managementThreadKey(owner, repo, number), `merge-${pr.headSha}`);
  traceLog(ctx.options, "githubbot_merge_decision", trace, {
    decision,
    mergeable_state: pr.mergeableState,
    pr: `${owner}/${repo}#${number}`,
  });

  if (decision === "merge") {
    // The claim guards against two concurrent lifecycle events both calling
    // merge. It's released on failure (below) so a transient merge error — "Base
    // branch was modified", a secondary rate limit, a 5xx — is retried on the
    // next event instead of leaving a clean, approved PR permanently unmerged
    // behind a stale claim. On success the claim stays as the "merged" marker.
    const mergedClaimKey = `${ctx.options.stateKeyPrefix ?? "centaur-githubbot"}:merged:${owner}/${repo}#${number}:${pr.headSha}`;
    if (!(await claim(ctx, mergedClaimKey))) {
      return;
    }
    try {
      await ctx.octokit.rest.pulls.merge({
        owner,
        repo,
        pull_number: number,
        merge_method: ctx.options.mergeMethod ?? "squash",
      });
      traceLog(ctx.options, "githubbot_merged", trace, { pr: `${owner}/${repo}#${number}` });
      if (
        ctx.options.deleteBranchOnMerge !== false &&
        pr.headRepoFullName?.toLowerCase() === `${owner}/${repo}`.toLowerCase()
      ) {
        try {
          await ctx.octokit.rest.git.deleteRef({
            owner,
            repo,
            ref: `heads/${pr.headRef}`,
          });
        } catch (error) {
          logger(ctx).debug("githubbot_branch_delete_failed", {
            error: errorMessage(error),
          });
        }
      }
    } catch (error) {
      // Re-merging an already-merged PR is a no-op (decideMerge returns
      // skip_closed next time), so releasing on any failure is safe.
      await release(ctx, mergedClaimKey);
      logger(ctx).warn("githubbot_merge_failed", {
        error: errorMessage(error),
        pr: `${owner}/${repo}#${number}`,
      });
    }
    return;
  }
  if (decision === "update_branch") {
    try {
      await ctx.octokit.rest.pulls.updateBranch({ owner, repo, pull_number: number });
    } catch (error) {
      logger(ctx).debug("githubbot_update_branch_failed", {
        error: errorMessage(error),
      });
    }
    return;
  }
  if (decision === "resolve_conflict") {
    fireConflictTurn(ctx, owner, repo, pr);
  }
}

async function escalate(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  number: number,
  failingNames: string[],
  maxAttempts: number,
): Promise<void> {
  const handle = ctx.options.escalationHandle?.replace(/^@/, "");
  const mention = handle ? `@${handle} ` : "";
  const checks = failingNames.length ? failingNames.join(", ") : "the CI checks";
  const body =
    `${mention}I've tried to fix CI on this PR ${maxAttempts} times without ` +
    `success and am pausing automatic fixes. Still failing: ${checks}. ` +
    `Could a human take a look?`;
  try {
    await ctx.octokit.rest.issues.createComment({
      owner,
      repo,
      issue_number: number,
      body,
    });
    traceLog(
      ctx.options,
      "githubbot_ci_escalated",
      makeTrace(managementThreadKey(owner, repo, number), `escalate-${number}`),
      { pr: `${owner}/${repo}#${number}` },
    );
  } catch (error) {
    logger(ctx).warn("githubbot_escalation_failed", {
      error: errorMessage(error),
    });
  }
}

// ---------------------------------------------------------------------------
// Agentic turns (run on the management thread; the agent does GitHub I/O via gh).
// ---------------------------------------------------------------------------

function fireCiFixTurn(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  pr: PullRequestSummary,
  failingNames: string[],
  attempt: number,
  maxAttempts: number,
): void {
  const handle = ctx.options.escalationHandle?.replace(/^@/, "");
  const fallback = handle
    ? `if you can't tell, @-mention @${handle}`
    : "if you can't tell, @-mention a maintainer";
  const preamble =
    `CI failed on pull request ${owner}/${repo}#${pr.number} at commit ` +
    `${pr.headSha}. Failing checks: ${failingNames.join(", ") || "unknown"}.\n\n` +
    `Fix it in your sandbox:\n` +
    `- Pull the failing logs (e.g. \`gh pr checks ${pr.number}\`, ` +
    `\`gh run view <run-id> --log-failed\`), understand the failure, fix it, and ` +
    `push to the PR's head branch (${pr.headRef}).\n` +
    `- If a check is flaky (infra/timeout, not your code), you may re-run it once ` +
    `instead of changing code.\n` +
    `- If you cannot confidently fix it, do NOT push a guess. Post a comment on ` +
    `the PR summarizing what's failing and what you tried, and @-mention the right ` +
    `human — find them via \`git blame\` on the affected files, recent authors ` +
    `(\`git log\`), or GitHub's suggested reviewers; ${fallback}.\n\n` +
    `This is fix attempt ${attempt} of ${maxAttempts}.`;
  fireManagementTurn(ctx, owner, repo, pr, preamble, {
    id: `fix-${owner}/${repo}#${pr.number}-${pr.headSha}-${attempt}`,
    label: "ci-fix",
    text: `Fix the failing CI on ${owner}/${repo}#${pr.number}.`,
  });
}

function fireAddressReviewTurn(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  pr: PullRequestSummary,
  review: { reviewer: string; reviewId: number; reviewNodeId?: string },
): void {
  const { reviewer, reviewId, reviewNodeId } = review;
  const preamble =
    `A review was submitted on pull request ${owner}/${repo}#${pr.number} ` +
    `(head ${pr.headSha}). Address it as the PR author, working in your sandbox:\n` +
    `- Read all of the feedback: \`gh pr view ${pr.number} --comments\` and the ` +
    `pull-request review-comments API.\n` +
    `- Make the changes you agree with in a single coherent commit and push to ` +
    `${pr.headRef}.\n` +
    `- Reply to each review thread saying what you changed; where you disagree, ` +
    `explain why, briefly and respectfully. Resolve the threads you've addressed.\n` +
    `- Re-request review from @${reviewer} once you've pushed.\n` +
    `- If a request is unclear or you can't address it, say so in the thread and ask.`;
  fireManagementTurn(
    ctx,
    owner,
    repo,
    pr,
    preamble,
    {
      id: `review-resp-${owner}/${repo}#${pr.number}-${reviewId}`,
      label: "address-review",
      text: `Address the review on ${owner}/${repo}#${pr.number} from @${reviewer}.`,
    },
    reviewNodeId,
  );
}

function fireConflictTurn(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  pr: PullRequestSummary,
): void {
  const preamble =
    `Pull request ${owner}/${repo}#${pr.number} has merge conflicts with its ` +
    `base branch. In your sandbox, update ${pr.headRef} against the base (rebase ` +
    `or merge), resolve the conflicts correctly, and push. If the conflicts are ` +
    `non-trivial or you're unsure of the right resolution, stop and @-mention a ` +
    `human instead of force-pushing a guess.`;
  fireManagementTurn(ctx, owner, repo, pr, preamble, {
    id: `conflict-${owner}/${repo}#${pr.number}-${pr.headSha}`,
    label: "resolve-conflict",
    text: `Resolve the merge conflicts on ${owner}/${repo}#${pr.number}.`,
  });
}

function fireManagementTurn(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  pr: PullRequestSummary,
  preamble: string,
  message: { id: string; label: string; text: string },
  reviewNodeId?: string,
): void {
  const threadKey = managementThreadKey(owner, repo, pr.number);
  const trace = makeTrace(threadKey, message.id);
  // A deployment can prepend its own constraints to the management methodology
  // (the per-action preamble still rides underneath).
  const guidance = ctx.options.managementPrompt;
  const contextPreamble = guidance ? `${guidance}\n\n${preamble}` : preamble;
  let lastEventId = 0;
  const forwardInput: ForwardSessionInput = {
    afterEventId: 0,
    contextPreamble,
    conversationName: `${owner}/${repo}#${pr.number}: ${pr.title}`,
    executeMessage: managementMessage(message.id, threadKey, message.text),
    messages: [],
    model: undefined,
    onEventId: (eventId) => {
      lastEventId = Math.max(lastEventId, eventId);
      forwardInput.afterEventId = lastEventId;
    },
    openStream: false,
    threadId: threadKey,
    trace,
  };
  traceLog(ctx.options, "githubbot_management_turn_started", trace, {
    pr: `${owner}/${repo}#${pr.number}`,
    work: message.label,
  });
  // Review-triggered turns ack on the reviewer's own review — instant 👀,
  // settled to 🚀/😕 when the turn finishes (same lifecycle as @-mention acks).
  // Not awaited: the ack must not delay the turn, and a failed reaction is only
  // a missing ack. Turns with no triggering review (CI-fix, conflicts) stay
  // silent — a reaction on the PR's top post isn't clearly tied to anything.
  if (reviewNodeId) {
    void reactWorkingOnReview(ctx.octokit, reviewNodeId, logger(ctx));
  }
  backgroundWaitUntil(
    runTurnStream(ctx.options, forwardInput)
      .then(async (result) => {
        traceLog(ctx.options, "githubbot_management_turn_complete", trace, {
          chars: turnOutputChars(result),
          failed: result.failed,
          work: message.label,
        });
        if (reviewNodeId) {
          await settleReviewReaction(
            ctx.octokit,
            reviewNodeId,
            result.failed,
            logger(ctx),
          );
        }
      })
      .catch(async (error) => {
        logger(ctx).warn("githubbot_management_turn_failed", {
          error: errorMessage(error),
          work: message.label,
        });
        if (reviewNodeId) {
          await settleReviewReaction(ctx.octokit, reviewNodeId, true, logger(ctx));
        }
      }),
  );
}

function managementMessage(
  id: string,
  threadKey: string,
  text: string,
): GithubbotApiMessage {
  return {
    attachments: [],
    author: {
      fullName: "GitHub",
      isBot: false,
      isMe: false,
      userId: "github-pr-manager",
      userName: "github-pr-manager",
    },
    id,
    isMention: true,
    raw: { githubbotManagement: true },
    text,
    threadId: threadKey,
    timestamp: new Date().toISOString(),
  };
}

// ---------------------------------------------------------------------------
// GitHub API reads.
// ---------------------------------------------------------------------------

async function commitAuthor(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  sha: string,
): Promise<string | undefined> {
  try {
    const { data } = await ctx.octokit.rest.repos.getCommit({ owner, repo, ref: sha });
    return data.author?.login ?? undefined;
  } catch {
    return undefined;
  }
}

async function fetchPrNumbersForCommit(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  sha: string,
): Promise<number[]> {
  try {
    const { data } =
      await ctx.octokit.rest.repos.listPullRequestsAssociatedWithCommit({
        owner,
        repo,
        commit_sha: sha,
      });
    return data.map((pr) => pr.number).filter((n) => typeof n === "number");
  } catch (error) {
    logger(ctx).debug("githubbot_commit_prs_fetch_failed", {
      error: errorMessage(error),
      ref: `${owner}/${repo}@${sha}`,
    });
    return [];
  }
}

// ---------------------------------------------------------------------------
// Payload parsing helpers.
// ---------------------------------------------------------------------------

type JsonRecord = Record<string, unknown>;

function isRecord(value: unknown): value is JsonRecord {
  return Boolean(value && typeof value === "object" && !Array.isArray(value));
}

function parseJson(rawBody: string): JsonRecord | null {
  try {
    const value = JSON.parse(rawBody);
    return isRecord(value) ? value : null;
  } catch {
    return null;
  }
}

function numberValue(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function repoFromPayload(
  payload: JsonRecord,
): { owner: string; repo: string } | null {
  const repository = payload.repository;
  if (!isRecord(repository)) return null;
  const fullName = stringValue(repository.full_name);
  if (!fullName) return null;
  const [owner, repo] = fullName.split("/", 2);
  if (!owner || !repo) return null;
  return { owner, repo };
}

function ciTarget(
  eventType: string,
  payload: JsonRecord,
): { headSha: string; prNumbers: number[] } | null {
  if (eventType === "status") {
    const headSha = stringValue(payload.sha);
    return headSha ? { headSha, prNumbers: [] } : null;
  }
  const node =
    eventType === "check_run"
      ? payload.check_run
      : eventType === "check_suite"
        ? payload.check_suite
        : eventType === "workflow_run"
          ? payload.workflow_run
          : undefined;
  if (!isRecord(node)) return null;
  const headSha = stringValue(node.head_sha);
  if (!headSha) return null;
  const prs = node.pull_requests;
  const prNumbers: number[] = [];
  if (Array.isArray(prs)) {
    for (const pr of prs) {
      const n = isRecord(pr) ? numberValue(pr.number) : undefined;
      if (n !== undefined) prNumbers.push(n);
    }
  }
  return { headSha, prNumbers };
}

function makeTrace(threadKey: string, messageId: string): GithubbotTrace {
  return {
    includeContext: false,
    messageId,
    mode: "execute",
    openStream: true,
    startedAtMs: nowMs(),
    threadId: threadKey,
  };
}
