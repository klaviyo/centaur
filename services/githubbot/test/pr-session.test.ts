import { beforeEach, describe, expect, test } from "bun:test";
import { drainBackgroundWork as drainOnce } from "../src/context";
import {
  handleCiEvent,
  handlePullRequestEvent,
  handleReviewEvent,
  type PrManagerContext,
} from "../src/pr-manager";
import {
  commentFollowup,
  createPrBindingClient,
  dispatchPendingBindings,
  handleBoundCommand,
  resolveBindingRoute,
  stripFollowupStatus,
} from "../src/pr-session";

// ---------------------------------------------------------------------------
// An in-memory api-rs for the /api/pr-bindings routes, reached through
// options.fetch exactly like the other githubbot tests mock api-rs. It follows
// the contract's queue semantics closely enough to observe githubbot's calls.
// ---------------------------------------------------------------------------

type Binding = {
  repository: string;
  number: number;
  thread_key: string;
  state: "registered" | "verified" | "rejected";
  pr_node_id: string | null;
  head_ref: string | null;
  managed: boolean;
  head_sha: string | null;
  automation_paused: boolean;
  pause_reason: string | null;
  push_grant: Record<string, unknown> | null;
  blocker: string | null;
};

type Followup = {
  id: string;
  repository: string;
  number: number;
  kind: string;
  dedupe_key: string;
  head_sha: string;
  actor: { github_user_id: string; github_login: string };
  prompt: { type: string; text: string }[];
  reply_target: Record<string, unknown> | null;
  status: "queued" | "dispatched" | "dropped";
  drop_reason: string | null;
  execution_id: string | null;
};

type Call = { method: string; path: string; body: unknown };

function createApi() {
  const bindings = new Map<string, Binding>();
  const followups: Followup[] = [];
  const calls: Call[] = [];
  const busySessions = new Set<string>();
  const unavailableSessions = new Set<string>();
  const executions: { thread_key: string; followup_id: string; execution_id: string }[] = [];
  let nextId = 1;
  let executionOutput = "Prepared the fix.\nPR-FOLLOWUP-STATUS: needs-push";

  const key = (repo: string, n: number) => `${repo}#${n}`;
  const json = (value: unknown, status = 200) =>
    Promise.resolve(
      new Response(JSON.stringify(value), {
        status,
        headers: { "content-type": "application/json" },
      }),
    );
  const dropQueued = (b: Binding, reason: string, keep?: (f: Followup) => boolean) => {
    for (const f of followups) {
      if (
        f.repository === b.repository &&
        f.number === b.number &&
        f.status === "queued" &&
        !keep?.(f)
      ) {
        f.status = "dropped";
        f.drop_reason = reason;
      }
    }
  };

  const fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input));
    const method = init?.method ?? "GET";
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    calls.push({ method, path: url.pathname, body });
    const match = /^\/api\/pr-bindings\/([^/]+)\/([^/]+)\/(\d+)(?:\/(.*))?$/.exec(url.pathname);
    if (!match) return json({ error: "not found" }, 404);
    const repository = `${decodeURIComponent(match[1]!)}/${decodeURIComponent(match[2]!)}`;
    const number = Number(match[3]);
    const suffix = match[4];
    const b = bindings.get(key(repository, number));
    if (!b) return json({ error: "binding_not_found" }, 404);

    if (!suffix && method === "GET") {
      return json({ ...b, session_exists: true, active_execution: busySessions.has(b.thread_key) });
    }
    if (suffix === "verify") {
      if (b.state === "verified" && b.pr_node_id !== body.node_id) {
        return json({ error: "binding_conflict" }, 409);
      }
      b.state = "verified";
      b.pr_node_id = body.node_id;
      b.head_ref = body.head_ref;
      b.head_sha = body.head_sha;
      return json(b);
    }
    if (suffix === "state") {
      if (body.managed !== undefined) b.managed = body.managed;
      if (body.head_sha !== undefined) b.head_sha = body.head_sha;
      if ("automation_paused" in body) {
        b.automation_paused = body.automation_paused !== null;
        b.pause_reason = body.automation_paused?.reason ?? null;
      }
      if ("push_grant" in body) {
        b.push_grant = body.push_grant ? { ...body.push_grant, active: false } : null;
      }
      if ("blocker" in body) b.blocker = body.blocker;
      if (body.closed === true || body.managed === false) {
        dropQueued(b, body.closed === true ? "closed" : "unassigned");
        b.push_grant = null;
      }
      return json(b);
    }
    if (suffix === "followups") {
      if (b.state !== "verified" || !b.managed) return json({ error: "binding_inactive" }, 409);
      const existing = followups.find(
        (f) => f.repository === repository && f.number === number && f.dedupe_key === body.dedupe_key,
      );
      if (existing) return json({ followup: existing, created: false });
      const followup: Followup = {
        ...body,
        id: `fu-${nextId++}`,
        repository,
        number,
        status: "queued",
        drop_reason: null,
        execution_id: null,
      };
      followups.push(followup);
      return json({ followup, created: true });
    }
    if (suffix === "dispatch") {
      if (b.state !== "verified" || !b.managed || !body.assigned || !body.open) {
        dropQueued(b, body.open ? "unassigned" : "closed");
        return json({ dispatched: null, reason: body.open ? "unassigned" : "closed" });
      }
      if (b.automation_paused) return json({ dispatched: null, reason: "paused" });
      dropQueued(
        b,
        "stale_head",
        (f) =>
          f.head_sha === body.current_head_sha ||
          ["review", "issue_comment", "review_comment", "mention"].includes(f.kind),
      );
      if (busySessions.has(b.thread_key)) return json({ dispatched: null, reason: "busy" });
      if (unavailableSessions.has(b.thread_key)) {
        b.blocker = "session was archived";
        return json({ dispatched: null, reason: "session_unavailable", blocker: b.blocker });
      }
      const next = followups.find(
        (f) => f.repository === repository && f.number === number && f.status === "queued",
      );
      if (!next) return json({ dispatched: null, reason: "empty" });
      const executionId = `exec-${next.id}`;
      next.status = "dispatched";
      next.execution_id = executionId;
      executions.push({ thread_key: b.thread_key, followup_id: next.id, execution_id: executionId });
      return json({ dispatched: next, execution_id: executionId });
    }
    if (suffix?.startsWith("executions/") && suffix.endsWith("/events")) {
      const data = JSON.stringify({ result: executionOutput });
      const sse = `id: 1\nevent: session.execution_completed\ndata: ${data}\n\n`;
      return new Response(sse, { status: 200, headers: { "content-type": "text/event-stream" } });
    }
    return json({ error: "unsupported" }, 404);
  };

  return {
    bindings,
    busySessions,
    calls,
    executions,
    fetch,
    followups,
    unavailableSessions,
    setOutput(text: string) {
      executionOutput = text;
    },
    bind(input: Partial<Binding> & { repository: string; number: number; thread_key: string }) {
      const binding: Binding = {
        state: "verified",
        pr_node_id: `PR_${input.number}`,
        head_ref: "centaur/fix",
        managed: false,
        head_sha: "abc123",
        automation_paused: false,
        pause_reason: null,
        push_grant: null,
        blocker: null,
        ...input,
      };
      bindings.set(key(binding.repository, binding.number), binding);
      return binding;
    },
  };
}

type Api = ReturnType<typeof createApi>;

function makeState() {
  const values = new Map<string, unknown>();
  return {
    values,
    async get(key: string) {
      return values.get(key);
    },
    async set(key: string, value: unknown) {
      values.set(key, value);
    },
    async setIfNotExists(key: string, value: unknown) {
      if (values.has(key)) return false;
      values.set(key, value);
      return true;
    },
    async delete(key: string) {
      values.delete(key);
    },
  };
}

type Pr = {
  number: number;
  assignees: string[];
  headSha: string;
  state?: string;
  mergeableState?: string;
};

function makeGithub(prs: Pr[]) {
  const comments: { number: number; body: string }[] = [];
  const reviewReplies: { comment_id: number; body: string }[] = [];
  const reactions: { subjectId: string; content: string }[] = [];
  const writes: string[] = [];
  const byNumber = new Map(prs.map((pr) => [pr.number, pr]));
  let rollup = "FAILURE";
  const octokit = {
    graphql: async (query: string, vars: Record<string, string>) => {
      if (query.includes("addReaction")) {
        reactions.push({ subjectId: vars.subjectId!, content: vars.content! });
        return {};
      }
      return {
        repository: {
          object: {
            statusCheckRollup: {
              state: rollup,
              contexts: {
                nodes: [
                  {
                    __typename: "CheckRun",
                    name: "build",
                    status: "COMPLETED",
                    conclusion: rollup === "FAILURE" ? "FAILURE" : "SUCCESS",
                  },
                ],
              },
            },
          },
        },
      };
    },
    rest: {
      pulls: {
        get: async ({ pull_number }: { pull_number: number }) => {
          const pr = byNumber.get(pull_number);
          if (!pr) throw Object.assign(new Error("Not Found"), { status: 404 });
          return {
            data: {
              assignees: pr.assignees.map((login) => ({ login })),
              created_at: "2026-11-23T10:00:00Z",
              draft: false,
              head: { ref: "centaur/fix", repo: { full_name: "base/repo" }, sha: pr.headSha },
              labels: [],
              mergeable_state: pr.mergeableState ?? "blocked",
              merged: false,
              node_id: `PR_${pr.number}`,
              number: pr.number,
              state: pr.state ?? "open",
              title: "Fix",
              user: { login: "centaur-bot" },
            },
          };
        },
        merge: async () => {
          writes.push("merge");
          return { data: {} };
        },
        updateBranch: async () => {
          writes.push("updateBranch");
          return { data: {} };
        },
        createReplyForReviewComment: async (input: { comment_id: number; body: string }) => {
          reviewReplies.push({ comment_id: input.comment_id, body: input.body });
          return { data: {} };
        },
      },
      issues: {
        createComment: async (input: { issue_number: number; body: string }) => {
          comments.push({ number: input.issue_number, body: input.body });
          return { data: {} };
        },
      },
      repos: {
        getCommit: async () => ({ data: { author: { login: "centaur-bot" } } }),
        listPullRequestsAssociatedWithCommit: async () => ({ data: [] }),
      },
      git: {
        deleteRef: async () => {
          writes.push("deleteRef");
          return { data: {} };
        },
      },
    },
  };
  return {
    comments,
    octokit,
    prs: byNumber,
    reactions,
    reviewReplies,
    writes,
    setRollup(state: "FAILURE" | "SUCCESS") {
      rollup = state;
    },
  };
}

type Github = ReturnType<typeof makeGithub>;

const quietLogger = { debug() {}, warn() {}, error() {}, info() {}, child: () => quietLogger };

function makeCtx(api: Api, github: Github): PrManagerContext {
  const options = {
    apiUrl: "http://api.test",
    ciSettleConfirmMs: 0,
    deleteBranchOnMerge: false,
    fetch: api.fetch,
    logger: quietLogger,
    prDispatchIntervalMs: 0,
    token: "test",
    webhookSecret: "test",
  };
  return {
    bindings: createPrBindingClient(options),
    octokit: github.octokit,
    options,
    state: makeState(),
    userName: "centaur-bot",
  } as unknown as PrManagerContext;
}

const reviewer = { id: 501, login: "reviewer" };

function reviewEvent(input: { number?: number; reviewId: number; state?: string; commitId?: string }) {
  return JSON.stringify({
    action: "submitted",
    repository: { full_name: "base/repo" },
    pull_request: { number: input.number ?? 7, head: { sha: input.commitId ?? "abc123" } },
    review: {
      id: input.reviewId,
      node_id: `PRR_${input.reviewId}`,
      state: input.state ?? "changes_requested",
      body: "Please rename the helper.",
      commit_id: input.commitId ?? "abc123",
      user: reviewer,
      author_association: "MEMBER",
    },
    sender: reviewer,
  });
}

function prEvent(action: string, extra: Record<string, unknown> = {}, number = 7) {
  return JSON.stringify({
    action,
    repository: { full_name: "base/repo" },
    pull_request: { number, head: { sha: "abc123" } },
    sender: { id: 900, login: "maintainer" },
    ...extra,
  });
}

const redCheckRun = (number = 7, sha = "abc123") =>
  JSON.stringify({
    action: "completed",
    repository: { full_name: "base/repo" },
    check_run: { head_sha: sha, pull_requests: [{ number }] },
    sender: { id: 41898282, login: "github-actions[bot]" },
  });

function sessionCalls(api: Api): Call[] {
  return api.calls.filter((call) => call.path.startsWith("/api/session/"));
}

/** Drain until idle: a dispatch schedules its render, which re-dispatches. */
async function drainBackgroundWork(timeoutMs: number): Promise<void> {
  for (let round = 0; round < 20; round++) {
    if ((await drainOnce(timeoutMs)) === 0) return;
  }
}

let api: Api;
let github: Github;
let ctx: PrManagerContext;

beforeEach(() => {
  api = createApi();
  github = makeGithub([{ number: 7, assignees: ["centaur-bot"], headSha: "abc123" }]);
  ctx = makeCtx(api, github);
});

describe("binding verification", () => {
  test("verifies a registered binding from a fresh GitHub fetch on opened", async () => {
    api.bind({ repository: "base/repo", number: 7, thread_key: "slack:C1:1.1", state: "registered" });
    github.prs.get(7)!.assignees = [];
    await handlePullRequestEvent(ctx, prEvent("opened"));

    const verify = api.calls.find((call) => call.path.endsWith("/verify"));
    expect(verify?.body).toEqual({
      node_id: "PR_7",
      author_login: "centaur-bot",
      created_at: "2026-11-23T10:00:00Z",
      head_ref: "centaur/fix",
      head_sha: "abc123",
    });
    expect(api.bindings.get("base/repo#7")?.state).toBe("verified");
  });

  test("a rejected binding is treated as unbound", async () => {
    api.bind({ repository: "base/repo", number: 7, thread_key: "slack:C1:1.1", state: "rejected" });
    expect((await resolveBindingRoute(ctx, "base", "repo", 7)).kind).toBe("unbound");
  });

  test("an unreadable binding fails closed: no agent work, no github-manage turn", async () => {
    const failing = makeCtx(
      { ...api, fetch: async () => new Response("boom", { status: 500 }) } as Api,
      github,
    );
    await handleReviewEvent(failing, reviewEvent({ reviewId: 1 }));
    await drainBackgroundWork(5_000);
    expect(github.reactions).toEqual([]);
    expect(sessionCalls(api)).toEqual([]);
  });
});

describe("explicit assignment activates management", () => {
  test("assigning the bot sets managed and head without a github-manage session", async () => {
    api.bind({ repository: "base/repo", number: 7, thread_key: "slack:C1:1.1" });
    github.setRollup("SUCCESS");
    await handlePullRequestEvent(ctx, prEvent("assigned", { assignee: { login: "centaur-bot" } }));
    await drainBackgroundWork(5_000);

    const binding = api.bindings.get("base/repo#7")!;
    expect(binding.managed).toBe(true);
    expect(binding.head_sha).toBe("abc123");
    expect(sessionCalls(api)).toEqual([]);
  });

  test("an assigned event for someone else does not activate the binding", async () => {
    api.bind({ repository: "base/repo", number: 7, thread_key: "slack:C1:1.1" });
    await handlePullRequestEvent(ctx, prEvent("assigned", { assignee: { login: "a-human" } }));
    expect(api.bindings.get("base/repo#7")!.managed).toBe(false);
  });

  test("activating on a red head queues one CI follow-up for the bound session", async () => {
    api.bind({ repository: "base/repo", number: 7, thread_key: "slack:C1:1.1" });
    await handlePullRequestEvent(ctx, prEvent("assigned", { assignee: { login: "centaur-bot" } }));
    await drainBackgroundWork(5_000);

    expect(api.followups.map((f) => [f.kind, f.dedupe_key])).toEqual([
      ["ci_failure", "ci:abc123:failure"],
    ]);
    expect(api.executions).toEqual([
      { thread_key: "slack:C1:1.1", followup_id: "fu-1", execution_id: "exec-fu-1" },
    ]);
  });
});

describe("unbound PRs keep the existing flow", () => {
  test("a review on an unbound owned PR runs the github-manage turn as before", async () => {
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 3 }));
    await drainBackgroundWork(5_000);
    const created = sessionCalls(api).find((call) => call.method === "POST");
    expect(created?.path).toBe(`/api/session/${encodeURIComponent("github-manage:base/repo:7")}`);
    expect(api.followups).toEqual([]);
  });
});

describe("managed bound PR follow-ups", () => {
  beforeEach(() => {
    api.bind({ repository: "base/repo", number: 7, thread_key: "slack:C1:1.1", managed: true });
  });

  test("a review is queued, dispatched to the bound session, and answered on GitHub", async () => {
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 11 }));
    await drainBackgroundWork(5_000);

    expect(api.followups).toHaveLength(1);
    const followup = api.followups[0]!;
    expect(followup.kind).toBe("review");
    expect(followup.dedupe_key).toBe("review:11");
    expect(followup.actor).toEqual({ github_user_id: "501", github_login: "reviewer" });
    expect(followup.reply_target).toEqual({
      type: "github_comment",
      repository: "base/repo",
      number: 7,
      review_id: 11,
    });
    expect(api.executions.map((e) => e.thread_key)).toEqual(["slack:C1:1.1"]);
    expect(github.comments).toEqual([{ number: 7, body: "Prepared the fix." }]);
    expect(github.reactions).toEqual([
      { subjectId: "PRR_11", content: "EYES" },
      { subjectId: "PRR_11", content: "ROCKET" },
    ]);
  });

  test("replay: a redelivered review is a no-op", async () => {
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 12 }));
    await drainBackgroundWork(5_000);
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 12 }));
    await drainBackgroundWork(5_000);

    expect(api.followups).toHaveLength(1);
    expect(api.executions).toHaveLength(1);
    expect(api.calls.filter((call) => call.path.endsWith("/dispatch"))).toHaveLength(2);
    expect(github.comments).toHaveLength(1);
  });

  test("distinct reviews stay distinct; redundant red CI on one head collapses", async () => {
    api.busySessions.add("slack:C1:1.1");
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 21 }));
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 22 }));
    await handleCiEvent(ctx, "check_run", redCheckRun());
    await handleCiEvent(ctx, "check_suite", redCheckRun().replace("check_run", "check_suite"));
    await drainBackgroundWork(5_000);
    expect(api.followups.map((f) => f.dedupe_key)).toEqual([
      "review:21",
      "review:22",
      "ci:abc123:failure",
    ]);
  });

  test("busy session: work waits, then the timer path dispatches it", async () => {
    api.busySessions.add("slack:C1:1.1");
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 31 }));
    await drainBackgroundWork(5_000);
    expect(api.executions).toEqual([]);
    expect(api.followups[0]!.status).toBe("queued");

    api.busySessions.delete("slack:C1:1.1");
    expect(await dispatchPendingBindings(ctx)).toBe(1);
    await drainBackgroundWork(5_000);
    expect(api.executions.map((e) => e.followup_id)).toEqual(["fu-1"]);
    // The completion re-dispatch found nothing else and cleared the index.
    expect(await dispatchPendingBindings(ctx)).toBe(0);
  });

  test("stale head: dispatch sends GitHub's current head and CI for the old head is dropped", async () => {
    api.busySessions.add("slack:C1:1.1");
    await handleCiEvent(ctx, "check_run", redCheckRun(7, "abc123"));
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 41 }));
    await drainBackgroundWork(5_000);

    github.prs.get(7)!.headSha = "def456";
    api.busySessions.delete("slack:C1:1.1");
    await dispatchPendingBindings(ctx);
    await drainBackgroundWork(5_000);

    const dispatches = api.calls.filter((call) => call.path.endsWith("/dispatch"));
    expect(dispatches.at(-1)?.body).toEqual({
      current_head_sha: "def456",
      assigned: true,
      open: true,
    });
    const ci = api.followups.find((f) => f.kind === "ci_failure")!;
    expect([ci.status, ci.drop_reason]).toEqual(["dropped", "stale_head"]);
    expect(api.followups.find((f) => f.kind === "review")!.status).toBe("dispatched");
  });

  test("red CI for a head the PR already moved past is ignored", async () => {
    github.prs.get(7)!.headSha = "def456";
    await handleCiEvent(ctx, "check_run", redCheckRun(7, "abc123"));
    expect(api.followups).toEqual([]);
  });

  test("unassignment drops the queue and stops dispatch", async () => {
    api.busySessions.add("slack:C1:1.1");
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 51 }));
    await drainBackgroundWork(5_000);
    await handlePullRequestEvent(ctx, prEvent("unassigned", { assignee: { login: "centaur-bot" } }));

    expect(api.calls.at(-1)).toMatchObject({ path: "/api/pr-bindings/base/repo/7/state", body: { managed: false } });
    expect(api.followups[0]).toMatchObject({ status: "dropped", drop_reason: "unassigned" });
    api.busySessions.delete("slack:C1:1.1");
    expect(await dispatchPendingBindings(ctx)).toBe(0);
    expect(api.executions).toEqual([]);
  });

  test("closure drops the queue", async () => {
    api.busySessions.add("slack:C1:1.1");
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 61 }));
    await drainBackgroundWork(5_000);
    await handlePullRequestEvent(ctx, prEvent("closed"));

    expect(api.calls.at(-1)).toMatchObject({ body: { closed: true } });
    expect(api.followups[0]).toMatchObject({ status: "dropped", drop_reason: "closed" });
    expect(await dispatchPendingBindings(ctx)).toBe(0);
  });

  test("a PR closed between enqueue and dispatch is rechecked from GitHub", async () => {
    api.busySessions.add("slack:C1:1.1");
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 62 }));
    await drainBackgroundWork(5_000);
    github.prs.get(7)!.state = "closed";
    api.busySessions.delete("slack:C1:1.1");
    await dispatchPendingBindings(ctx);
    expect(api.calls.at(-1)?.body).toMatchObject({ open: false });
    expect(api.executions).toEqual([]);
  });

  test("an unavailable session posts one blocker and keeps the binding", async () => {
    api.unavailableSessions.add("slack:C1:1.1");
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 71 }));
    await drainBackgroundWork(5_000);
    // Not retried by the timer; a later enqueue retries, and the note is not repeated.
    expect(await dispatchPendingBindings(ctx)).toBe(0);
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 72 }));
    await drainBackgroundWork(5_000);

    expect(github.comments).toHaveLength(1);
    expect(github.comments[0]!.body).toContain("session was archived");
    expect(api.bindings.get("base/repo#7")!.thread_key).toBe("slack:C1:1.1");
    expect(api.followups.map((f) => f.status)).toEqual(["queued", "queued"]);
  });

  test("merge conflicts queue a conflict follow-up instead of a github-manage turn", async () => {
    github.prs.get(7)!.mergeableState = "dirty";
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 81, state: "approved" }));
    await drainBackgroundWork(5_000);
    expect(api.followups.map((f) => f.dedupe_key)).toEqual(["conflict:abc123"]);
    expect(sessionCalls(api)).toEqual([]);
  });

  test("green CI still merges deterministically, as today", async () => {
    github.setRollup("SUCCESS");
    github.prs.get(7)!.mergeableState = "clean";
    await handleCiEvent(ctx, "check_run", redCheckRun());
    expect(github.writes).toEqual(["merge"]);
    expect(api.followups).toEqual([]);
  });
});

describe("scope isolation", () => {
  test("another PR's events never reach this session", async () => {
    github = makeGithub([
      { number: 7, assignees: ["centaur-bot"], headSha: "abc123" },
      { number: 8, assignees: ["centaur-bot"], headSha: "fff000" },
    ]);
    ctx = makeCtx(api, github);
    api.bind({ repository: "base/repo", number: 7, thread_key: "slack:C1:1.1", managed: true });
    api.bind({ repository: "base/repo", number: 8, thread_key: "slack:C2:2.2", managed: true, head_sha: "fff000" });

    await handleReviewEvent(ctx, reviewEvent({ number: 8, reviewId: 91, commitId: "fff000" }));
    await drainBackgroundWork(5_000);

    expect(api.followups.map((f) => [f.number, f.dedupe_key])).toEqual([[8, "review:91"]]);
    expect(api.executions.map((e) => e.thread_key)).toEqual(["slack:C2:2.2"]);
    expect(api.calls.some((call) => call.path.startsWith("/api/pr-bindings/base/repo/7/"))).toBe(false);
  });
});

describe("outside push", () => {
  beforeEach(() => {
    api.bind({ repository: "base/repo", number: 7, thread_key: "slack:C1:1.1", managed: true });
  });

  const push = (sender: string, after: string) =>
    prEvent("synchronize", { after, sender: { id: 700, login: sender } });

  test("a human push pauses automation with one note, and resume reconfirms", async () => {
    await handlePullRequestEvent(ctx, push("a-human", "bbb222"));
    await handlePullRequestEvent(ctx, push("a-human", "ccc333"));

    const binding = api.bindings.get("base/repo#7")!;
    expect(binding.automation_paused).toBe(true);
    expect(binding.head_sha).toBe("ccc333");
    expect(github.comments).toHaveLength(1);
    expect(github.comments[0]!.body).toContain("@centaur-bot resume");

    // Queued work waits while paused.
    github.prs.get(7)!.headSha = "ccc333";
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 95, commitId: "ccc333" }));
    await drainBackgroundWork(5_000);
    expect(api.executions).toEqual([]);

    const route = await resolveBindingRoute(ctx, "base", "repo", 7);
    if (route.kind !== "bound") throw new Error("expected bound");
    const handled = await handleBoundCommand(ctx, route, {
      owner: "base",
      repo: "repo",
      number: 7,
      raw: { type: "issue_comment", comment: { id: 5, user: { id: 900, login: "maintainer" } } },
      text: "@centaur-bot resume",
    });
    await drainBackgroundWork(5_000);
    expect(handled).toBe(true);
    expect(api.bindings.get("base/repo#7")!.automation_paused).toBe(false);
    expect(api.executions.map((e) => e.followup_id)).toEqual(["fu-1"]);
  });

  test("githubbot's own branch update is not an outside push", async () => {
    await handlePullRequestEvent(ctx, push("centaur-bot", "bbb222"));
    expect(api.bindings.get("base/repo#7")!.automation_paused).toBe(false);
    expect(github.comments).toEqual([]);
  });
});

describe("binding confers no write", () => {
  beforeEach(() => {
    api.bind({ repository: "base/repo", number: 7, thread_key: "slack:C1:1.1", managed: true });
  });

  test("follow-ups run no push-capable githubbot action and carry prepare-and-ask", async () => {
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 101 }));
    await handleCiEvent(ctx, "check_run", redCheckRun());
    await drainBackgroundWork(5_000);

    // githubbot never executes a turn itself for a bound PR: the only
    // executions are api-rs dispatches onto the bound key.
    expect(sessionCalls(api)).toEqual([]);
    expect(github.writes).toEqual([]);
    for (const followup of api.followups) {
      const text = followup.prompt.map((part) => part.text).join("\n");
      expect(text).toContain("[PR follow-up]");
      expect(text).toContain("do not push");
    }
  });

  test("'manage pushes as me' records an inactive grant and changes nothing", async () => {
    const route = await resolveBindingRoute(ctx, "base", "repo", 7);
    if (route.kind !== "bound") throw new Error("expected bound");
    await handleBoundCommand(ctx, route, {
      owner: "base",
      repo: "repo",
      number: 7,
      raw: { type: "issue_comment", comment: { id: 6, user: { id: 900, login: "maintainer" } } },
      text: "@centaur-bot manage pushes as me",
    });
    const grantCall = api.calls.find((call) => call.path.endsWith("/state"));
    expect(grantCall?.body).toMatchObject({
      push_grant: { actor: { platform: "github", id: "900", name: "maintainer" }, active: false },
    });
    expect(api.bindings.get("base/repo#7")!.push_grant).toMatchObject({ active: false });

    await handleReviewEvent(ctx, reviewEvent({ reviewId: 102 }));
    await drainBackgroundWork(5_000);
    expect(api.followups[0]!.prompt[0]!.text).toContain("do not push");
    expect(github.writes).toEqual([]);
  });

  test("dispatch never names a requester: identity comes only from the webhook actor", async () => {
    await handleReviewEvent(ctx, reviewEvent({ reviewId: 103 }));
    await drainBackgroundWork(5_000);
    for (const call of api.calls.filter((c) => c.path.endsWith("/dispatch"))) {
      expect(Object.keys(call.body as object).sort()).toEqual(["assigned", "current_head_sha", "open"]);
    }
    expect(api.followups[0]!.actor).toEqual({ github_user_id: "501", github_login: "reviewer" });
  });
});

describe("comment follow-ups", () => {
  test("a review-comment mention replies to its thread root and dedupes on the comment", () => {
    const draft = commentFollowup({
      owner: "base",
      repo: "repo",
      number: 7,
      headSha: "abc123",
      text: "@centaur-bot can you handle the nil case?",
      raw: {
        type: "review_comment",
        comment: {
          id: 222,
          in_reply_to_id: 200,
          node_id: "PRRC_222",
          path: "src/a.ts",
          line: 4,
          diff_hunk: "@@ -1 +1 @@",
          user: { id: 501, login: "reviewer" },
        },
      },
    })!;
    expect(draft.kind).toBe("review_comment");
    expect(draft.dedupeKey).toBe("comment:222");
    expect(draft.replyTarget).toEqual({
      type: "github_comment",
      repository: "base/repo",
      number: 7,
      comment_id: 200,
    });
    expect(draft.actor).toEqual({ github_user_id: "501", github_login: "reviewer" });
    expect(draft.request).toContain("src/a.ts");
  });

  test("a dispatched review-comment reply posts to the review thread", async () => {
    api.bind({ repository: "base/repo", number: 7, thread_key: "slack:C1:1.1", managed: true });
    api.setOutput("Handled the nil case.\n`PR-FOLLOWUP-STATUS: done`");
    const route = await resolveBindingRoute(ctx, "base", "repo", 7);
    if (route.kind !== "bound") throw new Error("expected bound");
    const { enqueueBoundFollowup } = await import("../src/pr-session");
    await enqueueBoundFollowup(
      ctx,
      route,
      commentFollowup({
        owner: "base",
        repo: "repo",
        number: 7,
        headSha: "abc123",
        text: "@centaur-bot please fix",
        raw: { type: "review_comment", comment: { id: 300, user: { id: 501, login: "reviewer" } } },
      })!,
    );
    await drainBackgroundWork(5_000);
    expect(github.reviewReplies).toEqual([{ comment_id: 300, body: "Handled the nil case." }]);
  });

  test("an unknown or non-numeric author is never queued", () => {
    expect(
      commentFollowup({
        owner: "base",
        repo: "repo",
        number: 7,
        headSha: "abc123",
        text: "hi",
        raw: { type: "issue_comment", comment: { id: 1, user: { login: "x" } } },
      }),
    ).toBeUndefined();
  });
});

test("stripFollowupStatus drops only the status line", () => {
  expect(stripFollowupStatus("Done.\n\n**PR-FOLLOWUP-STATUS: done**")).toBe("Done.");
  expect(stripFollowupStatus("Keep this line.")).toBe("Keep this line.");
});
