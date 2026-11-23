import { createHash } from "node:crypto";
import { buildCommentReplyBody } from "./comment-bot";
import { backgroundWaitUntil, runExclusive } from "./context";
import {
  fetchPr,
  isOwnedPr,
  type PrManagerContext,
  type PullRequestSummary,
} from "./pr-manager";
import { reactWorkingOnReview, settleReviewReaction } from "./reactions";
import {
  dispatchPrFollowup,
  enqueuePrFollowup,
  getPrBinding,
  isRetryableSessionApiError,
  SessionApiError,
  streamPrFollowupEvents,
  updatePrBindingState,
  verifyPrBinding,
  type PrBindingRef,
} from "./session-api";
import { collectTurnOutput, type TurnResult } from "./turn";
import type {
  GithubbotOptions,
  GithubbotRendererSource,
  PrBinding,
  PrBindingStateUpdate,
  PrDispatchRequest,
  PrDispatchResult,
  PrFollowup,
  PrFollowupInput,
  PrFollowupKind,
  PrVerification,
} from "./types";
import { errorMessage, noopLogger, stringValue, traceLog } from "./utils";

/**
 * PR session continuity (v1). A PR the Centaur sandbox opened and registered
 * (`centaur_sdk.register_pull_request`) is verified here from a signed webhook
 * plus a fresh GitHub fetch. Once the bot is explicitly assigned, follow-ups
 * (reviews, mentions, red CI, conflicts) go to api-rs's durable per-binding
 * queue and are dispatched into the exact session that opened the PR, never a
 * `github-manage:` session. api-rs owns the queue, dedupe and dispatch; this
 * module enqueues, rechecks GitHub immediately before each dispatch, and
 * renders replies for direct GitHub requests.
 *
 * Push authorization is prepare-and-ask only: no path here pushes, dispatched
 * turns carry the prepare-and-ask preamble, and a recorded push grant is
 * stored inactive and confers nothing.
 */

export type PrBindingClient = {
  get(ref: PrBindingRef): Promise<PrBinding | null>;
  verify(ref: PrBindingRef, verification: PrVerification): Promise<PrBinding>;
  updateState(ref: PrBindingRef, update: PrBindingStateUpdate): Promise<PrBinding>;
  enqueue(
    ref: PrBindingRef,
    followup: PrFollowupInput,
  ): Promise<{ followup: PrFollowup; created: boolean }>;
  dispatch(ref: PrBindingRef, request: PrDispatchRequest): Promise<PrDispatchResult>;
  streamEvents(
    ref: PrBindingRef,
    executionId: string,
    afterEventId: number,
    onEventId: (eventId: number) => void,
  ): Promise<AsyncIterable<GithubbotRendererSource>>;
};

export function createPrBindingClient(options: GithubbotOptions): PrBindingClient {
  return {
    get: (ref) => getPrBinding(options, ref),
    verify: (ref, verification) => verifyPrBinding(options, ref, verification),
    updateState: (ref, update) => updatePrBindingState(options, ref, update),
    enqueue: (ref, followup) => enqueuePrFollowup(options, ref, followup),
    dispatch: (ref, request) => dispatchPrFollowup(options, ref, request),
    streamEvents: (ref, executionId, afterEventId, onEventId) =>
      streamPrFollowupEvents(options, ref, executionId, afterEventId, onEventId),
  };
}

export const PR_FOLLOWUP_STATUS_PREFIX = "PR-FOLLOWUP-STATUS:";
const DEFAULT_DISPATCH_INTERVAL_MS = 30_000;
const LOOKUP_ATTEMPTS = 3;
const RENDER_ATTEMPTS = 3;
const CLAIM_TTL_MS = 7 * 24 * 60 * 60 * 1000;
const PROMPT_TEXT_MAX_CHARS = 8_000;
const DIFF_HUNK_MAX_CHARS = 4_000;

// ---------------------------------------------------------------------------
// Binding resolution.
// ---------------------------------------------------------------------------

/**
 * How githubbot treats one PR:
 *  - `unbound`: no binding, or a registration that failed verification. The
 *    existing GitHub-only flow runs unchanged.
 *  - `bound`: a verified binding. Never routed to `github-manage:`.
 *  - `unknown`: the binding could not be read or verified. Fail closed: no
 *    agent work for this event, since githubbot can't prove the PR is unbound.
 */
export type BindingRoute =
  | { kind: "unbound" }
  | { kind: "bound"; binding: PrBinding; ref: PrBindingRef }
  | { kind: "unknown" };

export function bindingRef(owner: string, repo: string, number: number): PrBindingRef {
  return { repository: `${owner}/${repo}`.toLowerCase(), number };
}

export async function resolveBindingRoute(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  number: number,
): Promise<BindingRoute> {
  const client = ctx.bindings;
  if (!client) return { kind: "unbound" };
  const ref = bindingRef(owner, repo, number);
  let binding: PrBinding | null;
  try {
    binding = await withRetry(() => client.get(ref));
  } catch (error) {
    logger(ctx).warn("githubbot_pr_binding_lookup_failed", {
      error: errorMessage(error),
      pr: prLabel(ref),
    });
    return { kind: "unknown" };
  }
  if (!binding) return { kind: "unbound" };
  if (binding.state === "registered") {
    // Verify whenever a registration is seen, not only on opened/edited: the
    // sandbox registers after `gh pr create` returns, so the opened webhook
    // usually arrives first. Every caller runs after a signed webhook.
    const verified = await verifyRegisteredBinding(ctx, owner, repo, number, ref);
    if (!verified) return { kind: "unknown" };
    binding = verified;
  }
  if (binding.state !== "verified") return { kind: "unbound" };
  return { kind: "bound", binding, ref };
}

async function verifyRegisteredBinding(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  number: number,
  ref: PrBindingRef,
): Promise<PrBinding | null> {
  const pr = await fetchPr(ctx, owner, repo, number);
  if (!pr?.nodeId || !pr.authorLogin || !pr.createdAt) {
    logger(ctx).warn("githubbot_pr_binding_verify_unreadable", { pr: prLabel(ref) });
    return null;
  }
  try {
    const binding = await ctx.bindings!.verify(ref, {
      node_id: pr.nodeId,
      author_login: pr.authorLogin,
      created_at: pr.createdAt,
      head_ref: pr.headRef,
      head_sha: pr.headSha,
    });
    traceLog(ctx.options, "githubbot_pr_binding_verified", undefined, {
      pr: prLabel(ref),
      state: binding.state,
    });
    return binding;
  } catch (error) {
    logger(ctx).warn("githubbot_pr_binding_verify_failed", {
      error: errorMessage(error),
      pr: prLabel(ref),
    });
    return null;
  }
}

// ---------------------------------------------------------------------------
// pull_request lifecycle for bound PRs.
// ---------------------------------------------------------------------------

/**
 * What `handlePullRequestEvent` does after the bound-PR handler ran:
 *  - `legacy`: unbound, run the existing flow unchanged;
 *  - `stop`: nothing more for this event;
 *  - `evaluate_ci`: the bot was just activated, evaluate CI on that head (red
 *    enqueues a follow-up; green reaches the unchanged merge gate);
 *  - `merge_gate`: run the unchanged deterministic merge gate.
 */
export type BoundPullRequestOutcome =
  | { next: "legacy" }
  | { next: "stop" }
  | { next: "evaluate_ci"; headSha: string }
  | { next: "merge_gate" };

export async function handleBoundPullRequestEvent(
  ctx: PrManagerContext,
  payload: Record<string, unknown>,
  action: string,
  owner: string,
  repo: string,
  number: number,
): Promise<BoundPullRequestOutcome> {
  const route = await resolveBindingRoute(ctx, owner, repo, number);
  if (route.kind === "unbound") return { next: "legacy" };
  // Unreadable binding: no agent work, but the deterministic merge gate is
  // safe either way (its conflict branch re-resolves and fails closed too).
  if (route.kind === "unknown") return { next: "merge_gate" };
  const { binding, ref } = route;
  const botLogin = ctx.userName.toLowerCase();
  const eventAssignee = loginOf(payload.assignee)?.toLowerCase();

  if (action === "closed") {
    await updateState(ctx, ref, { closed: true });
    await clearPending(ctx, ref);
    return { next: "stop" };
  }
  if (action === "assigned" && eventAssignee === botLogin) {
    // Explicit assignment of the bot (self-assignment at a user's request is
    // the same event) activates management for the bound session. No
    // `github-manage:` session is created.
    const pr = await fetchPr(ctx, owner, repo, number);
    if (!pr) return { next: "stop" };
    const updated = await updateState(ctx, ref, { managed: true, head_sha: pr.headSha });
    if (!updated?.managed) return { next: "stop" };
    traceLog(ctx.options, "githubbot_pr_binding_managed", undefined, { pr: prLabel(ref) });
    return { next: "evaluate_ci", headSha: pr.headSha };
  }
  if (action === "unassigned" && eventAssignee === botLogin) {
    await updateState(ctx, ref, { managed: false });
    await clearPending(ctx, ref);
    return { next: "stop" };
  }
  if (action === "synchronize" && binding.managed) {
    await handleBoundPush(ctx, payload, binding, ref, owner, repo, number);
  }
  return binding.managed ? { next: "merge_gate" } : { next: "stop" };
}

/**
 * v1 has no automated pushes, so any push to a managed bound PR is from
 * outside the authorized flow: pause automation and post one note. Pushes by
 * githubbot itself (the deterministic branch update) are not outside pushes.
 */
async function handleBoundPush(
  ctx: PrManagerContext,
  payload: Record<string, unknown>,
  binding: PrBinding,
  ref: PrBindingRef,
  owner: string,
  repo: string,
  number: number,
): Promise<void> {
  const prNode = isRecord(payload.pull_request) ? payload.pull_request : {};
  const headSha =
    stringValue(payload.after) ??
    stringValue(isRecord(prNode.head) ? prNode.head.sha : undefined);
  const pusher = loginOf(payload.sender);
  const outside = !pusher || pusher.toLowerCase() !== ctx.userName.toLowerCase();
  if (!outside || binding.automation_paused) {
    if (headSha) await updateState(ctx, ref, { head_sha: headSha });
    return;
  }
  const reason = `outside push${pusher ? ` by @${pusher}` : ""}${headSha ? ` at ${headSha.slice(0, 12)}` : ""}`;
  const updated = await updateState(ctx, ref, {
    ...(headSha ? { head_sha: headSha } : {}),
    automation_paused: { reason },
  });
  if (!updated) return;
  if (!(await claimOnce(ctx, `pr-paused:${prLabel(ref)}:${headSha ?? "unknown"}`))) return;
  await postIssueComment(
    ctx,
    owner,
    repo,
    number,
    `Paused automated follow-ups on this PR: ${reason} is outside Centaur's authorized flow. ` +
      `Queued follow-ups stay queued. An authorized maintainer can comment \`@${ctx.userName} resume\` to continue.`,
  );
}

// ---------------------------------------------------------------------------
// Follow-up construction and enqueue.
// ---------------------------------------------------------------------------

export type FollowupActor = { github_user_id: string; github_login: string };

/** The webhook-signed actor, or undefined when the id isn't a numeric user id. */
export function actorFrom(user: unknown): FollowupActor | undefined {
  if (!isRecord(user)) return undefined;
  const login = stringValue(user.login);
  const id =
    typeof user.id === "number" && Number.isSafeInteger(user.id)
      ? String(user.id)
      : stringValue(user.id);
  if (!login || !id || !/^\d+$/.test(id)) return undefined;
  return { github_user_id: id, github_login: login };
}

export type FollowupDraft = {
  kind: PrFollowupKind;
  dedupeKey: string;
  headSha: string;
  actor: FollowupActor;
  /** Kind-specific request text, appended after the prepare-and-ask preamble. */
  request: string;
  replyTarget: PrFollowupInput["reply_target"];
  /** Node id of the triggering review/comment, for the 👀 → 🚀/😕 ack. */
  ackNodeId?: string;
};

/**
 * Queue one follow-up for a managed bound PR and, when it's new, drive the
 * dispatcher. Repeat deliveries of the same logical item are no-ops (api-rs
 * dedupes on `dedupe_key`). Returns whether a new item was queued.
 */
export async function enqueueBoundFollowup(
  ctx: PrManagerContext,
  route: Extract<BindingRoute, { kind: "bound" }>,
  draft: FollowupDraft,
): Promise<boolean> {
  const { binding, ref } = route;
  if (!binding.managed) {
    traceLog(ctx.options, "githubbot_pr_followup_unmanaged_skipped", undefined, {
      kind: draft.kind,
      pr: prLabel(ref),
    });
    return false;
  }
  const input: PrFollowupInput = {
    kind: draft.kind,
    dedupe_key: draft.dedupeKey,
    head_sha: draft.headSha,
    actor: draft.actor,
    prompt: [{ type: "text", text: followupPromptText(ctx, binding, draft.request) }],
    reply_target: draft.replyTarget,
  };
  let created: boolean;
  let followupId: string;
  try {
    const result = await ctx.bindings!.enqueue(ref, input);
    created = result.created;
    followupId = result.followup.id;
  } catch (error) {
    logger(ctx).warn("githubbot_pr_followup_enqueue_failed", {
      error: errorMessage(error),
      kind: draft.kind,
      pr: prLabel(ref),
      status: error instanceof SessionApiError ? error.status : undefined,
    });
    return false;
  }
  traceLog(ctx.options, "githubbot_pr_followup_enqueued", undefined, {
    created,
    dedupe_key: draft.dedupeKey,
    kind: draft.kind,
    pr: prLabel(ref),
  });
  if (!created) return false;
  if (draft.ackNodeId) {
    void reactWorkingOnReview(ctx.octokit, draft.ackNodeId, ctx.options.logger);
    await setState(ctx, `pr-followup-ack:${followupId}`, draft.ackNodeId);
  }
  await markPending(ctx, ref);
  requestDispatch(ctx, ref);
  return true;
}

/**
 * Text that leads every dispatched follow-up turn: v1 prepare-and-ask push
 * authorization plus the status line Slack keys its brief update on.
 */
export function prepareAndAskPreamble(binding: PrBinding): string {
  const subject = `${binding.repository}#${binding.number}`;
  const branch = binding.head_ref ? ` (branch \`${binding.head_ref}\`)` : "";
  return (
    `[PR follow-up] This turn comes from GitHub activity on ${subject}${branch}, ` +
    `a pull request this session opened. It is not a chat message.\n` +
    `Push authorization is prepare-and-ask: prepare and commit the change on the PR ` +
    `branch in your sandbox, but do not push, force-push, merge, or change the PR's ` +
    `branch on GitHub. Ask once for an authorized human to push, naming the branch ` +
    `and what the change does.\n` +
    `End your final message with exactly one line: \`${PR_FOLLOWUP_STATUS_PREFIX} needs-push\` ` +
    `when a prepared change is waiting on a human push, \`${PR_FOLLOWUP_STATUS_PREFIX} blocked\` ` +
    `when you cannot proceed, or \`${PR_FOLLOWUP_STATUS_PREFIX} done\` when nothing needs pushing.`
  );
}

function followupPromptText(
  ctx: PrManagerContext,
  binding: PrBinding,
  request: string,
): string {
  const guidance = ctx.options.managementPrompt;
  const preamble = prepareAndAskPreamble(binding);
  return [guidance, preamble, truncate(request, PROMPT_TEXT_MAX_CHARS)]
    .filter(Boolean)
    .join("\n\n");
}

export function reviewFollowup(input: {
  owner: string;
  repo: string;
  number: number;
  reviewId: number;
  reviewNodeId?: string;
  reviewState: string;
  reviewBody?: string;
  headSha: string;
  actor: FollowupActor;
}): FollowupDraft {
  const subject = `${input.owner}/${input.repo}#${input.number}`;
  const body = input.reviewBody?.trim();
  return {
    kind: "review",
    dedupeKey: `review:${input.reviewId}`,
    headSha: input.headSha,
    actor: input.actor,
    request:
      `@${input.actor.github_login} submitted a review (${input.reviewState}) on ${subject} ` +
      `at head ${input.headSha}. Read all of the feedback (\`gh pr view ${input.number} --comments\` ` +
      `and the review-comments API) and prepare the changes you agree with. Your final message ` +
      `is posted on the PR as the reply to this review.` +
      (body ? `\n\nReview body:\n${body}` : ""),
    replyTarget: {
      type: "github_comment",
      repository: `${input.owner}/${input.repo}`.toLowerCase(),
      number: input.number,
      review_id: input.reviewId,
    },
    ackNodeId: input.reviewNodeId,
  };
}

/** A comment that mentions the bot on a managed bound PR. */
export function commentFollowup(input: {
  owner: string;
  repo: string;
  number: number;
  raw: unknown;
  text: string;
  headSha: string;
}): FollowupDraft | undefined {
  if (!isRecord(input.raw) || !isRecord(input.raw.comment)) return undefined;
  const comment = input.raw.comment;
  const commentId = typeof comment.id === "number" ? comment.id : undefined;
  const actor = actorFrom(comment.user);
  if (commentId === undefined || !actor) return undefined;
  const subject = `${input.owner}/${input.repo}#${input.number}`;
  const isReviewComment = input.raw.type === "review_comment";
  // Replies go to a review thread's root comment; GitHub rejects replies to
  // replies. Dedupe stays on the comment itself.
  const rootId =
    isReviewComment && typeof comment.in_reply_to_id === "number"
      ? comment.in_reply_to_id
      : commentId;
  let location = "";
  if (isReviewComment) {
    const path = stringValue(comment.path);
    const line = typeof comment.line === "number" ? ` line ${comment.line}` : "";
    const hunk = stringValue(comment.diff_hunk);
    location =
      (path ? ` on \`${path}\`${line}` : "") +
      (hunk
        ? `\n\nThe diff hunk it is anchored to:\n\n\`\`\`diff\n${truncate(hunk, DIFF_HUNK_MAX_CHARS)}\n\`\`\``
        : "");
  }
  return {
    kind: isReviewComment ? "review_comment" : "issue_comment",
    dedupeKey: `comment:${commentId}`,
    headSha: input.headSha,
    actor,
    request:
      `@${actor.github_login} commented${isReviewComment ? " in a review thread" : ""} on ${subject}` +
      `${location}\n\nTheir comment:\n${input.text.trim()}\n\n` +
      `Your final message is posted back as the reply to this comment.`,
    replyTarget: {
      type: "github_comment",
      repository: `${input.owner}/${input.repo}`.toLowerCase(),
      number: input.number,
      comment_id: rootId,
    },
    ackNodeId: stringValue(comment.node_id),
  };
}

export function ciFailureFollowup(input: {
  owner: string;
  repo: string;
  number: number;
  headSha: string;
  failingNames: string[];
  actor: FollowupActor;
}): FollowupDraft {
  const subject = `${input.owner}/${input.repo}#${input.number}`;
  return {
    kind: "ci_failure",
    dedupeKey: `ci:${input.headSha}:failure`,
    headSha: input.headSha,
    actor: input.actor,
    request:
      `CI failed on ${subject} at ${input.headSha}. Failing checks: ` +
      `${input.failingNames.join(", ") || "unknown"}. Pull the failing logs ` +
      `(\`gh pr checks ${input.number}\`, \`gh run view <run-id> --log-failed\`), find the ` +
      `cause, and prepare a fix. If a check is flaky rather than broken, say so instead of ` +
      `changing code.`,
    replyTarget: null,
  };
}

export function conflictFollowup(input: {
  owner: string;
  repo: string;
  number: number;
  headSha: string;
  actor: FollowupActor;
}): FollowupDraft {
  const subject = `${input.owner}/${input.repo}#${input.number}`;
  return {
    kind: "conflict",
    dedupeKey: `conflict:${input.headSha}`,
    headSha: input.headSha,
    actor: input.actor,
    request:
      `${subject} has merge conflicts with its base branch at ${input.headSha}. Prepare a ` +
      `resolution against the base in your sandbox. If the right resolution is unclear, ` +
      `say what conflicts and stop.`,
    replyTarget: null,
  };
}

// ---------------------------------------------------------------------------
// Direct GitHub commands on a managed bound PR.
// ---------------------------------------------------------------------------

/**
 * `@bot resume` (clears an outside-push pause) and `@bot manage pushes as me`
 * (records an inactive grant). Returns true when the comment was one of these
 * and has been handled. Callers have already applied the author gate.
 */
export async function handleBoundCommand(
  ctx: PrManagerContext,
  route: Extract<BindingRoute, { kind: "bound" }>,
  input: { owner: string; repo: string; number: number; raw: unknown; text: string },
): Promise<boolean> {
  const command = commandText(input.text, ctx.userName);
  const comment = isRecord(input.raw) && isRecord(input.raw.comment) ? input.raw.comment : {};
  const actor = actorFrom(comment.user);
  if (!actor) return false;
  const { binding, ref } = route;

  if (/^resume\b/i.test(command) && binding.automation_paused) {
    const updated = await updateState(ctx, ref, { automation_paused: null });
    if (!updated) return true;
    await postIssueComment(
      ctx,
      input.owner,
      input.repo,
      input.number,
      `Resumed automated follow-ups on this PR (reconfirmed by @${actor.github_login}).`,
    );
    await markPending(ctx, ref);
    requestDispatch(ctx, ref);
    return true;
  }

  if (/^manage pushes as me\b/i.test(command)) {
    // Recorded for audit and future enforcement only: api-rs stores every
    // grant with `active: false`, and v1 never pushes on its strength.
    const updated = await updateState(ctx, ref, {
      push_grant: {
        actor: { platform: "github", id: actor.github_user_id, name: actor.github_login },
        branch: binding.head_ref ?? null,
        recorded_at: new Date().toISOString(),
        active: false,
      },
    });
    if (!updated) return true;
    await postIssueComment(
      ctx,
      input.owner,
      input.repo,
      input.number,
      `Noted, @${actor.github_login}. Centaur can't push on anyone's behalf yet, so it will keep ` +
        `preparing changes in its session and ask for an authorized push.`,
    );
    return true;
  }
  return false;
}

function commandText(text: string, userName: string): string {
  const mention = new RegExp(`@${escapeRegExp(userName)}\\b`, "gi");
  return text.replace(mention, " ").replace(/\s+/g, " ").trim();
}

// ---------------------------------------------------------------------------
// Dispatcher.
// ---------------------------------------------------------------------------

/**
 * Drive one binding's queue in the background: on enqueue, on resume, on the
 * timer, and when a dispatched execution completes.
 */
export function requestDispatch(ctx: PrManagerContext, ref: PrBindingRef): void {
  backgroundWaitUntil(
    dispatchBinding(ctx, ref).catch((error) => {
      logger(ctx).warn("githubbot_pr_dispatch_failed", {
        error: errorMessage(error),
        pr: prLabel(ref),
      });
    }),
  );
}

/**
 * One dispatch attempt. GitHub is rechecked (assignment, open state, current
 * head) immediately before the call; api-rs then decides under a row lock and
 * starts at most one execution on the bound thread key.
 */
export async function dispatchBinding(
  ctx: PrManagerContext,
  ref: PrBindingRef,
): Promise<PrDispatchResult | null> {
  if (!ctx.bindings) return null;
  return runExclusive(`pr-dispatch:${prLabel(ref)}`, async () => {
    const [owner = "", repo = ""] = ref.repository.split("/", 2);
    const pr = await fetchPr(ctx, owner, repo, ref.number);
    if (!pr) return null; // stays pending; the timer retries
    const request: PrDispatchRequest = {
      current_head_sha: pr.headSha,
      assigned: isOwnedPr({ assignees: pr.assignees, userName: ctx.userName }),
      open: isOpen(pr),
    };
    let result: PrDispatchResult;
    try {
      result = await ctx.bindings!.dispatch(ref, request);
    } catch (error) {
      logger(ctx).warn("githubbot_pr_dispatch_call_failed", {
        error: errorMessage(error),
        pr: prLabel(ref),
      });
      return null;
    }
    traceLog(ctx.options, "githubbot_pr_dispatch_result", undefined, {
      execution_id: result.dispatched ? result.execution_id : undefined,
      followup_id: result.dispatched?.id,
      pr: prLabel(ref),
      reason: result.dispatched ? "dispatched" : result.reason,
    });
    if (result.dispatched) {
      backgroundWaitUntil(
        renderDispatchedFollowup(ctx, ref, result.dispatched, result.execution_id),
      );
      return result;
    }
    // Only `busy` is retried by the timer. Everything else (`empty`,
    // `paused`, `unassigned`, `closed`, `not_verified`, and
    // `session_unavailable`, whose binding and queue api-rs keeps) waits for
    // the next enqueue, resume, or assignment.
    if (result.reason === "busy") return result;
    if (result.reason === "session_unavailable") {
      await surfaceBlocker(ctx, ref, owner, repo, result.blocker ?? undefined);
    }
    await clearPending(ctx, ref);
    return result;
  });
}

/** Retry every binding with queued work. Returns how many were attempted. */
export async function dispatchPendingBindings(ctx: PrManagerContext): Promise<number> {
  const pending = await loadPending(ctx);
  for (const ref of pending) {
    await dispatchBinding(ctx, ref).catch((error) => {
      logger(ctx).warn("githubbot_pr_dispatch_failed", {
        error: errorMessage(error),
        pr: prLabel(ref),
      });
    });
  }
  return pending.length;
}

/** Periodic dispatcher retry (busy sessions, restarts, missed completions). */
export function startPrDispatchTimer(ctx: PrManagerContext): () => void {
  const intervalMs = ctx.options.prDispatchIntervalMs ?? DEFAULT_DISPATCH_INTERVAL_MS;
  if (!ctx.bindings || intervalMs <= 0) return () => undefined;
  let running = false;
  const timer = setInterval(() => {
    if (running) return;
    running = true;
    void dispatchPendingBindings(ctx).finally(() => {
      running = false;
    });
  }, intervalMs);
  timer.unref?.();
  return () => clearInterval(timer);
}

async function surfaceBlocker(
  ctx: PrManagerContext,
  ref: PrBindingRef,
  owner: string,
  repo: string,
  blocker: string | undefined,
): Promise<void> {
  const text = truncate((blocker ?? "the session is unavailable").trim(), 300);
  const digest = createHash("sha256").update(text).digest("hex").slice(0, 16);
  if (!(await claimOnce(ctx, `pr-blocker:${prLabel(ref)}:${digest}`))) return;
  await postIssueComment(
    ctx,
    owner,
    repo,
    ref.number,
    `Centaur can't continue follow-ups for this PR in the session that opened it: ${text}. ` +
      `The binding and queued follow-ups are kept and resume once the session is available.`,
  );
}

// ---------------------------------------------------------------------------
// Rendering dispatched executions.
// ---------------------------------------------------------------------------

async function renderDispatchedFollowup(
  ctx: PrManagerContext,
  ref: PrBindingRef,
  followup: PrFollowup,
  executionId: string,
): Promise<void> {
  const result = await collectDispatchedOutput(ctx, ref, executionId);
  const ackNodeId = await getState(ctx, `pr-followup-ack:${followup.id}`);
  if (result && followup.reply_target) {
    await postFollowupReply(ctx, ref, followup, result);
  }
  if (ackNodeId) {
    await settleReviewReaction(
      ctx.octokit,
      ackNodeId,
      !result || result.failed,
      ctx.options.logger,
    );
  }
  // Completion event: drive the next queued item, if any.
  await dispatchBinding(ctx, ref).catch(() => undefined);
}

async function collectDispatchedOutput(
  ctx: PrManagerContext,
  ref: PrBindingRef,
  executionId: string,
): Promise<TurnResult | null> {
  for (let attempt = 1; attempt <= RENDER_ATTEMPTS; attempt++) {
    try {
      const stream = await ctx.bindings!.streamEvents(ref, executionId, 0, () => undefined);
      return await collectTurnOutput(ctx.options, stream);
    } catch (error) {
      if (attempt < RENDER_ATTEMPTS && isRetryableSessionApiError(error)) {
        await sleep(250 * 2 ** attempt);
        continue;
      }
      // No readable output: stay silent on GitHub rather than post a reply
      // that isn't the turn's answer.
      logger(ctx).warn("githubbot_pr_followup_render_failed", {
        error: errorMessage(error),
        execution_id: executionId,
        pr: prLabel(ref),
      });
      return null;
    }
  }
  return null;
}

/**
 * Post the turn's answer to the GitHub comment or review it answers, after
 * rechecking that the bot is still assigned, the PR is open, and (for
 * head-specific work) the head hasn't moved.
 */
async function postFollowupReply(
  ctx: PrManagerContext,
  ref: PrBindingRef,
  followup: PrFollowup,
  result: TurnResult,
): Promise<void> {
  const target = followup.reply_target;
  if (!target) return;
  const [owner = "", repo = ""] = ref.repository.split("/", 2);
  const pr = await fetchPr(ctx, owner, repo, ref.number);
  const headBound = followup.kind === "ci_failure" || followup.kind === "conflict";
  if (
    !pr ||
    !isOpen(pr) ||
    !isOwnedPr({ assignees: pr.assignees, userName: ctx.userName }) ||
    (headBound && pr.headSha !== followup.head_sha)
  ) {
    traceLog(ctx.options, "githubbot_pr_followup_reply_skipped", undefined, {
      followup_id: followup.id,
      pr: prLabel(ref),
    });
    return;
  }
  const body = result.failed
    ? buildCommentReplyBody({
        answer: `⚠️ I ran into an error before finishing:\n\n${result.errorText || "unknown error"}`,
        cotLines: result.cotLines,
      })
    : buildCommentReplyBody({
        answer: stripFollowupStatus(result.answer),
        cotLines: result.cotLines,
        fallback: stripFollowupStatus(result.fallbackText),
      });
  try {
    if (followup.kind === "review_comment" && typeof target.comment_id === "number") {
      await ctx.octokit.rest.pulls.createReplyForReviewComment({
        owner,
        repo,
        pull_number: ref.number,
        comment_id: target.comment_id,
        body,
      });
    } else {
      await ctx.octokit.rest.issues.createComment({
        owner,
        repo,
        issue_number: ref.number,
        body,
      });
    }
  } catch (error) {
    logger(ctx).warn("githubbot_pr_followup_reply_failed", {
      error: errorMessage(error),
      pr: prLabel(ref),
    });
  }
}

/** Drop the trailing machine-readable status line from a GitHub reply. */
export function stripFollowupStatus(text: string): string {
  return text
    .split("\n")
    .filter((line) => !line.replace(/[`*_]/g, "").trim().startsWith(PR_FOLLOWUP_STATUS_PREFIX))
    .join("\n")
    .trim();
}

// ---------------------------------------------------------------------------
// Pending-dispatch index (githubbot's state store). The queue itself is in
// api-rs; this only remembers which bindings to retry after a restart.
// ---------------------------------------------------------------------------

function pendingKey(ctx: PrManagerContext): string {
  return `${ctx.options.stateKeyPrefix ?? "centaur-githubbot"}:pr-dispatch-pending`;
}

async function loadPending(ctx: PrManagerContext): Promise<PrBindingRef[]> {
  try {
    const value = await ctx.state.get<PrBindingRef[]>(pendingKey(ctx));
    return Array.isArray(value) ? value : [];
  } catch {
    return [];
  }
}

async function markPending(ctx: PrManagerContext, ref: PrBindingRef): Promise<void> {
  await runExclusive(pendingKey(ctx), async () => {
    const pending = await loadPending(ctx);
    if (pending.some((item) => sameRef(item, ref))) return;
    await ctx.state
      .set(pendingKey(ctx), [...pending, ref])
      .catch(() => undefined);
  });
}

async function clearPending(ctx: PrManagerContext, ref: PrBindingRef): Promise<void> {
  await runExclusive(pendingKey(ctx), async () => {
    const pending = await loadPending(ctx);
    const next = pending.filter((item) => !sameRef(item, ref));
    if (next.length === pending.length) return;
    await ctx.state.set(pendingKey(ctx), next).catch(() => undefined);
  });
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

async function updateState(
  ctx: PrManagerContext,
  ref: PrBindingRef,
  update: PrBindingStateUpdate,
): Promise<PrBinding | null> {
  try {
    return await withRetry(() => ctx.bindings!.updateState(ref, update));
  } catch (error) {
    logger(ctx).warn("githubbot_pr_binding_state_failed", {
      error: errorMessage(error),
      pr: prLabel(ref),
      fields: Object.keys(update).join(","),
    });
    return null;
  }
}

async function postIssueComment(
  ctx: PrManagerContext,
  owner: string,
  repo: string,
  number: number,
  body: string,
): Promise<void> {
  try {
    await ctx.octokit.rest.issues.createComment({ owner, repo, issue_number: number, body });
  } catch (error) {
    logger(ctx).warn("githubbot_pr_binding_comment_failed", {
      error: errorMessage(error),
      pr: `${owner}/${repo}#${number}`,
    });
  }
}

async function claimOnce(ctx: PrManagerContext, key: string): Promise<boolean> {
  try {
    return await ctx.state.setIfNotExists(
      `${ctx.options.stateKeyPrefix ?? "centaur-githubbot"}:${key}`,
      "1",
      CLAIM_TTL_MS,
    );
  } catch {
    return true;
  }
}

async function setState(ctx: PrManagerContext, key: string, value: string): Promise<void> {
  try {
    await ctx.state.set(
      `${ctx.options.stateKeyPrefix ?? "centaur-githubbot"}:${key}`,
      value,
      CLAIM_TTL_MS,
    );
  } catch {
    // best-effort ack bookkeeping
  }
}

async function getState(ctx: PrManagerContext, key: string): Promise<string | undefined> {
  try {
    const value = await ctx.state.get<string>(
      `${ctx.options.stateKeyPrefix ?? "centaur-githubbot"}:${key}`,
    );
    return typeof value === "string" ? value : undefined;
  } catch {
    return undefined;
  }
}

async function withRetry<T>(fn: () => Promise<T>): Promise<T> {
  for (let attempt = 1; ; attempt++) {
    try {
      return await fn();
    } catch (error) {
      if (attempt >= LOOKUP_ATTEMPTS || !isRetryableSessionApiError(error)) throw error;
      await sleep(100 * 2 ** attempt);
    }
  }
}

function isOpen(pr: PullRequestSummary): boolean {
  return pr.state === "open" && !pr.merged;
}

function sameRef(a: PrBindingRef, b: PrBindingRef): boolean {
  return a.repository === b.repository && a.number === b.number;
}

function prLabel(ref: PrBindingRef): string {
  return `${ref.repository}#${ref.number}`;
}

function loginOf(user: unknown): string | undefined {
  return isRecord(user) ? stringValue(user.login) : undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value && typeof value === "object" && !Array.isArray(value));
}

function truncate(text: string, max: number): string {
  return text.length <= max ? text : `${text.slice(0, max)}\n…(truncated)`;
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function logger(ctx: PrManagerContext) {
  return ctx.options.logger ?? noopLogger;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
