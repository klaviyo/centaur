import { describe, expect, test } from "bun:test";
import { drainBackgroundWork } from "../src/context";
import {
  decideMerge,
  handleCiEvent,
  handlePullRequestEvent,
  handleReviewEvent,
  isOwnedPr,
  type PrManagerContext,
} from "../src/pr-manager";
import { emitWorkflowEvent, getPrBinding, isRetryableSessionApiError } from "../src/session-api";
import {
  evaluateCi,
  fetchCiEvaluation,
  firstUsableUrl,
  isCentaurSkipCheck,
  type CiCheck,
} from "../src/workflow-events";

/** The app the fixture's default failing check run was reported by. */
const CHECK_APP_ID = 15368;

/** How api-rs answers a real binding miss: a coded 404, not just any 404. */
function bindingMiss() {
  return Response.json(
    { error: "no binding for that pull request", code: "pr_binding_not_found" },
    { status: 404 },
  );
}

function makeState() {
  const values = new Map<string, unknown>();
  return {
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
    keys() {
      return [...values.keys()];
    },
  };
}

function prPayload(input: {
  assignees?: { login: string }[];
  headRepoFullName: string;
  headSha?: string;
  mergeableState?: string;
  number?: number;
}) {
  return {
    assignees: input.assignees ?? [{ login: "centaur-bot" }],
    draft: false,
    head: {
      ref: "feature",
      repo: { full_name: input.headRepoFullName },
      sha: input.headSha ?? "abc123",
    },
    labels: [],
    mergeable_state: input.mergeableState ?? "clean",
    merged: false,
    number: input.number ?? 7,
    state: "open",
    title: "Test PR",
  };
}

describe("evaluateCi", () => {
  test("not settled while any check is in progress", () => {
    const checks: CiCheck[] = [
      { name: "build", status: "completed", conclusion: "success" },
      { name: "test", status: "in_progress", conclusion: null },
    ];
    expect(evaluateCi(checks, [])).toMatchObject({ settled: false });
  });

  test("settled + green when all checks succeed", () => {
    const checks: CiCheck[] = [
      { name: "build", status: "completed", conclusion: "success" },
      { name: "test", status: "completed", conclusion: "skipped" },
    ];
    expect(evaluateCi(checks, [])).toEqual({
      settled: true,
      failed: false,
      failingNames: [],
    });
  });

  test("settled + red, collecting failing names from checks and statuses", () => {
    const checks: CiCheck[] = [
      { name: "build", status: "completed", conclusion: "success" },
      { name: "lint", status: "completed", conclusion: "failure" },
      { name: "e2e", status: "completed", conclusion: "timed_out" },
    ];
    const result = evaluateCi(checks, [
      { state: "success", context: "coverage" },
      { state: "error", context: "deploy-preview" },
    ]);
    expect(result.settled).toBe(true);
    expect(result.failed).toBe(true);
    expect(result.failingNames.sort()).toEqual(["deploy-preview", "e2e", "lint"]);
  });

  test("carries the first failing check's link, and a failing status's target url", () => {
    const failing: CiCheck[] = [
      { name: "build", status: "completed", conclusion: "success", url: "https://ci.example.test/ok" },
      { name: "lint", status: "completed", conclusion: "failure", url: "https://ci.example.test/lint" },
      { name: "e2e", status: "completed", conclusion: "failure", url: "https://ci.example.test/e2e" },
    ];
    expect(evaluateCi(failing, []).noticeFailure).toEqual({
      name: "lint",
      kind: "check_run",
      url: "https://ci.example.test/lint",
    });
    expect(
      evaluateCi(
        [{ name: "build", status: "completed", conclusion: "success" }],
        [{ state: "failure", context: "deploy", url: "https://ci.example.test/deploy" }],
      ),
    ).toMatchObject({
      failingNames: ["deploy"],
      noticeFailure: { name: "deploy", kind: "status", url: "https://ci.example.test/deploy" },
    });
  });

  test("has no link when the first failure has none, and never borrows a later one", () => {
    const result = evaluateCi(
      [
        { name: "lint", status: "completed", conclusion: "failure" },
        { name: "e2e", status: "completed", conclusion: "failure", url: "https://ci.example.test/e2e" },
      ],
      [],
    );
    expect(result.failingNames[0]).toBe("lint");
    expect(result.noticeFailure).toEqual({ name: "lint", kind: "check_run" });
  });

  test("pending legacy status keeps it unsettled", () => {
    const result = evaluateCi(
      [{ name: "build", status: "completed", conclusion: "success" }],
      [{ state: "pending", context: "deploy" }],
    );
    expect(result.settled).toBe(false);
  });
});

describe("centaur-skip checks", () => {
  function checkRun(input: { conclusion: string; name: string }) {
    return {
      __typename: "CheckRun",
      conclusion: input.conclusion,
      name: input.name,
      startedAt: "2026-08-01T10:00:00Z",
      status: "COMPLETED",
    };
  }

  // Without an explicit display name, GitHub reports the job id as the check name.
  const gate = checkRun({ conclusion: "FAILURE", name: "agent-pr-rules-centaur-skip" });

  function rollupCtx(rollup: {
    state: string;
    nodes: (Record<string, unknown> | null)[];
    counts?: { count: number; state: string }[];
  }) {
    return {
      octokit: {
        graphql: async () => ({
          repository: {
            object: {
              statusCheckRollup: {
                state: rollup.state,
                contexts: {
                  nodes: rollup.nodes,
                  pageInfo: { hasNextPage: false, endCursor: null },
                  checkRunCountsByState: rollup.counts,
                  statusContextCountsByState: rollup.counts ? [] : undefined,
                },
              },
            },
          },
        }),
        // No Actions data: the fallback finds nothing and degrades, which is
        // what the aggregate-fallback case below asserts.
        rest: {
          actions: {
            listWorkflowRunsForRepo: async () => ({ data: { workflow_runs: [] } }),
            listJobsForWorkflowRun: async () => ({ data: { jobs: [] } }),
          },
        },
      },
      options: { logger: quietLogger },
    } as unknown as PrManagerContext;
  }

  test("matches the marker anywhere in the name, case-insensitively", () => {
    for (const name of [
      "agent-pr-rules-centaur-skip",
      "centaur-skip-approvals",
      "Approvals (CENTAUR-SKIP)",
    ]) {
      expect(isCentaurSkipCheck(name)).toBe(true);
    }
  });

  test("leaves an unmarked or absent name alone", () => {
    for (const name of ["agent-pr-rules", "centaur", "skip", ""]) {
      expect(isCentaurSkipCheck(name)).toBe(false);
    }
    expect(isCentaurSkipCheck(null)).toBe(false);
    expect(isCentaurSkipCheck(undefined)).toBe(false);
  });

  test("a marked failure leaves CI green, despite a FAILURE aggregate", async () => {
    // Green here is only possible if the detail wins over the FAILURE aggregate.
    const ctx = rollupCtx({
      state: "FAILURE",
      nodes: [gate, checkRun({ conclusion: "SUCCESS", name: "build" })],
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: false,
      failingNames: [],
      settled: true,
    });
  });

  test("a real failure alongside it still fails, naming only the real one", async () => {
    const ctx = rollupCtx({
      state: "FAILURE",
      nodes: [gate, checkRun({ conclusion: "FAILURE", name: "build" })],
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: true,
      failingNames: ["build"],
      noticeFailure: { name: "build", kind: "check_run" },
      settled: true,
    });
  });

  test("an unreadable detail set with no Actions data falls back to the aggregate", async () => {
    // A null node means contexts are missing, not marked: aggregate is all we have.
    const ctx = rollupCtx({
      state: "FAILURE",
      nodes: [null, checkRun({ conclusion: "SUCCESS", name: "build" })],
      counts: [{ count: 2, state: "COMPLETED" }],
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: true,
      failingNames: [],
      settled: true,
    });
  });
});

// A fine-grained PAT has no Checks permission, so every check node comes back
// null and the rollup alone can't tell a marked check from a real failure.
describe("Actions fallback when check nodes are unreadable", () => {
  function statusCtx(context: string, state: string) {
    return { __typename: "StatusContext", context, createdAt: "2026-08-01T10:00:00Z", state };
  }
  function job(
    name: string,
    conclusion: string | null,
    status = "completed",
    runAttempt = 1,
  ) {
    return { conclusion, name, run_attempt: runAttempt, status };
  }
  function ctxFor(input: {
    nodes: (Record<string, unknown> | null)[];
    checkRuns: number;
    checkState?: string;
    statuses: number;
    actionsJobs: Record<number, ReturnType<typeof job>[]>;
    actionRuns?: Record<number, { event?: string; runAttempt?: number; workflowId: number }>;
    onActionsCall?: () => void;
    rollupState?: string;
  }) {
    const runIds = Object.keys(input.actionsJobs).map(Number);
    return {
      octokit: {
        graphql: async () => ({
          repository: {
            object: {
              statusCheckRollup: {
                state: input.rollupState ?? "FAILURE",
                contexts: {
                  nodes: input.nodes,
                  pageInfo: { hasNextPage: false, endCursor: null },
                  checkRunCountsByState: [
                    { count: input.checkRuns, state: input.checkState ?? "COMPLETED" },
                  ],
                  statusContextCountsByState: [{ count: input.statuses, state: "SUCCESS" }],
                },
              },
            },
          },
        }),
        rest: {
          actions: {
            listWorkflowRunsForRepo: async ({
              page = 1,
              per_page = 30,
            }: {
              page?: number;
              per_page?: number;
            }) => {
              input.onActionsCall?.();
              const pageRunIds = runIds.slice((page - 1) * per_page, page * per_page);
              return {
                data: {
                  workflow_runs: pageRunIds.map((id) => ({
                    event: input.actionRuns?.[id]?.event ?? "pull_request",
                    id,
                    run_attempt: input.actionRuns?.[id]?.runAttempt ?? 1,
                    workflow_id: input.actionRuns?.[id]?.workflowId ?? 1,
                  })),
                },
              };
            },
            listJobsForWorkflowRun: async ({
              filter,
              page = 1,
              per_page = 30,
              run_id,
            }: {
              filter?: string;
              page?: number;
              per_page?: number;
              run_id: number;
            }) => {
              input.onActionsCall?.();
              expect(filter).toBe("latest");
              const jobs = input.actionsJobs[run_id] ?? [];
              return { data: { jobs: jobs.slice((page - 1) * per_page, page * per_page) } };
            },
          },
        },
      },
      options: { logger: quietLogger },
    } as unknown as PrManagerContext;
  }

  test("does not call Actions while the aggregate is pending", async () => {
    let actionsCalls = 0;
    const ctx = ctxFor({
      nodes: [null],
      checkRuns: 1,
      statuses: 0,
      actionsJobs: {},
      onActionsCall: () => {
        actionsCalls += 1;
      },
      rollupState: "PENDING",
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: false,
      failingNames: [],
      settled: false,
    });
    expect(actionsCalls).toBe(0);
  });

  test("does not call Actions while aggregate counts remain pending", async () => {
    let actionsCalls = 0;
    const ctx = ctxFor({
      nodes: [null],
      checkRuns: 1,
      checkState: "IN_PROGRESS",
      statuses: 0,
      actionsJobs: {},
      onActionsCall: () => {
        actionsCalls += 1;
      },
      rollupState: "FAILURE",
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: true,
      failingNames: [],
      settled: false,
    });
    expect(actionsCalls).toBe(0);
  });

  test("reconstructs the checks and drops the marked one", async () => {
    // A typical fine-grained PAT response: check nodes null, commit status readable.
    const ctx = ctxFor({
      nodes: [null, null, null, null, null, statusCtx("Vercel", "SUCCESS")],
      checkRuns: 5,
      statuses: 1,
      actionsJobs: {
        3: [job("agent-pr-rules-centaur-skip", "failure")],
        2: [job("assign-author", "success")],
        1: [job("lint-and-format", "success"), job("typecheck", "success"), job("test", "success")],
      },
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: false,
      failingNames: [],
      settled: true,
    });
  });

  test("still reports a real failure, and names it", async () => {
    const ctx = ctxFor({
      nodes: [null, null, statusCtx("Vercel", "SUCCESS")],
      checkRuns: 2,
      statuses: 1,
      actionsJobs: {
        2: [job("agent-pr-rules-centaur-skip", "failure")],
        1: [job("typecheck", "failure")],
      },
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: true,
      failingNames: ["typecheck"],
      noticeFailure: { name: "typecheck", kind: "check_run" },
      settled: true,
    });
  });

  test("refuses to trust an incomplete reconstruction", async () => {
    // 3 check runs counted but only 2 are Actions jobs: something else posted
    // one (another App), and it could be the red one. Degrade, don't guess.
    const ctx = ctxFor({
      nodes: [null, null, null, statusCtx("Vercel", "SUCCESS")],
      checkRuns: 3,
      statuses: 1,
      actionsJobs: {
        2: [job("agent-pr-rules-centaur-skip", "failure")],
        1: [job("typecheck", "success")],
      },
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: true,
      failingNames: [],
      settled: true,
    });
  });

  test("refuses when a commit status is unreadable too", async () => {
    // 2 statuses counted, 1 readable: a red status could be hiding.
    const ctx = ctxFor({
      nodes: [null, statusCtx("Vercel", "SUCCESS")],
      checkRuns: 1,
      statuses: 2,
      actionsJobs: { 1: [job("agent-pr-rules-centaur-skip", "failure")] },
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: true,
      failingNames: [],
      settled: true,
    });
  });

  test("keeps same-named jobs from separate workflows", async () => {
    const ctx = ctxFor({
      nodes: [null, null, statusCtx("Vercel", "SUCCESS")],
      checkRuns: 2,
      statuses: 1,
      actionsJobs: {
        9: [job("build", "success")],
        4: [job("build", "failure")],
      },
      actionRuns: {
        9: { workflowId: 90 },
        4: { workflowId: 40 },
      },
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: true,
      failingNames: ["build"],
      noticeFailure: { name: "build", kind: "check_run" },
      settled: true,
    });
  });

  test("deduplicates superseded runs before checking the rollup count", async () => {
    // The rollup and the fallback both retain only the newest check context.
    const ctx = ctxFor({
      nodes: [null, statusCtx("Vercel", "SUCCESS")],
      checkRuns: 1,
      statuses: 1,
      actionsJobs: { 9: [job("typecheck", "success")], 4: [job("typecheck", "failure")] },
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: false,
      failingNames: [],
      settled: true,
    });
  });

  test("uses the latest job attempt after a partial rerun", async () => {
    // GitHub's latest filter carries forward successful jobs while replacing
    // rerun jobs, matching the rollup's one context per job.
    const ctx = ctxFor({
      nodes: [null, null, statusCtx("Vercel", "SUCCESS")],
      checkRuns: 2,
      statuses: 1,
      actionsJobs: {
        9: [
          job("build", "success", "completed", 2),
          job("test", "success", "completed", 2),
        ],
      },
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: false,
      failingNames: [],
      settled: true,
    });
  });

  test("paginates jobs within a workflow run", async () => {
    const jobs = Array.from({ length: 101 }, (_, index) =>
      index === 0
        ? job("approval-centaur-skip", "failure")
        : job(`job-${index}`, "success"),
    );
    const ctx = ctxFor({
      nodes: Array.from({ length: 101 }, () => null),
      checkRuns: 101,
      statuses: 0,
      actionsJobs: { 1: jobs },
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: false,
      failingNames: [],
      settled: true,
    });
  });

  test("paginates workflow runs for a SHA", async () => {
    const actionsJobs: Record<number, ReturnType<typeof job>[]> = {};
    const actionRuns: Record<number, { workflowId: number }> = {};
    for (let id = 1; id <= 101; id += 1) {
      actionsJobs[id] = [
        id === 101 ? job("approval-centaur-skip", "failure") : job(`job-${id}`, "success"),
      ];
      actionRuns[id] = { workflowId: id };
    }
    const ctx = ctxFor({
      nodes: Array.from({ length: 101 }, () => null),
      checkRuns: 101,
      statuses: 0,
      actionsJobs,
      actionRuns,
    });
    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: false,
      failingNames: [],
      settled: true,
    });
  });
});

describe("isOwnedPr", () => {
  test("owned when the bot is an assignee (case-insensitive)", () => {
    expect(
      isOwnedPr({
        assignees: ["someone-else", "Centaur-Bot"],
        userName: "centaur-bot",
      }),
    ).toBe(true);
  });

  test("not owned when the bot is not an assignee", () => {
    expect(
      isOwnedPr({
        assignees: ["someone-else"],
        userName: "centaur-bot",
      }),
    ).toBe(false);
  });

  test("not owned when there are no assignees", () => {
    expect(isOwnedPr({ assignees: [], userName: "centaur-bot" })).toBe(false);
  });
});

describe("decideMerge", () => {
  const base = {
    autoMerge: true,
    draft: false,
    holdLabel: "do-not-merge",
    labels: [] as string[],
    merged: false,
    mergeableState: "clean",
    state: "open",
  };

  test("merges a clean, open, non-draft PR", () => {
    expect(decideMerge(base)).toBe("merge");
  });

  test("respects the global disable switch", () => {
    expect(decideMerge({ ...base, autoMerge: false })).toBe("skip_disabled");
  });

  test("respects the per-PR hold label (case-insensitive)", () => {
    expect(decideMerge({ ...base, labels: ["Do-Not-Merge"] })).toBe("skip_hold");
  });

  test("does not merge drafts or closed/merged PRs", () => {
    expect(decideMerge({ ...base, draft: true })).toBe("skip_draft");
    expect(decideMerge({ ...base, merged: true })).toBe("skip_closed");
    expect(decideMerge({ ...base, state: "closed" })).toBe("skip_closed");
  });

  test("routes dirty -> conflict and behind -> update", () => {
    expect(decideMerge({ ...base, mergeableState: "dirty" })).toBe("resolve_conflict");
    expect(decideMerge({ ...base, mergeableState: "behind" })).toBe("update_branch");
  });

  test("waits on blocked/unstable/unknown states", () => {
    expect(decideMerge({ ...base, mergeableState: "blocked" })).toBe("wait");
    expect(decideMerge({ ...base, mergeableState: "unstable" })).toBe("wait");
    expect(decideMerge({ ...base, mergeableState: "unknown" })).toBe("wait");
  });
});

describe("PR management webhooks", () => {
  test("merges when CI settles green again on an already-evaluated head", async () => {
    let mergeCalls = 0;
    const state = makeState();
    // An earlier green settle on this head was claimed while the PR was still
    // blocked (e.g. waiting on an approval).
    await state.set("centaur-githubbot:ci-settled:base/repo#7:abc123", "1");
    const ctx = {
      octokit: {
        graphql: async () => ({
          repository: {
            object: {
              statusCheckRollup: { state: "SUCCESS", contexts: { nodes: [] } },
            },
          },
        }),
        rest: {
          pulls: {
            get: async () => ({ data: prPayload({ headRepoFullName: "base/repo" }) }),
            merge: async () => {
              mergeCalls += 1;
              return { data: {} };
            },
          },
          git: { deleteRef: async () => ({ data: {} }) },
        },
      },
      options: {
        apiUrl: "http://localhost",
        deleteBranchOnMerge: false,
        logger: { debug() {}, warn() {}, error() {}, info() {} },
      },
      state,
      userName: "centaur-bot",
    } as unknown as PrManagerContext;

    // A required check that finishes later on the same SHA.
    await handleCiEvent(
      ctx,
      "check_suite",
      JSON.stringify({
        action: "completed",
        repository: { full_name: "base/repo" },
        check_suite: { head_sha: "abc123", pull_requests: [{ number: 7 }] },
      }),
    );
    expect(mergeCalls).toBe(1);

    // Re-assigning an already-green PR merges too.
    mergeCalls = 0;
    const reassigned = {
      ...ctx,
      options: {
        ...ctx.options,
        // Unbound: the binding lookup must not reach a real network.
        fetch: () => Promise.resolve(bindingMiss()),
      },
      state: makeState(),
    } as unknown as PrManagerContext;
    await reassigned.state.set("centaur-githubbot:ci-settled:base/repo#7:abc123", "1");
    await handlePullRequestEvent(
      reassigned,
      JSON.stringify({
        action: "assigned",
        repository: { full_name: "base/repo" },
        pull_request: { number: 7 },
      }),
    );
    expect(mergeCalls).toBe(1);
  });

  test("does not delete a base-repo branch after merging a fork PR", async () => {
    let deleteRefCalls = 0;
    let mergeCalls = 0;
    const ctx = {
      octokit: {
        rest: {
          pulls: {
            get: async () => ({
              data: prPayload({ headRepoFullName: "contributor/repo" }),
            }),
            merge: async () => {
              mergeCalls += 1;
              return { data: {} };
            },
          },
          git: {
            deleteRef: async () => {
              deleteRefCalls += 1;
              return { data: {} };
            },
          },
        },
      },
      options: {
        apiUrl: "http://localhost",
        logger: { debug() {}, warn() {}, error() {}, info() {} },
      },
      state: makeState(),
      userName: "centaur-bot",
    } as unknown as PrManagerContext;

    await handleReviewEvent(
      ctx,
      JSON.stringify({
        action: "submitted",
        repository: { full_name: "base/repo" },
        pull_request: { number: 7 },
        review: {
          id: 123,
          state: "approved",
          user: { login: "reviewer" },
          author_association: "MEMBER",
        },
      }),
    );

    expect(mergeCalls).toBe(1);
    expect(deleteRefCalls).toBe(0);
  });

  test("routes legacy status webhooks through associated PRs", async () => {
    let associatedCommitSha: string | undefined;
    let mergeCalls = 0;
    const ctx = {
      octokit: {
        graphql: async () => ({
          repository: {
            object: {
              statusCheckRollup: { state: "SUCCESS", contexts: { nodes: [] } },
            },
          },
        }),
        rest: {
          pulls: {
            get: async () => ({
              data: prPayload({
                headRepoFullName: "base/repo",
                headSha: "abc123",
              }),
            }),
            merge: async () => {
              mergeCalls += 1;
              return { data: {} };
            },
          },
          repos: {
            listPullRequestsAssociatedWithCommit: async (input: {
              commit_sha: string;
            }) => {
              associatedCommitSha = input.commit_sha;
              return { data: [{ number: 7 }] };
            },
          },
          git: {
            deleteRef: async () => ({ data: {} }),
          },
        },
      },
      options: {
        apiUrl: "http://localhost",
        deleteBranchOnMerge: false,
        logger: { debug() {}, warn() {}, error() {}, info() {} },
      },
      state: makeState(),
      userName: "centaur-bot",
    } as unknown as PrManagerContext;

    await handleCiEvent(
      ctx,
      "status",
      JSON.stringify({
        repository: { full_name: "base/repo" },
        sha: "abc123",
        state: "success",
      }),
    );

    expect(associatedCommitSha).toBe("abc123");
    expect(mergeCalls).toBe(1);
  });
});

const approvedReview = (reviewId: number) =>
  JSON.stringify({
    action: "submitted",
    repository: { full_name: "base/repo" },
    pull_request: { number: 7 },
    review: {
      id: reviewId,
      state: "approved",
      user: { login: "reviewer" },
      author_association: "MEMBER",
    },
  });

const quietLogger = { debug() {}, warn() {}, error() {}, info() {} };

describe("merge claim lifecycle", () => {
  function mergeCtx(merge: () => Promise<unknown>) {
    return {
      octokit: {
        rest: {
          pulls: {
            get: async () => ({
              data: prPayload({ headRepoFullName: "base/repo" }),
            }),
            merge,
          },
          git: { deleteRef: async () => ({ data: {} }) },
        },
      },
      options: {
        apiUrl: "http://localhost",
        deleteBranchOnMerge: false,
        logger: quietLogger,
      },
      state: makeState(),
      userName: "centaur-bot",
    } as unknown as PrManagerContext;
  }

  test("releases the claim when merge fails, so a later event retries", async () => {
    let mergeCalls = 0;
    const ctx = mergeCtx(async () => {
      mergeCalls += 1;
      if (mergeCalls === 1) throw new Error("Base branch was modified");
      return { data: {} };
    });
    await handleReviewEvent(ctx, approvedReview(1));
    await handleReviewEvent(ctx, approvedReview(2));
    expect(mergeCalls).toBe(2);
  });

  test("keeps the claim on success, so the same head sha is not re-merged", async () => {
    let mergeCalls = 0;
    const ctx = mergeCtx(async () => {
      mergeCalls += 1;
      return { data: {} };
    });
    await handleReviewEvent(ctx, approvedReview(1));
    await handleReviewEvent(ctx, approvedReview(2));
    expect(mergeCalls).toBe(1);
  });
});

describe("CI fix counter and escalation", () => {
  const redCheckRun = JSON.stringify({
    repository: { full_name: "base/repo" },
    check_run: { head_sha: "abc123", pull_requests: [{ number: 7 }] },
  });

  function ciCtx(
    state: ReturnType<typeof makeState>,
    comments: string[],
  ): PrManagerContext {
    return {
      octokit: {
        graphql: async () => ({
          repository: {
            object: {
              statusCheckRollup: {
                state: "FAILURE",
                contexts: {
                  nodes: [
                    {
                      __typename: "CheckRun",
                      name: "build",
                      status: "COMPLETED",
                      conclusion: "FAILURE",
                    },
                  ],
                },
              },
            },
          },
        }),
        rest: {
          repos: {
            getCommit: async () => ({
              data: { author: { login: "centaur-bot" } },
            }),
          },
          pulls: {
            get: async () => ({
              data: prPayload({ headRepoFullName: "base/repo" }),
            }),
          },
          issues: {
            createComment: async (input: { body: string }) => {
              comments.push(input.body);
              return { data: {} };
            },
          },
        },
      },
      options: {
        apiUrl: "http://localhost",
        ciFixMaxAttempts: 3,
        escalationHandle: "maintainer",
        logger: quietLogger,
        // The binding lookup answers 404 (unbound); anything else is
        // non-retryable so the backgrounded fix turn settles off the network.
        fetch: (url: string | URL | Request) =>
          Promise.resolve(
            String(url).includes("/api/pr-bindings/")
              ? bindingMiss()
              : new Response("no", { status: 400 }),
          ),
      },
      state,
      userName: "centaur-bot",
    } as unknown as PrManagerContext;
  }

  test("increments the consecutive-fix counter below the cap", async () => {
    const state = makeState();
    await handleCiEvent(ciCtx(state, []), "check_run", redCheckRun);
    expect(await state.get("centaur-githubbot:pr:base/repo#7")).toMatchObject({
      consecutiveCiFixes: 1,
    });
  });

  test("escalates and fires no fix turn once the cap is reached", async () => {
    const state = makeState();
    await state.set("centaur-githubbot:pr:base/repo#7", {
      consecutiveCiFixes: 3,
    });
    const comments: string[] = [];
    await handleCiEvent(ciCtx(state, comments), "check_run", redCheckRun);
    expect(comments.length).toBe(1);
    expect(comments[0]).toContain("@maintainer");
    // The counter is not bumped past the cap.
    expect(await state.get("centaur-githubbot:pr:base/repo#7")).toMatchObject({
      consecutiveCiFixes: 3,
    });
  });
});

describe("workflow event emission", () => {
  type EmitCall = {
    url: string;
    body: { event_type?: string; correlation_id?: string; payload?: unknown };
  };

  // The PR is deliberately NOT bot-owned (no assignees): workflow events must
  // emit before the owned-PR gate, and the management path then no-ops, so no
  // merge/turn mocks are needed.
  type RollupStub = {
    state?: string;
    contexts?: (Record<string, unknown> | null)[];
    checkRunCountsByState?: { count: number; state: string }[];
    fail?: boolean;
    pageInfo?: { endCursor?: string | null; hasNextPage: boolean };
    partial?: boolean;
    statusContextCountsByState?: { count: number; state: string }[];
  };

  function emitCtx(
    emits: EmitCall[],
    options?: {
      rollupSequence?: RollupStub[];
      workflowEvents?: boolean;
    },
  ): PrManagerContext {
    const sequence = [...(options?.rollupSequence ?? [{ state: "SUCCESS" }])];
    return {
      octokit: {
        graphql: async () => {
          const next = sequence.length > 1 ? sequence.shift()! : sequence[0]!;
          if (next.fail) throw new Error("403 Forbidden");
          const result = {
            repository: {
              object: {
                statusCheckRollup: {
                  state: next.state ?? "SUCCESS",
                  contexts: {
                    nodes: next.contexts ?? [],
                    pageInfo: next.pageInfo,
                    checkRunCountsByState: next.checkRunCountsByState,
                    statusContextCountsByState: next.statusContextCountsByState,
                  },
                },
              },
            },
          };
          if (next.partial) {
            throw Object.assign(new Error("partial GraphQL result"), { data: result });
          }
          return result;
        },
        rest: {
          repos: {
            listPullRequestsAssociatedWithCommit: async () => ({ data: [] }),
          },
          pulls: {
            get: async () => ({
              data: prPayload({ assignees: [], headRepoFullName: "base/repo" }),
            }),
          },
        },
      },
      options: {
        apiUrl: "http://localhost",
        ciSettleConfirmMs: 0,
        logger: quietLogger,
        workflowEvents: options?.workflowEvents ?? true,
        fetch: (url: RequestInfo | URL, init?: RequestInit) => {
          emits.push({
            url: String(url),
            body: JSON.parse(String(init?.body ?? "{}")),
          });
          return Promise.resolve(new Response('{"ok":true}', { status: 200 }));
        },
      },
      state: makeState(),
      userName: "centaur-bot",
    } as unknown as PrManagerContext;
  }

  const completedCheckRun = JSON.stringify({
    action: "completed",
    repository: { full_name: "base/repo" },
    check_run: {
      head_sha: "abc123",
      name: "build",
      conclusion: "success",
      html_url: "https://example.test/checks/1",
      pull_requests: [{ number: 7 }],
    },
  });

  test("emits ci-completed for a completed check run before ownership gating", async () => {
    const emits: EmitCall[] = [];
    await handleCiEvent(emitCtx(emits), "check_run", completedCheckRun);
    await drainBackgroundWork(1_000);
    expect(emits.length).toBe(1);
    const emit = emits[0]!;
    expect(emit.url).toBe("http://localhost/api/workflows/events");
    expect(emit.body).toEqual({
      event_type: "ci-completed",
      correlation_id: "base/repo:abc123",
      payload: { failed: false, failing: [] },
    });
  });

  test("lowercases correlation ids against repository full_name case drift", async () => {
    const emits: EmitCall[] = [];
    await handleCiEvent(
      emitCtx(emits),
      "check_run",
      JSON.stringify({
        action: "completed",
        repository: { full_name: "Base/Repo" },
        check_run: { head_sha: "ABC123", pull_requests: [{ number: 7 }] },
      }),
    );
    await drainBackgroundWork(1_000);
    expect(emits.length).toBe(1);
    expect(emits[0]!.body.correlation_id).toBe("base/repo:abc123");
  });

  test("emits ci-completed for a terminal legacy status", async () => {
    const emits: EmitCall[] = [];
    await handleCiEvent(
      emitCtx(emits, {
        rollupSequence: [
          {
            state: "FAILURE",
            contexts: [
              { __typename: "StatusContext", context: "deploy", state: "FAILURE" },
            ],
          },
        ],
      }),
      "status",
      JSON.stringify({
        repository: { full_name: "base/repo" },
        sha: "abc123",
        state: "failure",
        context: "deploy",
        target_url: "https://example.test/deploy/1",
      }),
    );
    await drainBackgroundWork(1_000);
    expect(emits.length).toBe(1);
    expect(emits[0]!.body).toEqual({
      event_type: "ci-completed",
      correlation_id: "base/repo:abc123",
      payload: { failed: true, failing: ["deploy"] },
    });
  });

  test("does not emit ci-completed while any check is still running", async () => {
    const emits: EmitCall[] = [];
    await handleCiEvent(
      emitCtx(emits, { rollupSequence: [{ state: "PENDING" }] }),
      "check_run",
      completedCheckRun,
    );
    expect(emits.length).toBe(0);
  });

  test("does not treat a failed aggregate as settled while another check is running", async () => {
    const emits: EmitCall[] = [];
    await handleCiEvent(
      emitCtx(emits, {
        rollupSequence: [
          {
            state: "FAILURE",
            contexts: [
              {
                __typename: "CheckRun",
                conclusion: "FAILURE",
                name: "lint",
                status: "COMPLETED",
              },
              {
                __typename: "CheckRun",
                conclusion: null,
                name: "test",
                status: "IN_PROGRESS",
              },
            ],
          },
        ],
      }),
      "check_run",
      completedCheckRun,
    );
    expect(emits.length).toBe(0);
  });

  test("uses the latest check run when a failed job is rerun successfully", async () => {
    const emits: EmitCall[] = [];
    const workflow = {
      workflowRun: { event: "pull_request", workflow: { name: "CI" } },
    };
    await handleCiEvent(
      emitCtx(emits, {
        rollupSequence: [
          {
            state: "SUCCESS",
            contexts: [
              {
                __typename: "CheckRun",
                checkSuite: workflow,
                conclusion: "FAILURE",
                name: "test",
                startedAt: "2026-08-01T10:00:00Z",
                status: "COMPLETED",
              },
              {
                __typename: "CheckRun",
                checkSuite: workflow,
                conclusion: "SUCCESS",
                name: "test",
                startedAt: "2026-08-01T10:05:00Z",
                status: "COMPLETED",
              },
            ],
          },
        ],
      }),
      "check_run",
      completedCheckRun,
    );
    await drainBackgroundWork(1_000);
    expect(emits[0]!.body.payload).toEqual({ failed: false, failing: [] });
  });

  test("fails closed on unreadable context detail while aggregate counts are pending", async () => {
    const emits: EmitCall[] = [];
    await handleCiEvent(
      emitCtx(emits, {
        rollupSequence: [
          {
            state: "FAILURE",
            contexts: [null],
            partial: true,
            checkRunCountsByState: [
              { count: 1, state: "FAILURE" },
              { count: 1, state: "IN_PROGRESS" },
            ],
            statusContextCountsByState: [],
          },
        ],
      }),
      "check_run",
      completedCheckRun,
    );
    expect(emits.length).toBe(0);
  });

  test("does not emit on a momentary green — the registration race", async () => {
    // Push lands, no-op checks complete first, the rollup reads SUCCESS for a
    // few seconds before the real suite registers. The confirm re-read must
    // catch it flipping PENDING and suppress the emission.
    const emits: EmitCall[] = [];
    await handleCiEvent(
      emitCtx(emits, { rollupSequence: [{ state: "SUCCESS" }, { state: "PENDING" }] }),
      "check_run",
      completedCheckRun,
    );
    await drainBackgroundWork(1_000);
    expect(emits.length).toBe(0);
  });

  test("does not emit ci-completed when the rollup is unreadable", async () => {
    // A failed read is UNKNOWN, not settled — emitting would manufacture a
    // green signal out of thin air (e.g. a token missing checks:read).
    const emits: EmitCall[] = [];
    await handleCiEvent(emitCtx(emits, { rollupSequence: [{ fail: true }] }), "check_run", completedCheckRun);
    expect(emits.length).toBe(0);
  });

  test("does not emit for an in-flight check run or a pending status", async () => {
    const emits: EmitCall[] = [];
    const ctx = emitCtx(emits);
    await handleCiEvent(
      ctx,
      "check_run",
      JSON.stringify({
        action: "created",
        repository: { full_name: "base/repo" },
        check_run: { head_sha: "abc123", pull_requests: [] },
      }),
    );
    await handleCiEvent(
      ctx,
      "status",
      JSON.stringify({
        repository: { full_name: "base/repo" },
        sha: "abc123",
        state: "pending",
      }),
    );
    expect(emits.length).toBe(0);
  });

  test("does not emit when workflowEvents is off", async () => {
    const emits: EmitCall[] = [];
    await handleCiEvent(
      emitCtx(emits, { workflowEvents: false }),
      "check_run",
      completedCheckRun,
    );
    expect(emits.length).toBe(0);
  });

  test("emits review-submitted keyed by PR, head sha, and reviewer", async () => {
    const emits: EmitCall[] = [];
    await handleReviewEvent(
      emitCtx(emits),
      JSON.stringify({
        action: "submitted",
        repository: { full_name: "base/repo" },
        pull_request: { number: 7 },
        review: {
          commit_id: "reviewed456",
          id: 123,
          state: "commented",
          user: { login: "chatgpt-codex-connector" },
        },
      }),
    );
    await drainBackgroundWork(1_000);
    expect(emits.length).toBe(1);
    expect(emits[0]!.body).toEqual({
      event_type: "review-submitted",
      correlation_id: "base/repo:pr-7:reviewed456:chatgpt-codex-connector",
      payload: { review_id: 123, state: "commented" },
    });
  });

  test("reviews from different authors get independent rows, never collapsing", async () => {
    const emits: EmitCall[] = [];
    const submittedReview = (id: number, login: string) =>
      JSON.stringify({
        action: "submitted",
        repository: { full_name: "base/repo" },
        pull_request: { number: 7 },
        review: { commit_id: "abc123", id, state: "commented", user: { login } },
      });
    const ctx = emitCtx(emits);
    await handleReviewEvent(ctx, submittedReview(125, "human-reviewer"));
    await handleReviewEvent(ctx, submittedReview(126, "chatgpt-codex-connector"));
    await drainBackgroundWork(1_000);
    expect(emits.map((e) => e.body.correlation_id)).toEqual([
      "base/repo:pr-7:abc123:human-reviewer",
      "base/repo:pr-7:abc123:chatgpt-codex-connector",
    ]);
  });

  test("does not delay PR management while a review event is being delivered", async () => {
    let finishDelivery!: (response: Response) => void;
    const delivery = new Promise<Response>((resolve) => {
      finishDelivery = resolve;
    });
    const ctx = emitCtx([]);
    ctx.options.fetch = () => delivery;

    await handleReviewEvent(
      ctx,
      JSON.stringify({
        action: "submitted",
        repository: { full_name: "base/repo" },
        pull_request: { number: 7 },
        review: {
          commit_id: "abc123",
          id: 127,
          state: "commented",
          user: { login: "human-reviewer" },
        },
      }),
    );

    finishDelivery(new Response("", { status: 200 }));
    await drainBackgroundWork(1_000);
  });

  test("paginates rollup contexts before deduplicating reruns", async () => {
    const ctx = emitCtx([]);
    ctx.octokit.graphql = (async (_query: string, variables: { after?: string }) => ({
      repository: {
        object: {
          statusCheckRollup: {
            state: "SUCCESS",
            contexts:
              variables.after === "page-2"
                ? {
                    nodes: [
                      {
                        __typename: "CheckRun",
                        conclusion: "SUCCESS",
                        name: "test",
                        startedAt: "2026-08-01T10:05:00Z",
                        status: "COMPLETED",
                      },
                    ],
                    pageInfo: { hasNextPage: false, endCursor: null },
                  }
                : {
                    nodes: [
                      {
                        __typename: "CheckRun",
                        conclusion: "FAILURE",
                        name: "test",
                        startedAt: "2026-08-01T10:00:00Z",
                        status: "COMPLETED",
                      },
                    ],
                    pageInfo: { hasNextPage: true, endCursor: "page-2" },
                  },
          },
        },
      },
    })) as typeof ctx.octokit.graphql;

    await expect(fetchCiEvaluation(ctx, "base", "repo", "abc123")).resolves.toEqual({
      failed: false,
      failingNames: [],
      settled: true,
    });
  });

  test("retries transient workflow event delivery", async () => {
    let attempts = 0;
    await emitWorkflowEvent(
      {
        apiUrl: "http://localhost",
        token: "test-token",
        webhookSecret: "test-secret",
        fetch: () => {
          attempts += 1;
          return Promise.resolve(
            new Response("", { status: attempts === 1 ? 503 : 200 }),
          );
        },
      },
      {
        correlationId: "base/repo:abc123",
        eventType: "ci-completed",
        payload: {},
      },
    );
    expect(attempts).toBe(2);
  });
});

describe("management turn reaction ack", () => {
  const submittedReview = (state: string, nodeId?: string) =>
    JSON.stringify({
      action: "submitted",
      repository: { full_name: "base/repo" },
      pull_request: { number: 7 },
      review: {
        id: 55,
        node_id: nodeId,
        state,
        user: { login: "reviewer" },
        author_association: "MEMBER",
      },
    });

  function reviewCtx(reactions: { subjectId: string; content: string }[]) {
    return {
      octokit: {
        graphql: async (
          _query: string,
          vars: { subjectId: string; content: string },
        ) => {
          reactions.push({ subjectId: vars.subjectId, content: vars.content });
          return {};
        },
        rest: {
          pulls: {
            get: async () => ({
              data: prPayload({ headRepoFullName: "base/repo" }),
            }),
            merge: async () => ({ data: {} }),
          },
          git: { deleteRef: async () => ({ data: {} }) },
        },
      },
      options: {
        apiUrl: "http://localhost",
        deleteBranchOnMerge: false,
        logger: quietLogger,
        // Non-retryable so the backgrounded turn settles off the network.
        fetch: () => Promise.resolve(new Response("no", { status: 400 })),
      },
      state: makeState(),
      userName: "centaur-bot",
    } as unknown as PrManagerContext;
  }

  test("acks a changes-requested review with eyes on the review itself, settling when the turn fails", async () => {
    const reactions: { subjectId: string; content: string }[] = [];
    await handleReviewEvent(
      reviewCtx(reactions),
      submittedReview("changes_requested", "PRR_test55"),
    );
    // The working ack lands before the management turn runs.
    expect(reactions).toContainEqual({ subjectId: "PRR_test55", content: "EYES" });
    await drainBackgroundWork(5_000);
    expect(reactions).toContainEqual({
      subjectId: "PRR_test55",
      content: "CONFUSED",
    });
  });

  test("does not react on an approved review (deterministic merge, no work turn)", async () => {
    const reactions: { subjectId: string; content: string }[] = [];
    await handleReviewEvent(
      reviewCtx(reactions),
      submittedReview("approved", "PRR_test55"),
    );
    await drainBackgroundWork(5_000);
    expect(reactions).toEqual([]);
  });

  test("stays quiet when the payload carries no review node id", async () => {
    const reactions: { subjectId: string; content: string }[] = [];
    await handleReviewEvent(reviewCtx(reactions), submittedReview("changes_requested"));
    await drainBackgroundWork(5_000);
    expect(reactions).toEqual([]);
  });

  const untrustedReview = (association?: string) =>
    JSON.stringify({
      action: "submitted",
      repository: { full_name: "base/repo" },
      pull_request: { number: 7 },
      review: {
        id: 77,
        node_id: "PRR_test77",
        state: "changes_requested",
        user: { login: "drive-by" },
        ...(association ? { author_association: association } : {}),
      },
    });

  test("ignores a changes-requested review from an untrusted association", async () => {
    const reactions: { subjectId: string; content: string }[] = [];
    const ctx = reviewCtx(reactions);
    await handleReviewEvent(ctx, untrustedReview("NONE"));
    await handleReviewEvent(ctx, untrustedReview());
    await drainBackgroundWork(5_000);
    expect(reactions).toEqual([]);
  });

  test("honors a custom allowedAuthorAssociations list and the wildcard", async () => {
    const reactions: { subjectId: string; content: string }[] = [];
    const restricted = reviewCtx(reactions);
    restricted.options.allowedAuthorAssociations = ["OWNER"];
    await handleReviewEvent(restricted, untrustedReview("CONTRIBUTOR"));
    await drainBackgroundWork(5_000);
    expect(reactions).toEqual([]);

    const open = reviewCtx(reactions);
    open.options.allowedAuthorAssociations = ["*"];
    await handleReviewEvent(open, untrustedReview("NONE"));
    expect(reactions).toContainEqual({
      subjectId: "PRR_test77",
      content: "EYES",
    });
    await drainBackgroundWork(5_000);
  });
});

describe("PR session bindings", () => {
  const redCheckRun = JSON.stringify({
    repository: { full_name: "base/repo" },
    check_run: { head_sha: "abc123", pull_requests: [{ number: 7 }] },
  });
  const PR_URL = "https://github.com/base/repo/pull/7";
  const FAILURE_URL = "https://ci.example.test/runs/1";

  type Binding = {
    managed: boolean;
    closed: boolean;
    head_sha?: string;
    pr_node_id?: string;
    revision?: number;
  };
  type Call = { body: unknown; method: string; path: string; search?: string };

  const NODE_ID = "PR_node_1";
  const UPDATED_AT = "2026-11-23T10:00:00Z";

  function bindingJson(binding: Binding, headSha = "abc123") {
    return {
      repository: "base/repo",
      number: 7,
      thread_key: "slack:T0:C0:1700000000.000100",
      pr_node_id: NODE_ID,
      revision: 1,
      head_sha: headSha,
      ...binding,
    };
  }

  /** In-memory api-rs: records every call, and any session (agent turn) call. */
  function makeApi(input: {
    binding: Binding | null;
    lookupStatus?: number;
    lookupBody?: unknown;
    // The binding GET answers with this instead (a generic 404, say).
    lookupResponse?: () => Response;
    // One entry per /state call, consumed in order: a failing HTTP status or a
    // network error. Calls after the list is used up succeed.
    stateFailures?: (number | "network" | "conflict")[];
    // One entry per binding GET, consumed in order; later GETs succeed.
    lookupFailures?: number[];
    // Another writer gets in before this many of the first /state calls, so the
    // revision they were derived from is stale by the time they arrive.
    interveneBeforeState?: number;
    // The binding is stored under other coordinates than the events carry (the
    // repository was renamed): found by node id, and its routes use these.
    storedAs?: { repository: string; number: number };
  }) {
    let interventions = input.interveneBeforeState ?? 0;
    const lookupFailures = [...(input.lookupFailures ?? [])];
    const stateFailures = [...(input.stateFailures ?? [])];
    // The binding as api-rs holds it: a state write moves its head.
    const current = input.binding ? { ...input.binding } : null;
    // api-rs's dedupe authority: one notice per (pr_node_id, head_sha).
    const noticed = new Set<string>();
    const notices: string[] = [];
    const calls: Call[] = [];
    const sessionCalls: string[] = [];
    const sessionBodies: string[] = [];
    const fetch = async (url: string | URL | Request, init?: RequestInit) => {
      const path = new URL(String(url)).pathname;
      const method = init?.method ?? "GET";
      if (path.startsWith("/api/session/")) {
        sessionCalls.push(path);
        sessionBodies.push(String(init?.body ?? ""));
        return new Response("no", { status: 400 });
      }
      const body = init?.body ? JSON.parse(String(init.body)) : undefined;
      calls.push({ body, method, path, search: new URL(String(url)).search });
      if (input.storedAs && method === "POST") {
        const base = `/api/pr-bindings/${input.storedAs.repository}/${input.storedAs.number}/`;
        if (!path.startsWith(base)) return new Response("not found", { status: 404 });
      }
      const stored = (json: object) => ({ ...json, ...(input.storedAs ?? {}) });
      if (method === "GET") {
        const byNodeId = new URL(String(url)).searchParams.get("node_id");
        if (
          input.storedAs &&
          !(byNodeId && byNodeId === (current?.pr_node_id ?? NODE_ID)) &&
          path !== `/api/pr-bindings/${input.storedAs.repository}/${input.storedAs.number}`
        ) {
          return bindingMiss();
        }
        const lookupFailure = lookupFailures.shift();
        if (lookupFailure) return new Response("x", { status: lookupFailure });
        if (input.lookupStatus) return new Response("x", { status: input.lookupStatus });
        if (input.lookupResponse) return input.lookupResponse();
        if ("lookupBody" in input) return Response.json(input.lookupBody);
        if (!input.binding) return bindingMiss();
        // Like api-rs: a lookup by node id finds that PR's binding or misses.
        if (byNodeId && (current?.pr_node_id ?? NODE_ID) !== byNodeId) return bindingMiss();
        return Response.json(stored(bindingJson(current ?? input.binding)));
      }
      if (path.endsWith("/ci-failures")) {
        const report = body as { pr_node_id: string; head_sha: string };
        const key = `${report.pr_node_id}:${report.head_sha}`;
        const notice = noticed.has(key) ? "duplicate" : "posted";
        noticed.add(key);
        notices.push(notice);
        return Response.json({ notice });
      }
      const failure = stateFailures.shift();
      if (failure === "network") throw new TypeError("fetch failed");
      if (failure === "conflict") {
        return Response.json(
          { error: { code: "binding_state_conflict", message: "contradicts the stored state" } },
          { status: 409 },
        );
      }
      if (failure) return new Response("unavailable", { status: failure });
      if (current && interventions > 0) {
        interventions -= 1;
        current.revision = (current.revision ?? 1) + 1;
      }
      const write = body as {
        closed?: boolean;
        expected_revision?: number;
        head_sha?: string;
        managed?: boolean;
      };
      // Compare-and-set: a write derived from an older revision conflicts.
      if (
        current &&
        write.expected_revision !== undefined &&
        write.expected_revision !== (current.revision ?? 1)
      ) {
        return Response.json(
          { error: { code: "binding_state_conflict", message: "stale revision" } },
          { status: 409 },
        );
      }
      if (current) {
        if (typeof write.head_sha === "string") current.head_sha = write.head_sha;
        if (typeof write.managed === "boolean") current.managed = write.managed;
        if (typeof write.closed === "boolean") current.closed = write.closed;
        current.revision = (current.revision ?? 1) + 1;
      }
      return Response.json({
        ...stored(bindingJson(current ?? { managed: false, closed: false })),
        applied: true,
      });
    };
    const posts = (suffix: string) =>
      calls.filter((c) => c.method === "POST" && c.path.endsWith(suffix));
    return { calls, current: () => current, fetch, notices, posts, sessionBodies, sessionCalls };
  }

  function makeCtx(input: {
    api: ReturnType<typeof makeApi>;
    assignees?: { login: string }[];
    checkNode?: Record<string, unknown>;
    // The rollup's check detail is unreadable, so the evaluation falls back to
    // aggregate counts: red, but with no failing name or link.
    aggregateOnly?: boolean;
    // Some, but not all, of the rollup's check nodes are readable.
    partial?: boolean;
    // What fresh reads of the signed event's check return (or "throw").
    github?: {
      // The head's latest check runs for the event's check name.
      checkRuns?: Record<string, unknown>[] | "throw";
      combinedStatus?: Record<string, unknown> | "throw";
      // The head's runs of the event's workflow.
      workflowRuns?: Record<string, unknown>[] | "throw";
    };
    // Head the PR reports on its Nth fetch (the last entry repeats).
    heads?: string[];
    nodeId?: string;
    // Overrides for the Nth PR read (0-based): the owns gate, then the fresh
    // read before a report, then the read after a head repair.
    perRead?: Record<number, { fails?: boolean; nodeId?: string; state?: string; updatedAt?: string }>;
    prFetchFails?: boolean;
    prState?: string;
    state?: ReturnType<typeof makeState>;
    settled?: boolean;
    updatedAt?: string | null;
  }) {
    const sleeps: number[] = [];
    const heads = input.heads ?? ["abc123"];
    let fetches = 0;
    const node = input.checkNode ?? {
      __typename: "CheckRun",
      name: "build",
      status: input.settled === false ? "IN_PROGRESS" : "COMPLETED",
      conclusion: input.settled === false ? null : "FAILURE",
      detailsUrl: FAILURE_URL,
      permalink: "https://github.com/base/repo/runs/1",
      checkSuite: { app: { slug: "ci", databaseId: CHECK_APP_ID } },
    };
    const comments: string[] = [];
    // Every GitHub read, in order, so a test can see which one came last.
    const reads: string[] = [];
    const checkQueries: Record<string, unknown>[] = [];
    const fresh = (kind: string, value: Record<string, unknown> | "throw" | undefined) =>
      async () => {
        reads.push(kind);
        if (value === undefined || value === "throw") throw new Error("not confirmable");
        return { data: value };
      };
    const ctx = {
      octokit: {
        // Like the client's paginate: page until a short page, mapping each.
        paginate: async (
          route: (params: Record<string, unknown>) => Promise<unknown>,
          params: { per_page?: number },
          map: (response: unknown) => unknown[],
        ) => {
          const items: unknown[] = [];
          for (let page = 1; ; page += 1) {
            const batch = map(await route({ ...params, page }));
            items.push(...batch);
            if (batch.length < (params.per_page ?? 30)) return items;
          }
        },
        graphql: async () => ({
          repository: {
            object: {
              statusCheckRollup: {
                state: input.settled === false ? "PENDING" : "FAILURE",
                contexts: {
                  nodes: input.aggregateOnly ? [null] : input.partial ? [node, null] : [node],
                  pageInfo: { hasNextPage: false, endCursor: null },
                  ...(input.aggregateOnly || input.partial
                    ? {
                        checkRunCountsByState: [{ count: input.partial ? 2 : 1, state: "FAILURE" }],
                        statusContextCountsByState: [],
                      }
                    : {}),
                },
              },
            },
          },
        }),
        rest: {
          repos: {
            getCommit: async () => ({ data: { author: { login: "centaur-bot" } } }),
            // Honours `per_page` like GitHub: 30 contexts unless more are asked for.
            getCombinedStatusForRef: async (args: { page?: number; per_page?: number }) => {
              reads.push("status");
              const combined = input.github?.combinedStatus;
              if (combined === undefined || combined === "throw") {
                throw new Error("not confirmable");
              }
              const all = combined.statuses as unknown[];
              const size = args.per_page ?? 30;
              const start = ((args.page ?? 1) - 1) * size;
              return { data: { ...combined, statuses: all.slice(start, start + size) } };
            },
          },
          checks: {
            listForRef: async (args: Record<string, unknown>) => {
              reads.push("check");
              checkQueries.push(args);
              const runs = input.github?.checkRuns;
              if (runs === undefined || runs === "throw") throw new Error("not confirmable");
              return { data: { check_runs: runs } };
            },
          },
          actions: {
            listWorkflowRuns: async () => {
              reads.push("workflow");
              const runs = input.github?.workflowRuns;
              if (runs === undefined || runs === "throw") throw new Error("not confirmable");
              return { data: { workflow_runs: runs } };
            },
          },
          pulls: {
            get: async () => {
              reads.push("pr");
              const head = heads[Math.min(fetches, heads.length - 1)];
              const read = input.perRead?.[fetches] ?? {};
              fetches += 1;
              if (input.prFetchFails || read.fails) throw new Error("github unavailable");
              return {
                data: {
                  ...prPayload({
                    assignees: input.assignees,
                    headRepoFullName: "base/repo",
                    headSha: head,
                  }),
                  html_url: PR_URL,
                  // Mixed case on purpose: the report lowercases it.
                  base: { repo: { full_name: "Base/Repo" } },
                  node_id: read.nodeId ?? input.nodeId ?? NODE_ID,
                  state: read.state ?? input.prState ?? "open",
                  updated_at:
                    input.updatedAt === null
                      ? undefined
                      : (read.updatedAt ?? input.updatedAt ?? UPDATED_AT),
                },
              };
            },
          },
          issues: {
            createComment: async (c: { body: string }) => {
              comments.push(c.body);
              return { data: {} };
            },
          },
        },
      },
      options: {
        apiUrl: "http://localhost",
        ciFixMaxAttempts: 3,
        escalationHandle: "maintainer",
        logger: quietLogger,
        fetch: input.api.fetch,
      },
      state: input.state ?? makeState(),
      userName: "centaur-bot",
      // Instant timers: the retry delays are recorded, not waited.
      sleep: async (ms: number) => {
        sleeps.push(ms);
      },
    } as unknown as PrManagerContext;
    return { checkQueries, comments, ctx, reads, sleeps };
  }

  const PR_KEY = "centaur-githubbot:pr:base/repo#7";
  /** githubbot keeps no notice claim of its own: api-rs is the dedupe authority. */
  const noNoticeClaim = (state: ReturnType<typeof makeState>) =>
    expect(state.keys().filter((key) => key.includes("ci-notice"))).toEqual([]);

  describe("failed CI", () => {
    describe("a renamed repository", () => {
      const stored = { repository: "acme/old-name", number: 7 };

      test("the binding is found by node id, and every later call uses its stored coordinates", async () => {
        const api = makeApi({ binding: { managed: true, closed: false, head_sha: "old111" }, storedAs: stored });
        const { ctx } = makeCtx({ api });
        await handleCiEvent(ctx, "check_run", redCheckRun);
        await drainBackgroundWork(2000);
        const gets = api.calls.filter((c) => c.method === "GET");
        expect(gets[0]?.path).toBe("/api/pr-bindings/base/repo/7");
        expect(gets[0]?.search).toBe(`?node_id=${NODE_ID}&authoritative=true`);
        // The head repair and the report both go to the stored coordinates.
        expect(
          api.calls.filter((c) => c.method === "POST").map((c) => c.path),
        ).toEqual([
          "/api/pr-bindings/acme/old-name/7/state",
          "/api/pr-bindings/acme/old-name/7/ci-failures",
        ]);
        expect(api.notices).toEqual(["posted"]);
        expect(api.sessionCalls).toEqual([]);
        // The route keeps the stored coordinates, but the report says where the
        // PR lives now, with the link from the same fresh read.
        expect(api.posts("/ci-failures")[0]?.body).toMatchObject({
          current_repository: "base/repo",
          current_number: 7,
          html_url: PR_URL,
        });
      });

      test("lifecycle events find it from the payload's node id and write to the stored coordinates", async () => {
        const api = makeApi({ binding: { managed: true, closed: false }, storedAs: stored });
        const { ctx } = makeCtx({ api, settled: false, prState: "closed" });
        await handlePullRequestEvent(
          ctx,
          JSON.stringify({
            action: "closed",
            repository: { full_name: "base/repo" },
            pull_request: { number: 7, node_id: NODE_ID, head: { sha: "old000" } },
          }),
        );
        expect(api.calls.find((c) => c.method === "GET")?.search).toBe(`?node_id=${NODE_ID}`);
        expect(api.posts("/state").map((c) => c.path)).toEqual(["/api/pr-bindings/acme/old-name/7/state"]);
        expect(api.current()).toMatchObject({ closed: true });
      });

      test("a conflict re-reads by node id too, and still writes to the stored coordinates", async () => {
        const api = makeApi({
          binding: { managed: true, closed: false },
          storedAs: stored,
          interveneBeforeState: 1,
        });
        const { ctx } = makeCtx({ api, settled: false, prState: "closed" });
        await handlePullRequestEvent(
          ctx,
          JSON.stringify({
            action: "closed",
            repository: { full_name: "base/repo" },
            pull_request: { number: 7, node_id: NODE_ID, head: { sha: "old000" } },
          }),
        );
        expect(api.posts("/state")).toHaveLength(2);
        expect(api.calls.filter((c) => c.method === "GET").every((c) => c.search === `?node_id=${NODE_ID}`)).toBe(true);
        expect(api.current()).toMatchObject({ closed: true });
      });

      test("a binding for another PR under the same coordinates is still not this PR's", async () => {
        const api = makeApi({
          binding: { managed: true, closed: false, pr_node_id: "PR_other" },
          storedAs: { repository: "base/repo", number: 7 },
        });
        const state = makeState();
        await handleCiEvent(makeCtx({ api, state }).ctx, "check_run", redCheckRun);
        await drainBackgroundWork(2000);
        expect(api.posts("/ci-failures")).toHaveLength(0);
        expect(api.sessionCalls.length).toBeGreaterThan(0);
      });
    });

    describe("authoritative binding lookups", () => {
      const eventGets = (api: ReturnType<typeof makeApi>) =>
        api.calls.filter((c) => c.method === "GET").map((c) => c.search);

      test("the red-CI lookup asks for an authoritative answer", async () => {
        const api = makeApi({ binding: null });
        await handleCiEvent(makeCtx({ api }).ctx, "check_run", redCheckRun);
        await drainBackgroundWork(2000);
        expect(eventGets(api)).toEqual([`?node_id=${NODE_ID}&authoritative=true`]);
      });

      test("an assignment of the bot asks for one; every other lifecycle lookup does not", async () => {
        const lifecycle = (action: string, assignee?: string) =>
          JSON.stringify({
            action,
            repository: { full_name: "base/repo" },
            pull_request: { number: 7, node_id: NODE_ID, head: { sha: "old000" } },
            ...(assignee ? { assignee: { login: assignee } } : {}),
          });
        const cases: [string, string | undefined, string][] = [
          ["assigned", "centaur-bot", `?node_id=${NODE_ID}&authoritative=true`],
          ["assigned", "a-human", `?node_id=${NODE_ID}`],
          ["unassigned", "centaur-bot", `?node_id=${NODE_ID}`],
          ["closed", undefined, `?node_id=${NODE_ID}`],
          ["reopened", undefined, `?node_id=${NODE_ID}`],
          ["synchronize", undefined, `?node_id=${NODE_ID}`],
        ];
        for (const [action, assignee, expected] of cases) {
          const api = makeApi({ binding: null });
          await handlePullRequestEvent(
            makeCtx({ api, settled: false }).ctx,
            lifecycle(action, assignee),
          );
          expect(eventGets(api)).toEqual([expected]);
        }
      });

      test("re-reads after a conflict do not ask for one", async () => {
        const api = makeApi({
          binding: { managed: true, closed: false, head_sha: "old111" },
          interveneBeforeState: 1,
        });
        await handleCiEvent(makeCtx({ api }).ctx, "check_run", redCheckRun);
        const gets = eventGets(api);
        // The red-path lookup is authoritative; the head repair's re-read is not.
        expect(gets).toEqual([
          `?node_id=${NODE_ID}&authoritative=true`,
          `?node_id=${NODE_ID}`,
        ]);
      });
    });

    test("a 404 is unbound at once: no waiting, one lookup", async () => {
      const api = makeApi({ binding: null });
      const state = makeState();
      const { ctx, sleeps } = makeCtx({ api, state });
      await handleCiEvent(ctx, "check_run", redCheckRun);
      await drainBackgroundWork(2000);
      expect(sleeps).toEqual([]);
      expect(api.calls.filter((c) => c.method === "GET")).toHaveLength(1);
      expect(api.sessionCalls.length).toBeGreaterThan(0);
      expect(await state.get(PR_KEY)).toMatchObject({ consecutiveCiFixes: 1 });
    });

    test("an unbound red PR still gets the fix turn", async () => {
      const api = makeApi({ binding: null });
      const state = makeState();
      const { ctx } = makeCtx({ api, state });
      await handleCiEvent(ctx, "check_run", redCheckRun);
      await drainBackgroundWork(2000);
      expect(api.sessionCalls.length).toBeGreaterThan(0);
      expect(await state.get(PR_KEY)).toMatchObject({ consecutiveCiFixes: 1 });
      expect(api.posts("/ci-failures")).toHaveLength(0);
    });

    test("a bound, managed red PR reports once, with the failing check and its link", async () => {
      const api = makeApi({ binding: { managed: true, closed: false } });
      const state = makeState();
      const { ctx, comments } = makeCtx({ api, state });
      await handleCiEvent(ctx, "check_run", redCheckRun);
      await drainBackgroundWork(2000);
      const reports = api.posts("/api/pr-bindings/base/repo/7/ci-failures");
      expect(reports.map((r) => r.body)).toEqual([
        {
          pr_node_id: NODE_ID,
          observed_updated_at: UPDATED_AT,
          head_sha: "abc123",
          current_head_sha: "abc123",
          current_repository: "base/repo",
          current_number: 7,
          open: true,
          assigned: true,
          check_name: "build",
          details_url: FAILURE_URL,
          html_url: PR_URL,
        },
      ]);
      // No agent turn, no escalation, and the fix counter is untouched.
      expect(api.sessionCalls).toEqual([]);
      expect(comments).toEqual([]);
      expect(await state.get(PR_KEY)).toBeUndefined();
    });

    test("falls back to the check's permalink, and to a status target url", async () => {
      for (const [node, expected] of [
        [
          { __typename: "CheckRun", name: "build", status: "COMPLETED", conclusion: "FAILURE", permalink: "https://github.com/base/repo/runs/9" },
          { name: "build", url: "https://github.com/base/repo/runs/9" },
        ],
        [
          { __typename: "StatusContext", context: "deploy", state: "FAILURE", targetUrl: "https://ci.example.test/deploy/3" },
          { name: "deploy", url: "https://ci.example.test/deploy/3" },
        ],
      ] as const) {
        const api = makeApi({ binding: { managed: true, closed: false } });
        const { ctx } = makeCtx({ api, checkNode: { ...node } });
        await handleCiEvent(ctx, "check_run", redCheckRun);
        expect(api.posts("/ci-failures")[0]?.body).toMatchObject({
          check_name: expected.name,
          details_url: expected.url,
        });
      }
    });

    test("omits the link when GitHub gives none", async () => {
      const api = makeApi({ binding: { managed: true, closed: false } });
      const { ctx } = makeCtx({
        api,
        checkNode: { __typename: "CheckRun", name: "build", status: "COMPLETED", conclusion: "FAILURE" },
      });
      await handleCiEvent(ctx, "check_run", redCheckRun);
      const body = api.posts("/ci-failures")[0]?.body as Record<string, unknown>;
      expect(body.check_name).toBe("build");
      expect("details_url" in body).toBe(false);
    });

    describe("a details URL api-rs would drop never shadows a usable link", () => {
      const overLong = `https://ci.example.test/${"a".repeat(2_048)}`;
      const permalink = "https://github.com/base/repo/runs/9";
      const failing = (extra: Record<string, unknown>) => ({
        __typename: "CheckRun",
        name: "build",
        status: "COMPLETED",
        conclusion: "FAILURE",
        ...extra,
      });
      const sent = async (checkNode: Record<string, unknown>) => {
        const api = makeApi({ binding: { managed: true, closed: false } });
        await handleCiEvent(makeCtx({ api, checkNode }).ctx, "check_run", redCheckRun);
        return api.posts("/ci-failures")[0]?.body as Record<string, unknown>;
      };

      test("an http:// detailsUrl alongside an HTTPS permalink sends the permalink", async () => {
        const body = await sent(
          failing({ detailsUrl: "http://ci.example.test/runs/1", permalink }),
        );
        expect(body.details_url).toBe(permalink);
      });

      test("an over-long detailsUrl alongside an HTTPS permalink sends the permalink", async () => {
        const body = await sent(failing({ detailsUrl: overLong, permalink }));
        expect(body.details_url).toBe(permalink);
      });

      test("a detailsUrl the Slack renderer would drop yields to the permalink", async () => {
        for (const detailsUrl of [
          "https://ci.example.test/a|b",
          "https://ci.example.test/has space",
          "https://ci.example.test/a<b",
          "https://ci.example.test/a>b",
          "https://ci.example.test/a\tb",
        ]) {
          const body = await sent(failing({ detailsUrl, permalink }));
          expect(body.details_url).toBe(permalink);
        }
      });

      test("a usable detailsUrl still wins over the permalink", async () => {
        const body = await sent(
          failing({ detailsUrl: "https://ci.example.test/runs/1", permalink }),
        );
        expect(body.details_url).toBe("https://ci.example.test/runs/1");
      });

      test("with no usable candidate, no details_url is sent", async () => {
        for (const node of [
          failing({ detailsUrl: "http://ci.example.test/runs/1", permalink: "http://github.com/x" }),
          failing({ detailsUrl: overLong, permalink: "javascript:alert(1)" }),
          { __typename: "StatusContext", context: "deploy", state: "FAILURE", targetUrl: "http://ci.example.test/d" },
        ]) {
          const body = await sent(node);
          expect("details_url" in body).toBe(false);
          expect(body.check_name).toBeTruthy();
        }
      });
    });

    test("a repeat delivery for the same head sends at most one notice, because api-rs answers duplicate", async () => {
      const api = makeApi({ binding: { managed: true, closed: false } });
      const state = makeState();
      const { ctx } = makeCtx({ api, state });
      await handleCiEvent(ctx, "check_run", redCheckRun);
      await handleCiEvent(ctx, "check_run", redCheckRun);
      // githubbot reports every eligible delivery; api-rs decides.
      expect(api.posts("/ci-failures")).toHaveLength(2);
      expect(api.notices).toEqual(["posted", "duplicate"]);
      noNoticeClaim(state);
    });

    test("a repo, number and head reused with a different PR node id is not suppressed by githubbot", async () => {
      const state = makeState();
      const first = makeApi({ binding: { managed: true, closed: false, pr_node_id: "PR_old" } });
      await handleCiEvent(makeCtx({ api: first, state, nodeId: "PR_old" }).ctx, "check_run", redCheckRun);
      const reused = makeApi({ binding: { managed: true, closed: false, pr_node_id: "PR_new" } });
      await handleCiEvent(makeCtx({ api: reused, state, nodeId: "PR_new" }).ctx, "check_run", redCheckRun);
      expect(first.notices).toEqual(["posted"]);
      expect(reused.notices).toEqual(["posted"]);
      expect(reused.posts("/ci-failures")[0]?.body).toMatchObject({ pr_node_id: "PR_new" });
    });

    test("a stale head is not reported, whether stale at the event or by report time", async () => {
      const api = makeApi({ binding: { managed: true, closed: false } });
      // The failing head is no longer the PR's head.
      await handleCiEvent(makeCtx({ api, heads: ["def456"] }).ctx, "check_run", redCheckRun);
      // The head moves between the first read and the report.
      await handleCiEvent(makeCtx({ api, heads: ["abc123", "def456"] }).ctx, "check_run", redCheckRun);
      expect(api.posts("/ci-failures")).toHaveLength(0);
      expect(api.sessionCalls).toEqual([]);
    });

    test("a closed binding, or one not managed, is not reported", async () => {
      for (const binding of [
        { managed: true, closed: true },
        { managed: false, closed: false },
        { managed: false, closed: true },
      ]) {
        const api = makeApi({ binding });
        const state = makeState();
        const { ctx } = makeCtx({ api, state });
        await handleCiEvent(ctx, "check_run", redCheckRun);
        await drainBackgroundWork(2000);
        expect(api.posts("/ci-failures")).toHaveLength(0);
        // Never falls back to the unbound fix turn either.
        expect(api.sessionCalls).toEqual([]);
        expect(await state.get(PR_KEY)).toBeUndefined();
      }
    });

    test("a closed PR is reported as not open, for api-rs to decide and record", async () => {
      const api = makeApi({ binding: { managed: true, closed: false } });
      const { ctx } = makeCtx({ api, prState: "closed" });
      await handleCiEvent(ctx, "check_run", redCheckRun);
      expect(api.posts("/ci-failures")[0]?.body).toMatchObject({ open: false, assigned: true });
    });

    test("an unassigned PR never reaches the bound path, and takes no claim", async () => {
      const api = makeApi({ binding: { managed: true, closed: false } });
      const state = makeState();
      const { ctx } = makeCtx({ api, state, assignees: [{ login: "a-human" }] });
      await handleCiEvent(ctx, "check_run", redCheckRun);
      expect(api.posts("/ci-failures")).toHaveLength(0);
      noNoticeClaim(state);
      expect(api.sessionCalls).toEqual([]);
    });

    test("fails closed when the fresh PR read fails: no report, no claim", async () => {
      const api = makeApi({ binding: { managed: true, closed: false } });
      const state = makeState();
      const { ctx } = makeCtx({ api, state, heads: ["abc123"] });
      // First read (owns gate) succeeds; the fresh read before the report does not.
      let reads = 0;
      const pulls = (ctx.octokit.rest.pulls as unknown as { get: () => Promise<unknown> });
      const get = pulls.get;
      pulls.get = async () => {
        reads += 1;
        if (reads > 1) throw new Error("github unavailable");
        return get();
      };
      await handleCiEvent(ctx, "check_run", redCheckRun);
      expect(api.posts("/ci-failures")).toHaveLength(0);
      noNoticeClaim(state);
      expect(api.sessionCalls).toEqual([]);
    });

    test("a fresh read without updated_at is not reportable", async () => {
      const api = makeApi({ binding: { managed: true, closed: false } });
      await handleCiEvent(makeCtx({ api, updatedAt: null }).ctx, "check_run", redCheckRun);
      expect(api.posts("/ci-failures")).toHaveLength(0);
    });

    test("records the fresh head first when CI beats the synchronize event, then sends one notice", async () => {
      const api = makeApi({ binding: { managed: true, closed: false, head_sha: "old111" } });
      const { ctx } = makeCtx({ api });
      await handleCiEvent(ctx, "check_run", redCheckRun);
      const writes = api.calls.filter((c) => c.method === "POST").map((c) => [c.path.split("/").pop(), c.body]);
      expect(writes).toEqual([
        [
          "state",
          { pr_node_id: NODE_ID, observed_updated_at: UPDATED_AT, expected_revision: 1, head_sha: "abc123" },
        ],
        ["ci-failures", expect.objectContaining({ head_sha: "abc123", current_head_sha: "abc123" })],
      ]);
      // No synchronize event follows. A repeat delivery needs no second head
      // write, and api-rs answers duplicate: still one notice.
      await handleCiEvent(ctx, "check_run", redCheckRun);
      expect(api.posts("/state")).toHaveLength(1);
      expect(api.notices).toEqual(["posted", "duplicate"]);
    });

    test("writes no head when the binding already has the current one", async () => {
      const api = makeApi({ binding: { managed: true, closed: false } });
      await handleCiEvent(makeCtx({ api }).ctx, "check_run", redCheckRun);
      expect(api.posts("/state")).toHaveLength(0);
    });

    test("an unrecordable head means no report and no claim", async () => {
      const api = makeApi({
        binding: { managed: true, closed: false, head_sha: "old111" },
        stateFailures: [409],
      });
      const state = makeState();
      await handleCiEvent(makeCtx({ api, state }).ctx, "check_run", redCheckRun);
      expect(api.posts("/ci-failures")).toHaveLength(0);
      noNoticeClaim(state);
    });

    test("a binding for another PR (recycled repo and number) leaves this PR on the unbound flow", async () => {
      const api = makeApi({ binding: { managed: true, closed: false, pr_node_id: "PR_other" } });
      const state = makeState();
      const { ctx } = makeCtx({ api, state });
      await handleCiEvent(ctx, "check_run", redCheckRun);
      await drainBackgroundWork(2000);
      // Exactly today's unbound behaviour: the settled claim, the fix turn and
      // the attempt counter. Nothing is reported to the other PR's binding.
      expect(api.posts("/ci-failures")).toHaveLength(0);
      expect(api.posts("/state")).toHaveLength(0);
      expect(api.sessionCalls.length).toBeGreaterThan(0);
      expect(await state.get(PR_KEY)).toMatchObject({ consecutiveCiFixes: 1 });
      expect(await state.get("centaur-githubbot:ci-settled:base/repo#7:abc123")).toBeDefined();
      noNoticeClaim(state);
    });

    test("a recycled repo and number escalates at the cap like any unbound PR", async () => {
      const api = makeApi({ binding: { managed: true, closed: false, pr_node_id: "PR_other" } });
      const state = makeState();
      await state.set(PR_KEY, { consecutiveCiFixes: 3 });
      const { ctx, comments } = makeCtx({ api, state });
      await handleCiEvent(ctx, "check_run", redCheckRun);
      expect(comments).toHaveLength(1);
      expect(api.sessionCalls).toEqual([]);
    });

    describe("after a head repair", () => {
      const stale = { managed: true, closed: false, head_sha: "old111" };
      const reported = (api: ReturnType<typeof makeApi>) => api.posts("/ci-failures");

      test("reads the PR again and builds the whole report from that read", async () => {
        const api = makeApi({ binding: stale });
        const { ctx } = makeCtx({
          api,
          perRead: { 2: { updatedAt: "2026-11-23T10:05:00Z" } },
        });
        await handleCiEvent(ctx, "check_run", redCheckRun);
        expect(api.posts("/state")).toHaveLength(1);
        expect(reported(api).map((c) => c.body)).toEqual([
          expect.objectContaining({
            // The repair carries the first read's time; the report the final one's.
            observed_updated_at: "2026-11-23T10:05:00Z",
            head_sha: "abc123",
            current_head_sha: "abc123",
            open: true,
            assigned: true,
          }),
        ]);
      });

      test("a close that lands between the repair and the report is what gets reported", async () => {
        const api = makeApi({ binding: stale });
        await handleCiEvent(
          makeCtx({ api, perRead: { 2: { state: "closed" } } }).ctx,
          "check_run",
          redCheckRun,
        );
        expect(reported(api).map((c) => c.body)).toEqual([
          expect.objectContaining({ open: false }),
        ]);
      });

      test("a conflicting head repair is recomputed and retried before the report", async () => {
        const api = makeApi({ binding: stale, stateFailures: ["conflict"] });
        await handleCiEvent(makeCtx({ api }).ctx, "check_run", redCheckRun);
        expect(api.calls.filter((c) => c.method === "POST").map((c) => c.path.split("/").pop())).toEqual([
          "state",
          "state",
          "ci-failures",
        ]);
      });

      test("a new head between the repair and the report is not reported", async () => {
        const api = makeApi({ binding: stale });
        await handleCiEvent(
          makeCtx({ api, heads: ["abc123", "abc123", "def456"] }).ctx,
          "check_run",
          redCheckRun,
        );
        expect(api.posts("/state")).toHaveLength(1);
        expect(reported(api)).toHaveLength(0);
        expect(api.sessionCalls).toEqual([]);
      });

      test("a different node id, or an unreadable PR, between the repair and the report is not reported", async () => {
        for (const perRead of [{ 2: { nodeId: "PR_other" } }, { 2: { fails: true } }]) {
          const api = makeApi({ binding: stale });
          await handleCiEvent(makeCtx({ api, perRead }).ctx, "check_run", redCheckRun);
          expect(reported(api)).toHaveLength(0);
          expect(api.sessionCalls).toEqual([]);
        }
      });

      test("without a repair the single fresh read is the last one", async () => {
        const api = makeApi({ binding: { managed: true, closed: false } });
        const { ctx } = makeCtx({ api, perRead: { 2: { fails: true } } });
        await handleCiEvent(ctx, "check_run", redCheckRun);
        expect(api.posts("/state")).toHaveLength(0);
        expect(reported(api)).toHaveLength(1);
      });
    });

    test("does not depend on the shared settled claim, which a green result may have consumed", async () => {
      const api = makeApi({ binding: { managed: true, closed: false } });
      const state = makeState();
      await state.set("centaur-githubbot:ci-settled:base/repo#7:abc123", "1");
      await handleCiEvent(makeCtx({ api, state }).ctx, "check_run", redCheckRun);
      expect(api.posts("/ci-failures")).toHaveLength(1);
    });

    test("an ineligible attempt reports nothing and consumes nothing, so the report goes out once management starts", async () => {
      const state = makeState();
      const early = makeApi({ binding: { managed: false, closed: false } });
      await handleCiEvent(makeCtx({ api: early, state }).ctx, "check_run", redCheckRun);
      expect(early.posts("/ci-failures")).toHaveLength(0);
      noNoticeClaim(state);
      expect(await state.get("centaur-githubbot:ci-settled:base/repo#7:abc123")).toBeUndefined();

      const later = makeApi({ binding: { managed: true, closed: false } });
      await handleCiEvent(makeCtx({ api: later, state }).ctx, "check_run", redCheckRun);
      expect(later.posts("/ci-failures")).toHaveLength(1);
    });

    test("posted and duplicate are done; skipped and errors are only logged, and none leaves a claim", async () => {
      const outcomes: (Response | "throw")[] = [
        Response.json({ notice: "posted" }),
        Response.json({ notice: "duplicate" }),
        Response.json({ notice: "skipped", reason: "stale_head" }),
        new Response(JSON.stringify({ error: { code: "slack_post_failed" } }), { status: 502 }),
        Response.json({ notice: "weird" }),
        "throw",
      ];
      for (const outcome of outcomes) {
        const state = makeState();
        const api = makeApi({ binding: { managed: true, closed: false } });
        const reporting = {
          ...api,
          fetch: async (url: string | URL | Request, init?: RequestInit) => {
            if (!String(url).endsWith("/ci-failures")) return api.fetch(url, init);
            api.calls.push({ body: undefined, method: "POST", path: "/ci-failures" });
            if (outcome === "throw") throw new TypeError("fetch failed");
            return outcome.clone();
          },
        };
        const { ctx } = makeCtx({ api: reporting, state });
        // Never throws, and a redelivery reports again rather than being blocked locally.
        await handleCiEvent(ctx, "check_run", redCheckRun);
        await handleCiEvent(ctx, "check_run", redCheckRun);
        expect(api.posts("/ci-failures")).toHaveLength(2);
        noNoticeClaim(state);
        expect(api.sessionCalls).toEqual([]);
      }
    });

    describe("naming the failing check", () => {
      const eventFor = (check: Record<string, unknown>) =>
        JSON.stringify({
          repository: { full_name: "base/repo" },
          check_run: {
            head_sha: "abc123",
            pull_requests: [{ number: 7 }],
            id: 99,
            app: { id: APP_ID },
            ...check,
          },
        });
      const bound = () => makeApi({ binding: { managed: true, closed: false } });
      const APP_ID = 15368;
      const stillFailing = {
        id: 90,
        app: { id: APP_ID },
        started_at: "2026-11-23T10:00:00Z",
        head_sha: "abc123",
        status: "completed",
        conclusion: "failure",
        name: "lint",
        details_url: "https://ci.example.test/lint/9",
        html_url: "https://github.com/base/repo/runs/9",
      };
      const failedEvent = { name: "lint", conclusion: "failure" };
      const body = (api: ReturnType<typeof makeApi>) =>
        api.posts("/ci-failures")[0]?.body as Record<string, unknown> | undefined;

      test("an aggregate-only evaluation uses the event's check once a fresh read confirms it", async () => {
        const api = bound();
        const { ctx, reads } = makeCtx({ api, aggregateOnly: true, github: { checkRuns: [stillFailing] } });
        await handleCiEvent(ctx, "check_run", eventFor(failedEvent));
        expect(api.posts("/ci-failures")).toHaveLength(1);
        expect(body(api)).toMatchObject({
          check_name: "lint",
          details_url: "https://ci.example.test/lint/9",
          head_sha: "abc123",
        });
        // The confirmation comes before, and the fresh PR read stays last.
        expect(reads).toEqual(["pr", "check", "pr"]);
      });

      test("takes the link from the latest run, not the event's, and falls back to html_url", async () => {
        const api = bound();
        const { ctx } = makeCtx({
          api,
          aggregateOnly: true,
          github: { checkRuns: [{ ...stillFailing, details_url: null }] },
        });
        await handleCiEvent(
          ctx,
          "check_run",
          eventFor({ ...failedEvent, details_url: "https://ci.example.test/stale-link" }),
        );
        expect(body(api)).toMatchObject({
          check_name: "lint",
          details_url: "https://github.com/base/repo/runs/9",
        });
      });

      test("omits the link when the fresh read has none", async () => {
        const api = bound();
        const { ctx } = makeCtx({
          api,
          aggregateOnly: true,
          github: { checkRuns: [{ ...stillFailing, details_url: null, html_url: null }] },
        });
        await handleCiEvent(ctx, "check_run", eventFor(failedEvent));
        expect(body(api)?.check_name).toBe("lint");
        expect("details_url" in (body(api) ?? {})).toBe(false);
      });

      test("a stale event (its check has since passed, moved or can't be read) is not used", async () => {
        for (const checkRuns of [
          [{ ...stillFailing, conclusion: "success" }],
          [{ ...stillFailing, status: "in_progress", conclusion: null }],
          [{ ...stillFailing, head_sha: "def456" }],
          [{ ...stillFailing, name: "other" }],
          [],
          "throw" as const,
        ]) {
          const api = bound();
          const state = makeState();
          const { ctx } = makeCtx({ api, state, aggregateOnly: true, github: { checkRuns } });
          await handleCiEvent(ctx, "check_run", eventFor(failedEvent));
          // No identifiable failing check: no report, and the head stays unclaimed.
          expect(api.posts("/ci-failures")).toHaveLength(0);
          expect(api.sessionCalls).toEqual([]);
          expect(state.keys()).toEqual([]);
        }
      });

      test("a late event from an old failed run is not used once a rerun of that check passed", async () => {
        const api = bound();
        const state = makeState();
        const { ctx, reads } = makeCtx({
          api,
          state,
          aggregateOnly: true,
          github: {
            checkRuns: [
              // The run the event is about, still failing when read by itself...
              stillFailing,
              // ...and its rerun, a newer run of the same check, which passed.
              {
                ...stillFailing,
                id: 91,
                started_at: "2026-11-23T10:05:00Z",
                conclusion: "success",
                details_url: "https://ci.example.test/lint/10",
              },
            ],
          },
        });
        await handleCiEvent(ctx, "check_run", eventFor(failedEvent));
        expect(reads).toContain("check");
        expect(api.posts("/ci-failures")).toHaveLength(0);
        expect(state.keys()).toEqual([]);
      });

      test("when the latest run of the check is the failing one, its link is used", async () => {
        const api = bound();
        const { ctx } = makeCtx({
          api,
          aggregateOnly: true,
          github: {
            checkRuns: [
              stillFailing,
              { ...stillFailing, id: 91, started_at: "2026-11-23T10:05:00Z", details_url: "https://ci.example.test/lint/10" },
            ],
          },
        });
        await handleCiEvent(ctx, "check_run", eventFor(failedEvent));
        expect(body(api)).toMatchObject({ details_url: "https://ci.example.test/lint/10" });
      });

      test("asks for the event's own app's runs, and uses none for an event with no app", async () => {
        const api = bound();
        const { ctx, checkQueries } = makeCtx({ api, aggregateOnly: true, github: { checkRuns: [stillFailing] } });
        await handleCiEvent(ctx, "check_run", eventFor(failedEvent));
        expect(checkQueries).toEqual([
          expect.objectContaining({ app_id: APP_ID, check_name: "lint", filter: "latest", ref: "abc123" }),
        ]);

        const noApp = bound();
        const unattributed = makeCtx({ api: noApp, aggregateOnly: true, github: { checkRuns: [stillFailing] } });
        await handleCiEvent(
          unattributed.ctx,
          "check_run",
          eventFor({ ...failedEvent, app: undefined }),
        );
        // No app id on the event: the fallback is not used, and nothing is read for it.
        expect(unattributed.checkQueries).toEqual([]);
        expect(noApp.posts("/ci-failures")).toHaveLength(0);
      });

      test("a same-name passing run from a different app does not hide the event's app's failure", async () => {
        const api = bound();
        const { ctx } = makeCtx({
          api,
          aggregateOnly: true,
          github: {
            checkRuns: [
              stillFailing,
              // Newer, same name, passing: but another app's check.
              {
                ...stillFailing,
                id: 91,
                app: { id: 99999 },
                started_at: "2026-11-23T10:05:00Z",
                conclusion: "success",
              },
            ],
          },
        });
        await handleCiEvent(ctx, "check_run", eventFor(failedEvent));
        expect(body(api)).toMatchObject({
          check_name: "lint",
          details_url: "https://ci.example.test/lint/9",
        });
      });

      test("a same-name failing run from a different app is not the event's check either", async () => {
        const api = bound();
        const { ctx } = makeCtx({
          api,
          aggregateOnly: true,
          github: {
            checkRuns: [{ ...stillFailing, app: { id: 99999 } }],
          },
        });
        await handleCiEvent(ctx, "check_run", eventFor(failedEvent));
        expect(api.posts("/ci-failures")).toHaveLength(0);
      });

      test("the confirmed check's http:// details_url yields to its HTTPS html_url, and to none", async () => {
        const run = (extra: Record<string, unknown>) => ({ ...stillFailing, ...extra });
        const urlFor = async (checkRun: Record<string, unknown>) => {
          const api = bound();
          await handleCiEvent(
            makeCtx({ api, aggregateOnly: true, github: { checkRuns: [checkRun] } }).ctx,
            "check_run",
            eventFor(failedEvent),
          );
          return body(api) as Record<string, unknown>;
        };
        expect(
          (await urlFor(run({ details_url: "http://ci.example.test/lint/9" }))).details_url,
        ).toBe("https://github.com/base/repo/runs/9");
        expect(
          (
            await urlFor(
              run({ details_url: `https://ci.example.test/${"a".repeat(2_048)}` }),
            )
          ).details_url,
        ).toBe("https://github.com/base/repo/runs/9");
        // Slack would drop a `|` in the details URL: the html_url is the link.
        expect(
          (await urlFor(run({ details_url: "https://ci.example.test/a|b" }))).details_url,
        ).toBe("https://github.com/base/repo/runs/9");
        // Nothing usable: the check is still named, with no link.
        const none = await urlFor(
          run({ details_url: "http://ci.example.test/lint/9", html_url: "http://github.com/x" }),
        );
        expect(none.check_name).toBe("lint");
        expect("details_url" in none).toBe(false);
      });

      test("no identifiable check means no report, with no generic notice", async () => {
        for (const check of [
          { name: "build", conclusion: "success", details_url: "https://ci.example.test/ok" },
          { conclusion: "failure", details_url: "https://ci.example.test/unnamed" },
        ]) {
          const api = bound();
          const { ctx, reads } = makeCtx({ api, aggregateOnly: true, github: { checkRuns: [stillFailing] } });
          await handleCiEvent(ctx, "check_run", eventFor(check));
          expect(api.posts("/ci-failures")).toHaveLength(0);
          // A passing or unnamed event is never even looked up.
          expect(reads).not.toContain("check");
        }
        // And an aggregate-only evaluation with no usable event at all.
        const api = bound();
        const { ctx } = makeCtx({ api, aggregateOnly: true });
        await handleCiEvent(ctx, "check_suite", JSON.stringify({
          repository: { full_name: "base/repo" },
          check_suite: { head_sha: "abc123", pull_requests: [{ number: 7 }] },
        }));
        expect(api.posts("/ci-failures")).toHaveLength(0);
      });

      const workflowEvent = JSON.stringify({
        repository: { full_name: "base/repo" },
        workflow_run: {
          id: 5,
          workflow_id: 17,
          name: "ci",
          conclusion: "failure",
          head_sha: "abc123",
          pull_requests: [{ number: 7 }],
        },
      });
      const workflowRun = {
        id: 5,
        run_number: 3,
        run_attempt: 1,
        head_sha: "abc123",
        status: "completed",
        conclusion: "failure",
        name: "ci",
        html_url: "https://github.com/base/repo/actions/runs/5",
      };

      test("a workflow run is confirmed by the latest run of that workflow on the head", async () => {
        const failing = bound();
        await handleCiEvent(
          makeCtx({ api: failing, aggregateOnly: true, github: { workflowRuns: [workflowRun] } }).ctx,
          "workflow_run",
          workflowEvent,
        );
        expect(body(failing)).toMatchObject({
          check_name: "ci",
          details_url: "https://github.com/base/repo/actions/runs/5",
        });

        // A later run of the same workflow on that head passed: the old failure is stale.
        for (const workflowRuns of [
          [workflowRun, { ...workflowRun, id: 6, run_number: 4, conclusion: "success" }],
          [workflowRun, { ...workflowRun, run_attempt: 2, conclusion: "success" }],
          [{ ...workflowRun, status: "in_progress", conclusion: null }],
          [],
          "throw" as const,
        ]) {
          const stale = bound();
          await handleCiEvent(
            makeCtx({ api: stale, aggregateOnly: true, github: { workflowRuns } }).ctx,
            "workflow_run",
            workflowEvent,
          );
          expect(stale.posts("/ci-failures")).toHaveLength(0);
        }
      });

      test("a status is confirmed by its latest state for the sha", async () => {
        const statusApi = bound();
        const statusCtx = makeCtx({
          api: statusApi,
          aggregateOnly: true,
          github: {
            combinedStatus: {
              sha: "abc123",
              statuses: [
                { context: "deploy", state: "failure", target_url: "https://ci.example.test/deploy/3" },
              ],
            },
          },
        }).ctx;
        (statusCtx.octokit.rest.repos as unknown as Record<string, unknown>).listPullRequestsAssociatedWithCommit =
          async () => ({ data: [{ number: 7 }] });
        const statusEvent = JSON.stringify({
          repository: { full_name: "base/repo" },
          sha: "abc123",
          state: "failure",
          context: "deploy",
          target_url: "https://ci.example.test/old",
        });
        await handleCiEvent(statusCtx, "status", statusEvent);
        expect(body(statusApi)).toMatchObject({
          check_name: "deploy",
          details_url: "https://ci.example.test/deploy/3",
        });

        // The same status has since passed.
        const passedApi = bound();
        const passedCtx = makeCtx({
          api: passedApi,
          aggregateOnly: true,
          github: {
            combinedStatus: {
              sha: "abc123",
              statuses: [{ context: "deploy", state: "success", target_url: "u" }],
            },
          },
        }).ctx;
        (passedCtx.octokit.rest.repos as unknown as Record<string, unknown>).listPullRequestsAssociatedWithCommit =
          async () => ({ data: [{ number: 7 }] });
        await handleCiEvent(passedCtx, "status", statusEvent);
        expect(passedApi.posts("/ci-failures")).toHaveLength(0);
      });

      test("finds a failing status context beyond the first page of 100", async () => {
        const statuses = Array.from({ length: 230 }, (_, index) => ({
          context: `ctx-${index}`,
          state: index === 215 ? "failure" : "success",
          target_url: `https://ci.example.test/ctx/${index}`,
        }));
        const api = bound();
        const ctx = makeCtx({
          api,
          aggregateOnly: true,
          github: { combinedStatus: { sha: "abc123", statuses } },
        }).ctx;
        (ctx.octokit.rest.repos as unknown as Record<string, unknown>).listPullRequestsAssociatedWithCommit =
          async () => ({ data: [{ number: 7 }] });
        await handleCiEvent(
          ctx,
          "status",
          JSON.stringify({
            repository: { full_name: "base/repo" },
            sha: "abc123",
            state: "failure",
            context: "ctx-215",
          }),
        );
        expect(body(api)).toMatchObject({
          check_name: "ctx-215",
          details_url: "https://ci.example.test/ctx/215",
        });
      });

      test("a centaur-skip check is never named, even when confirmed failing", async () => {
        const skipRun = { ...stillFailing, name: "agent-pr-rules-centaur-skip" };
        const api = bound();
        await handleCiEvent(
          makeCtx({ api, aggregateOnly: true, github: { checkRuns: [skipRun] } }).ctx,
          "check_run",
          eventFor({ name: skipRun.name, conclusion: "failure" }),
        );
        // With no other eligible named failure, nothing is reported.
        expect(api.posts("/ci-failures")).toHaveLength(0);

        // The same for a workflow, whatever the event claims.
        const workflowApi = bound();
        await handleCiEvent(
          makeCtx({
            api: workflowApi,
            aggregateOnly: true,
            github: {
              workflowRuns: [
                {
                  id: 5,
                  run_number: 1,
                  head_sha: "abc123",
                  status: "completed",
                  conclusion: "failure",
                  name: "Centaur-Skip gate",
                  html_url: "https://github.com/base/repo/actions/runs/5",
                },
              ],
            },
          }).ctx,
          "workflow_run",
          JSON.stringify({
            repository: { full_name: "base/repo" },
            workflow_run: {
              id: 5,
              workflow_id: 17,
              name: "ci",
              conclusion: "failure",
              head_sha: "abc123",
              pull_requests: [{ number: 7 }],
            },
          }),
        );
        expect(workflowApi.posts("/ci-failures")).toHaveLength(0);
      });

      test("a skipped check does not hide a real failure the evaluation did read", async () => {
        const api = bound();
        const { ctx } = makeCtx({
          api,
          github: { checkRuns: [{ ...stillFailing, name: "x-centaur-skip" }] },
        });
        await handleCiEvent(ctx, "check_run", eventFor({ name: "x-centaur-skip", conclusion: "failure" }));
        expect(body(api)).toMatchObject({ check_name: "build" });
      });

      const buildRun = {
        ...stillFailing,
        id: 70,
        name: "build",
        details_url: FAILURE_URL,
      };
      const lintNode = (extra: Record<string, unknown> = {}) => ({
        __typename: "CheckRun",
        name: "lint",
        status: "COMPLETED",
        conclusion: "FAILURE",
        detailsUrl: "https://ci.example.test/lint/old",
        checkSuite: { app: { slug: "ci", databaseId: APP_ID } },
        ...extra,
      });

      test("a check read from an incomplete rollup is named only once its latest run is confirmed failing", async () => {
        const api = bound();
        const { ctx, reads, checkQueries } = makeCtx({
          api,
          partial: true,
          // A readable event that would name something else must not win.
          github: { checkRuns: [buildRun] },
        });
        await handleCiEvent(ctx, "check_run", eventFor({ name: "other", conclusion: "failure" }));
        expect(body(api)).toMatchObject({ check_name: "build", details_url: FAILURE_URL });
        // Confirmed by name and app, before the final PR read.
        expect(checkQueries).toEqual([
          expect.objectContaining({ check_name: "build", app_id: APP_ID, filter: "latest" }),
        ]);
        expect(reads).toEqual(["pr", "check", "pr"]);
      });

      test("an old failed run whose rerun passed (unreadably) is not named; the confirmed event's check is", async () => {
        const api = bound();
        const { ctx, checkQueries } = makeCtx({
          api,
          partial: true,
          checkNode: lintNode(),
          github: {
            checkRuns: [
              // lint: the visible failure was superseded by a newer passing run.
              { ...stillFailing, id: 60, name: "lint", conclusion: "failure" },
              { ...stillFailing, id: 61, name: "lint", started_at: "2026-11-23T10:05:00Z", conclusion: "success" },
              // build: the real, confirmed failure.
              buildRun,
            ],
          },
        });
        await handleCiEvent(ctx, "check_run", eventFor({ name: "build", conclusion: "failure" }));
        expect(body(api)).toMatchObject({ check_name: "build", details_url: FAILURE_URL });
        expect(checkQueries.map((q) => q.check_name)).toEqual(["lint", "build"]);
      });

      test("an unconfirmed provisional check with no event failure reports nothing and claims nothing", async () => {
        const api = bound();
        const state = makeState();
        const { ctx } = makeCtx({
          api,
          state,
          partial: true,
          checkNode: lintNode(),
          github: { checkRuns: [{ ...stillFailing, name: "lint", conclusion: "success" }] },
        });
        await handleCiEvent(
          ctx,
          "check_suite",
          JSON.stringify({
            repository: { full_name: "base/repo" },
            check_suite: { head_sha: "abc123", pull_requests: [{ number: 7 }] },
          }),
        );
        expect(api.posts("/ci-failures")).toHaveLength(0);
        expect(state.keys()).toEqual([]);
        expect(api.sessionCalls).toEqual([]);
      });

      test("a provisional check with no app id can't be confirmed, so the event's check is used", async () => {
        const api = bound();
        const { ctx, checkQueries } = makeCtx({
          api,
          partial: true,
          checkNode: lintNode({ checkSuite: undefined }),
          github: { checkRuns: [{ ...stillFailing, name: "lint" }, buildRun] },
        });
        await handleCiEvent(ctx, "check_run", eventFor({ name: "build", conclusion: "failure" }));
        // Only the event's check was looked up; lint was never confirmable.
        expect(checkQueries.map((q) => q.check_name)).toEqual(["build"]);
        expect(body(api)).toMatchObject({ check_name: "build" });
      });

      test("a provisional failing status is confirmed by its latest state for the sha", async () => {
        const statusNode = {
          __typename: "StatusContext",
          context: "deploy",
          state: "FAILURE",
          targetUrl: "https://ci.example.test/deploy/old",
        };
        const confirm = (state: string) => ({
          sha: "abc123",
          statuses: [{ context: "deploy", state, target_url: "https://ci.example.test/deploy/new" }],
        });
        const failing = bound();
        await handleCiEvent(
          makeCtx({
            api: failing,
            partial: true,
            checkNode: statusNode,
            github: { combinedStatus: confirm("failure") },
          }).ctx,
          "check_run",
          eventFor({ name: "x", conclusion: "success" }),
        );
        expect(body(failing)).toMatchObject({
          check_name: "deploy",
          details_url: "https://ci.example.test/deploy/new",
        });
        const passed = bound();
        await handleCiEvent(
          makeCtx({
            api: passed,
            partial: true,
            checkNode: statusNode,
            github: { combinedStatus: confirm("success") },
          }).ctx,
          "check_run",
          eventFor({ name: "x", conclusion: "success" }),
        );
        expect(passed.posts("/ci-failures")).toHaveLength(0);
      });

      test("never overrides a failing check the evaluation did read", async () => {
        const api = bound();
        const { ctx, reads } = makeCtx({ api, github: { checkRuns: [stillFailing] } });
        await handleCiEvent(ctx, "check_run", eventFor(failedEvent));
        expect(body(api)).toMatchObject({ check_name: "build", details_url: FAILURE_URL });
        expect(reads).not.toContain("check");
      });

      test("truncates a very long check name to api-rs's 200 characters", async () => {
        const api = bound();
        const long = "x".repeat(150) + "é".repeat(100);
        const { ctx } = makeCtx({
          api,
          checkNode: { __typename: "CheckRun", name: long, status: "COMPLETED", conclusion: "FAILURE" },
        });
        await handleCiEvent(ctx, "check_run", eventFor({}));
        const name = body(api)?.check_name as string;
        expect(Array.from(name)).toHaveLength(200);
        expect(name).toBe(long.slice(0, 200));
        // A name at the limit is left alone.
        const exact = bound();
        await handleCiEvent(
          makeCtx({
            api: exact,
            checkNode: { __typename: "CheckRun", name: "y".repeat(200), status: "COMPLETED", conclusion: "FAILURE" },
          }).ctx,
          "check_run",
          eventFor({}),
        );
        expect(body(exact)?.check_name).toBe("y".repeat(200));
      });
    });

    test("never escalates a bound PR, even at the attempt cap", async () => {
      const api = makeApi({ binding: { managed: true, closed: false } });
      const state = makeState();
      await state.set(PR_KEY, { consecutiveCiFixes: 3 });
      const { ctx, comments } = makeCtx({ api, state });
      await handleCiEvent(ctx, "check_run", redCheckRun);
      expect(comments).toEqual([]);
      expect(api.posts("/ci-failures")).toHaveLength(1);
      expect(await state.get(PR_KEY)).toEqual({ consecutiveCiFixes: 3 });
    });

    test("anything but a 404 from the binding lookup does nothing: no turn, no escalation, no counter change", async () => {
      for (const lookup of [
        { lookupStatus: 403 },
        { lookupStatus: 503 },
        { lookupStatus: 400 },
        { lookupBody: {} },
        { lookupBody: { thread_key: "slack:T0:C0:1.2" } },
      ]) {
        const api = makeApi({ binding: null, ...lookup });
        const state = makeState();
        // At the cap, so a fall-through would escalate with a comment.
        await state.set(PR_KEY, { consecutiveCiFixes: 3 });
        const { ctx, comments } = makeCtx({ api, state });
        await handleCiEvent(ctx, "check_run", redCheckRun);
        await drainBackgroundWork(2000);
        expect(api.sessionCalls).toEqual([]);
        expect(comments).toEqual([]);
        expect(api.posts("/ci-failures")).toHaveLength(0);
        expect(await state.get(PR_KEY)).toEqual({ consecutiveCiFixes: 3 });
        // No claim is left behind to suppress a later delivery.
        expect(await state.get("centaur-githubbot:ci-settled:base/repo#7:abc123")).toBeUndefined();
        noNoticeClaim(state);
      }
    });

    test("a generic 404 starts no fix turn, no escalation and no counter change", async () => {
      for (const lookupResponse of [
        () => new Response("not found", { status: 404 }),
        () => new Response("", { status: 404 }),
        () => Response.json({ error: "not found" }, { status: 404 }),
        () => Response.json({ code: "route_not_found" }, { status: 404 }),
      ]) {
        const api = makeApi({ binding: null, lookupResponse });
        const state = makeState();
        // At the cap, so a fall-through would escalate with a comment.
        await state.set(PR_KEY, { consecutiveCiFixes: 3 });
        const { ctx, comments } = makeCtx({ api, state });
        await handleCiEvent(ctx, "check_run", redCheckRun);
        await drainBackgroundWork(2000);
        expect(api.sessionCalls).toEqual([]);
        expect(comments).toEqual([]);
        expect(api.posts("/ci-failures")).toHaveLength(0);
        expect(await state.get(PR_KEY)).toEqual({ consecutiveCiFixes: 3 });
        expect(await state.get("centaur-githubbot:ci-settled:base/repo#7:abc123")).toBeUndefined();
      }
    });

    describe("an answer for another PR's binding", () => {
      // Not a miss: only a coded miss means unbound. api-rs answers a node-id
      // lookup with that PR's binding or a miss, so anything else is anomalous.
      const otherPr = () =>
        Response.json({
          repository: "base/repo",
          number: 7,
          thread_key: "slack:T0:C0:1700000000.000100",
          pr_node_id: "PR_other",
          revision: 1,
          head_sha: "abc123",
          managed: true,
          closed: false,
        });

      test("a 200 for a different node starts no fix turn, no escalation, no counter change and no claim", async () => {
        const api = makeApi({ binding: null, lookupResponse: otherPr });
        const state = makeState();
        // At the cap, so a fall-through would escalate with a comment.
        await state.set(PR_KEY, { consecutiveCiFixes: 3 });
        const { ctx, comments } = makeCtx({ api, state });
        await handleCiEvent(ctx, "check_run", redCheckRun);
        await drainBackgroundWork(2000);
        expect(api.sessionCalls).toEqual([]);
        expect(comments).toEqual([]);
        expect(api.posts("/ci-failures")).toHaveLength(0);
        expect(api.posts("/state")).toHaveLength(0);
        expect(await state.get(PR_KEY)).toEqual({ consecutiveCiFixes: 3 });
        expect(await state.get("centaur-githubbot:ci-settled:base/repo#7:abc123")).toBeUndefined();
        expect(state.keys()).toEqual([PR_KEY]);
      });

      test("lifecycle and assignment events write nothing and take no turn for it", async () => {
        const lifecycle = (action: string, assignee?: string) =>
          JSON.stringify({
            action,
            repository: { full_name: "base/repo" },
            pull_request: { number: 7, node_id: NODE_ID, head: { sha: "abc123" } },
            ...(assignee ? { assignee: { login: assignee } } : {}),
          });
        for (const [action, assignee] of [
          ["closed", undefined],
          ["reopened", undefined],
          ["synchronize", undefined],
          ["unassigned", "centaur-bot"],
          ["assigned", "centaur-bot"],
        ] as const) {
          const api = makeApi({ binding: null, lookupResponse: otherPr });
          const { ctx } = makeCtx({ api });
          await handlePullRequestEvent(ctx, lifecycle(action, assignee));
          await drainBackgroundWork(2000);
          expect(api.posts("/state")).toHaveLength(0);
          expect(api.posts("/ci-failures")).toHaveLength(0);
          // Not even an assignment's forced takeover runs a turn on it.
          expect(api.sessionCalls).toEqual([]);
        }
      });
    });

    describe("a partially readable rollup", () => {
      // One readable failing check, one unreadable: the verdict is the aggregate's.
      test("leaves the unbound fix prompt and escalation exactly as before, with no failing names", async () => {
        const api = makeApi({ binding: null });
        const state = makeState();
        const { ctx } = makeCtx({ api, state, partial: true });
        await handleCiEvent(ctx, "check_run", redCheckRun);
        await drainBackgroundWork(2000);
        // The fix turn fires as in base. Its prompt is built from `failingNames`
        // alone (`failingNames.join(", ") || "unknown"`), which the evaluation
        // test below pins to [].
        expect(api.sessionCalls.length).toBeGreaterThan(0);
        expect(await state.get(PR_KEY)).toMatchObject({ consecutiveCiFixes: 1 });
        expect(api.sessionBodies.join("\n")).not.toContain("build");
        expect(api.posts("/ci-failures")).toHaveLength(0);

        const atCap = makeState();
        await atCap.set(PR_KEY, { consecutiveCiFixes: 3 });
        const escalation = makeCtx({ api: makeApi({ binding: null }), state: atCap, partial: true });
        await handleCiEvent(escalation.ctx, "check_run", redCheckRun);
        expect(escalation.comments).toHaveLength(1);
        expect(escalation.comments[0]).toContain("Still failing: the CI checks.");
        expect(escalation.comments[0]).not.toContain("build");
      });

      test("still lets the bound notice name the readable check and its link", async () => {
        const api = makeApi({ binding: { managed: true, closed: false } });
        const { ctx } = makeCtx({
          api,
          partial: true,
          // The provisional check is confirmed still failing before it is named.
          github: {
            checkRuns: [
              {
                id: 1,
                started_at: "2026-11-23T10:00:00Z",
                head_sha: "abc123",
                app: { id: CHECK_APP_ID },
                status: "completed",
                conclusion: "failure",
                name: "build",
                details_url: FAILURE_URL,
                html_url: "https://github.com/base/repo/runs/1",
              },
            ],
          },
        });
        await handleCiEvent(ctx, "check_run", redCheckRun);
        expect(api.sessionCalls).toEqual([]);
        expect(api.posts("/ci-failures")[0]?.body).toMatchObject({
          check_name: "build",
          details_url: FAILURE_URL,
        });
      });

      test("the evaluation's failing names are empty while the notice failure is kept", async () => {
        const graphql = async () => ({
          repository: {
            object: {
              statusCheckRollup: {
                state: "FAILURE",
                contexts: {
                  nodes: [
                    {
                      __typename: "CheckRun",
                      name: "build",
                      status: "COMPLETED",
                      conclusion: "FAILURE",
                      detailsUrl: FAILURE_URL,
                      checkSuite: { app: { slug: "ci", databaseId: CHECK_APP_ID } },
                    },
                    null,
                  ],
                  pageInfo: { hasNextPage: false, endCursor: null },
                  checkRunCountsByState: [{ count: 2, state: "FAILURE" }],
                  statusContextCountsByState: [],
                },
              },
            },
          },
        });
        const evaluation = await fetchCiEvaluation(
          { octokit: { graphql, rest: {} }, options: { apiUrl: "http://localhost", logger: quietLogger } } as never,
          "base",
          "repo",
          "abc123",
        );
        expect(evaluation).toEqual({
          settled: true,
          failed: true,
          failingNames: [],
          // Provisional: the unreadable half may hold a newer run of it.
          noticeFailure: {
            name: "build",
            kind: "check_run",
            url: FAILURE_URL,
            appId: CHECK_APP_ID,
            provisional: true,
          },
        });
      });
    });

    test("a later delivery after a failed lookup reports normally", async () => {
      const state = makeState();
      await handleCiEvent(
        makeCtx({ api: makeApi({ binding: null, lookupStatus: 503 }), state }).ctx,
        "check_run",
        redCheckRun,
      );
      const recovered = makeApi({ binding: { managed: true, closed: false } });
      await handleCiEvent(makeCtx({ api: recovered, state }).ctx, "check_run", redCheckRun);
      expect(recovered.posts("/ci-failures")).toHaveLength(1);
    });

    test("a failed report does not block a later delivery", async () => {
      const api = makeApi({ binding: { managed: true, closed: false } });
      const state = makeState();
      const realFetch = api.fetch;
      let failReports = true;
      const flaky = {
        ...api,
        fetch: async (url: string | URL | Request, init?: RequestInit) =>
          failReports && String(url).endsWith("/ci-failures")
            ? new Response("no", { status: 503 })
            : realFetch(url, init),
      };
      await handleCiEvent(makeCtx({ api: flaky, state }).ctx, "check_run", redCheckRun);
      failReports = false;
      await handleCiEvent(makeCtx({ api: flaky, state }).ctx, "check_run", redCheckRun);
      expect(api.posts("/ci-failures")).toHaveLength(1);
    });
  });

  describe("lifecycle state", () => {
    function prEvent(
      action: string,
      extra: Record<string, unknown> = {},
      head = "old000",
      nodeId = NODE_ID,
    ) {
      return JSON.stringify({
        action,
        repository: { full_name: "base/repo" },
        pull_request: { number: 7, node_id: nodeId, head: { sha: head } },
        ...extra,
      });
    }

    type RunOpts = {
      assignees?: { login: string }[];
      heads?: string[];
      lookupFailures?: number[];
      lookupStatus?: number;
      nodeId?: string;
      perRead?: Record<number, { fails?: boolean; nodeId?: string; state?: string; updatedAt?: string }>;
      prFetchFails?: boolean;
      prState?: string;
      stateFailures?: (number | "network" | "conflict")[];
      updatedAt?: string | null;
    };

    async function exec(binding: Binding | null, body: string, opts: RunOpts = {}) {
      const api = makeApi({
        binding,
        lookupFailures: opts.lookupFailures,
        lookupStatus: opts.lookupStatus,
        stateFailures: opts.stateFailures,
      });
      const { ctx, sleeps } = makeCtx({ api, settled: false, ...opts });
      await handlePullRequestEvent(ctx, body);
      await drainBackgroundWork(2000);
      return {
        gets: api.calls.filter((c) => c.method === "GET").length,
        sleeps,
        updates: api.posts("/api/pr-bindings/base/repo/7/state").map((c) => c.body),
      };
    }

    async function run(binding: Binding | null, body: string, opts: RunOpts = {}) {
      return (await exec(binding, body, opts)).updates;
    }

    // Every write names the revision of the binding read before the PR fetch.
    const seen = { observed_updated_at: UPDATED_AT, expected_revision: 1 };

    const bound = { managed: false, closed: false };

    test("explicitly assigning the bot sets managed, at the PR's current head", async () => {
      const updates = await run(
        bound,
        prEvent("assigned", { assignee: { login: "Centaur-Bot" } }),
        { heads: ["fresh11"] },
      );
      expect(updates).toEqual([
        { pr_node_id: NODE_ID, ...seen, managed: true, head_sha: "fresh11" },
      ]);
    });

    test("assigning someone else does not, even with the bot already assigned", async () => {
      const updates = await run(
        bound,
        prEvent("assigned", { assignee: { login: "a-human" } }),
        { assignees: [{ login: "centaur-bot" }, { login: "a-human" }] },
      );
      expect(updates).toEqual([]);
    });

    test("a late assigned after the bot was unassigned does not enable management", async () => {
      expect(
        await run(bound, prEvent("assigned", { assignee: { login: "centaur-bot" } }), {
          assignees: [{ login: "a-human" }],
        }),
      ).toEqual([]);
      // If the fresh read fails, management is not enabled either.
      expect(
        await run(bound, prEvent("assigned", { assignee: { login: "centaur-bot" } }), {
          prFetchFails: true,
        }),
      ).toEqual([]);
    });

    test("unassigning the bot clears managed; unassigning someone else does not", async () => {
      const managed = { managed: true, closed: false };
      expect(
        await run(managed, prEvent("unassigned", { assignee: { login: "centaur-bot" } }), {
          assignees: [],
        }),
      ).toEqual([{ pr_node_id: NODE_ID, ...seen, managed: false }]);
      expect(
        await run(managed, prEvent("unassigned", { assignee: { login: "a-human" } })),
      ).toEqual([]);
    });

    test("a late unassigned while the bot is assigned again does not disable management", async () => {
      expect(
        await run(
          { managed: true, closed: false },
          prEvent("unassigned", { assignee: { login: "centaur-bot" } }),
          { assignees: [{ login: "centaur-bot" }] },
        ),
      ).toEqual([]);
    });

    test("no write is derived without a fresh read, or without its updated_at", async () => {
      const unassign = prEvent("unassigned", { assignee: { login: "centaur-bot" } });
      const managed = { managed: true, closed: false };
      for (const opts of [{ prFetchFails: true }, { updatedAt: null }]) {
        expect(await run(managed, unassign, opts)).toEqual([]);
        expect(await run(managed, prEvent("closed"), opts)).toEqual([]);
        expect(await run(managed, prEvent("synchronize"), opts)).toEqual([]);
      }
    });

    test("closed and reopened are derived from the fresh PR, not from the event", async () => {
      expect(await run(bound, prEvent("closed"), { prState: "closed" })).toEqual([
        { pr_node_id: NODE_ID, ...seen, closed: true },
      ]);
      expect(await run(bound, prEvent("reopened"), { prState: "open" })).toEqual([
        { pr_node_id: NODE_ID, ...seen, closed: false },
      ]);
    });

    test("a late reopened after a close converges to the fresh (closed) state", async () => {
      expect(await run(bound, prEvent("reopened"), { prState: "closed" })).toEqual([
        { pr_node_id: NODE_ID, ...seen, closed: true },
      ]);
    });

    test("a late closed after a reopen converges to the fresh (open) state", async () => {
      expect(await run(bound, prEvent("closed"), { prState: "open" })).toEqual([
        { pr_node_id: NODE_ID, ...seen, closed: false },
      ]);
    });

    test("a merged PR counts as closed", async () => {
      const api = makeApi({ binding: bound });
      const { ctx } = makeCtx({ api, settled: false });
      const pulls = ctx.octokit.rest.pulls as unknown as { get: () => Promise<{ data: object }> };
      const get = pulls.get;
      pulls.get = async () => ({ data: { ...(await get()).data, merged: true } });
      await handlePullRequestEvent(ctx, prEvent("closed"));
      expect(api.posts("/state").map((c) => c.body)).toEqual([
        { pr_node_id: NODE_ID, ...seen, closed: true },
      ]);
    });

    test("an unapplied (superseded) write is not an error", async () => {
      const api = makeApi({ binding: bound });
      const stale = {
        ...api,
        fetch: async (url: string | URL | Request, init?: RequestInit) =>
          String(url).endsWith("/state")
            ? Response.json({ ...bindingJson(bound), applied: false })
            : api.fetch(url, init),
      };
      const { ctx, sleeps } = makeCtx({ api: stale, settled: false, prState: "closed" });
      await handlePullRequestEvent(ctx, prEvent("closed"));
      expect(sleeps).toEqual([]);
    });

    describe("retrying invalidating writes", () => {
      const managed = { managed: true, closed: false };
      const invalidations: [string, string, RunOpts][] = [
        ["closed", prEvent("closed"), { prState: "closed" }],
        ["reopened", prEvent("reopened"), { prState: "open" }],
        [
          "unassigned",
          prEvent("unassigned", { assignee: { login: "centaur-bot" } }),
          { assignees: [] },
        ],
      ];

      test("retries retryable and network failures with bounded backoff, then succeeds", async () => {
        for (const [, body, opts] of invalidations) {
          const result = await exec(managed, body, { ...opts, stateFailures: [503, "network"] });
          expect(result.updates).toHaveLength(3);
          expect(result.sleeps).toEqual([10_000, 20_000]);
        }
      });

      test("gives up after three attempts", async () => {
        for (const [, body, opts] of invalidations) {
          const result = await exec(managed, body, {
            ...opts,
            stateFailures: [503, 503, 503, 503],
          });
          expect(result.updates).toHaveLength(3);
          expect(result.sleeps).toEqual([10_000, 20_000]);
        }
      });

      test("does not retry a refusal", async () => {
        const result = await exec(managed, prEvent("closed"), {
          prState: "closed",
          stateFailures: [409],
        });
        expect(result.updates).toHaveLength(1);
        expect(result.sleeps).toEqual([]);
      });

      test("other writes are not retried", async () => {
        const result = await exec(bound, prEvent("synchronize"), { stateFailures: [503] });
        expect(result.updates).toHaveLength(1);
        expect(result.sleeps).toEqual([]);
      });

      test("a transient 503 on the only assigned event still ends with managed set", async () => {
        const assigned = prEvent("assigned", { assignee: { login: "centaur-bot" } });
        // The preliminary binding GET fails once, then the write fails twice.
        const result = await exec(bound, assigned, {
          lookupFailures: [503],
          stateFailures: [503, "network"],
        });
        expect(result.updates).toHaveLength(3);
        expect(result.updates.at(-1)).toMatchObject({ managed: true, pr_node_id: NODE_ID });
        expect(result.sleeps).toEqual([10_000, 10_000, 20_000]);
      });

      test("the assigned write is retried only for retryable errors", async () => {
        const assigned = prEvent("assigned", { assignee: { login: "centaur-bot" } });
        const refused = await exec(bound, assigned, { stateFailures: [409] });
        expect(refused.updates).toHaveLength(1);
        expect(refused.sleeps).toEqual([]);
        const gaveUp = await exec(bound, assigned, { stateFailures: [503, 503, 503] });
        expect(gaveUp.updates).toHaveLength(3);
        expect(gaveUp.sleeps).toEqual([10_000, 20_000]);
      });

      test("the binding lookup is retried within the bounded budget, and only when retryable", async () => {
        const assigned = prEvent("assigned", { assignee: { login: "centaur-bot" } });
        const never = await exec(bound, assigned, { lookupStatus: 503 });
        expect(never.updates).toEqual([]);
        expect(never.sleeps).toEqual([10_000, 20_000]);
        const forbidden = await exec(bound, assigned, { lookupStatus: 403 });
        expect(forbidden.updates).toEqual([]);
        expect(forbidden.sleeps).toEqual([]);
      });

      test("a binding_state_conflict is recomputed from a fresh read and converges to it", async () => {
        // A reopened event reads the PR as closed and writes closed:true, but
        // api-rs holds the contradicting value at the same timestamp. By the
        // re-read the PR is open: the retry writes that.
        const result = await exec(managed, prEvent("reopened"), {
          prState: "closed",
          perRead: { 1: { state: "open" } },
          stateFailures: ["conflict"],
        });
        expect(result.updates).toEqual([
          { pr_node_id: NODE_ID, ...seen, closed: true },
          { pr_node_id: NODE_ID, ...seen, closed: false },
        ]);
        expect(result.sleeps).toEqual([10_000]);
      });

      test("a close stored first, then a reopen GitHub reports at the same updated_at, converges to open", async () => {
        const api = makeApi({ binding: managed });
        const { ctx: closing } = makeCtx({ api, settled: false, prState: "closed" });
        const { ctx: reopening } = makeCtx({ api, settled: false, prState: "open" });
        await handlePullRequestEvent(closing, prEvent("closed"));
        expect(api.current()).toMatchObject({ closed: true, revision: 2 });
        // Same updated_at, contradicting value, but nothing intervened: the
        // write names the revision it read, so it applies.
        await handlePullRequestEvent(reopening, prEvent("reopened"));
        expect(api.posts("/state").map((c) => c.body)).toEqual([
          { pr_node_id: NODE_ID, observed_updated_at: UPDATED_AT, expected_revision: 1, closed: true },
          { pr_node_id: NODE_ID, observed_updated_at: UPDATED_AT, expected_revision: 2, closed: false },
        ]);
        expect(api.current()).toMatchObject({ closed: false, revision: 3 });
      });

      test("when another write intervenes, both the binding and the PR are read again before the retry", async () => {
        const api = makeApi({ binding: managed, interveneBeforeState: 1 });
        const { ctx, sleeps } = makeCtx({ api, settled: false, prState: "closed" });
        await handlePullRequestEvent(ctx, prEvent("closed"));
        // Revision 1 was stale on arrival; the retry carries the re-read one.
        expect(api.posts("/state").map((c) => (c.body as { expected_revision: number }).expected_revision)).toEqual([1, 2]);
        expect(sleeps).toEqual([10_000]);
        expect(api.calls.filter((c) => c.method === "GET")).toHaveLength(2);
        expect(api.current()).toMatchObject({ closed: true });
      });

      test("a conflicting head repair re-reads the binding too", async () => {
        const api = makeApi({
          binding: { managed: true, closed: false, head_sha: "old111" },
          interveneBeforeState: 1,
        });
        await handleCiEvent(makeCtx({ api }).ctx, "check_run", redCheckRun);
        expect(
          api.posts("/state").map((c) => (c.body as { expected_revision: number }).expected_revision),
        ).toEqual([1, 2]);
        expect(api.posts("/ci-failures")).toHaveLength(1);
      });

      test("a conflict is retried for any write, within the same bounded budget", async () => {
        const sync = await exec(bound, prEvent("synchronize"), {
          heads: ["head-1", "head-2"],
          stateFailures: ["conflict"],
        });
        expect(sync.updates.map((u) => (u as { head_sha: string }).head_sha)).toEqual([
          "head-1",
          "head-2",
        ]);
        const exhausted = await exec(managed, prEvent("closed"), {
          prState: "closed",
          stateFailures: ["conflict", "conflict", "conflict", "conflict"],
        });
        expect(exhausted.updates).toHaveLength(3);
        expect(exhausted.sleeps).toEqual([10_000, 20_000]);
      });

      test("a conflict whose re-read is unreadable, or no longer calls for a write, stops there", async () => {
        const unreadable = await exec(managed, prEvent("closed"), {
          prState: "closed",
          perRead: { 1: { fails: true } },
          stateFailures: ["conflict"],
        });
        expect(unreadable.updates).toHaveLength(1);
        const moot = await exec(managed, prEvent("closed"), {
          prState: "closed",
          perRead: { 1: { nodeId: "PR_other" } },
          stateFailures: ["conflict"],
        });
        expect(moot.updates).toHaveLength(1);
      });

      test("still writes on the payload's node id when the binding lookup fails", async () => {
        for (const [, body, opts] of invalidations) {
          const result = await exec(managed, body, { ...opts, lookupStatus: 503 });
          expect(result.updates).toHaveLength(1);
          expect(result.updates[0]).toMatchObject({ pr_node_id: NODE_ID, observed_updated_at: UPDATED_AT });
          // No binding was read, so there is no revision to compare against.
          expect("expected_revision" in (result.updates[0] as object)).toBe(false);
        }
        // Never on a payload that disagrees with the fresh read.
        expect(
          await run(managed, prEvent("closed"), { lookupStatus: 503, nodeId: "PR_other", prState: "closed" }),
        ).toEqual([]);
        // Enabling and head writes need a verified binding.
        for (const body of [
          prEvent("assigned", { assignee: { login: "centaur-bot" } }),
          prEvent("synchronize"),
        ]) {
          expect(await run(managed, body, { lookupStatus: 503 })).toEqual([]);
        }
      });
    });

    test("synchronize records the freshly fetched head, not the payload's", async () => {
      expect(
        await run(bound, prEvent("synchronize", {}, "payload9"), { heads: ["fresh22"] }),
      ).toEqual([{ pr_node_id: NODE_ID, ...seen, head_sha: "fresh22" }]);
      // No fresh head, no write.
      expect(
        await run(bound, prEvent("synchronize", {}, "payload9"), { prFetchFails: true }),
      ).toEqual([]);
    });

    test("an event for a different PR than the binding's writes nothing", async () => {
      for (const body of [
        prEvent("closed", {}, "old000", "PR_other"),
        prEvent("synchronize", {}, "old000", "PR_other"),
        prEvent("assigned", { assignee: { login: "centaur-bot" } }, "old000", "PR_other"),
      ]) {
        expect(await run(bound, body)).toEqual([]);
      }
      // The payload agrees with the binding but the fresh read does not.
      expect(await run(bound, prEvent("synchronize"), { nodeId: "PR_other" })).toEqual([]);
      // The fresh read alone proves identity when the payload has no node id,
      // and disproves it when it names another PR.
      const noNodeId = JSON.stringify({
        action: "closed",
        repository: { full_name: "base/repo" },
        pull_request: { number: 7 },
      });
      expect(await run(bound, noNodeId, { prState: "closed" })).toHaveLength(1);
      expect(await run(bound, noNodeId, { nodeId: "PR_other", prState: "closed" })).toEqual([]);
    });

    test("an unbound PR records nothing and keeps its existing handling", async () => {
      for (const body of [
        prEvent("assigned", { assignee: { login: "centaur-bot" } }),
        prEvent("unassigned", { assignee: { login: "centaur-bot" } }),
        prEvent("closed"),
        prEvent("reopened"),
        prEvent("synchronize"),
      ]) {
        expect(await run(null, body)).toEqual([]);
      }
    });

    test("assigning the bot to an unbound red PR still takes it over with a fix turn", async () => {
      const api = makeApi({ binding: null });
      const { ctx } = makeCtx({ api });
      await handlePullRequestEvent(ctx, prEvent("assigned", { assignee: { login: "centaur-bot" } }, "abc123"));
      await drainBackgroundWork(2000);
      expect(api.sessionCalls.length).toBeGreaterThan(0);
    });

    test("assigning the bot to a bound red PR reports it instead of running a turn", async () => {
      const api = makeApi({ binding: bound });
      const { ctx } = makeCtx({ api });
      await handlePullRequestEvent(ctx, prEvent("assigned", { assignee: { login: "centaur-bot" } }, "abc123"));
      await drainBackgroundWork(2000);
      expect(api.sessionCalls).toEqual([]);
      // The mock binding still reads unmanaged, so api-rs's own state decides;
      // githubbot never ran a turn and never touched the fix counter.
      expect(api.posts("/state")[0]?.body).toMatchObject({ managed: true });
    });

    test("a state-update failure does not break the existing handling", async () => {
      const api = makeApi({ binding: bound });
      const failing = {
        ...api,
        fetch: async (url: string | URL | Request, init?: RequestInit) =>
          String(url).endsWith("/state")
            ? new Response("no", { status: 500 })
            : api.fetch(url, init),
      };
      const { ctx } = makeCtx({ api: failing, settled: false });
      await expect(
        handlePullRequestEvent(ctx, prEvent("assigned", { assignee: { login: "centaur-bot" } })),
      ).resolves.toBeUndefined();
    });
  });
});

describe("getPrBinding", () => {
  const valid = {
    repository: "base/repo",
    number: 7,
    thread_key: "slack:T0:C0:1700000000.000100",
    pr_node_id: "PR_node_1",
    head_sha: "abc123",
    revision: 4,
    managed: true,
    closed: false,
  };

  function lookup(response: () => Response) {
    return getPrBinding(
      { apiUrl: "http://localhost", fetch: async () => response() } as never,
      { repository: "base/repo", number: 7 },
    );
  }

  test("returns a complete binding, ignoring extra fields", async () => {
    await expect(lookup(() => Response.json({ ...valid, author_login: "a" }))).resolves.toMatchObject(
      valid,
    );
  });

  test("only a coded 404 is unbound", async () => {
    await expect(lookup(() => bindingMiss())).resolves.toBeNull();
    // A generic 404 is not a binding miss: an older replica without the route
    // answers one while a newer replica has bound the PR.
    for (const generic of [
      () => new Response("nf", { status: 404 }),
      () => new Response("", { status: 404 }),
      () => Response.json({ error: "not found" }, { status: 404 }),
      () => Response.json({ code: "something_else" }, { status: 404 }),
      () => Response.json({ error: { code: "pr_binding_not_found" } }, { status: 404 }),
      () => Response.json(["pr_binding_not_found"], { status: 404 }),
    ]) {
      const error = await lookup(generic).catch((e: unknown) => e);
      expect((error as Error).message).toContain("get pr binding failed: 404");
      // Not retryable: nothing can be assumed from it.
      expect(isRetryableSessionApiError(error)).toBe(false);
    }
    for (const status of [400, 403, 500, 503]) {
      await expect(lookup(() => new Response("x", { status }))).rejects.toThrow();
    }
  });

  test("rejects every malformed field", async () => {
    const malformed: Record<string, unknown>[] = [
      { repository: undefined },
      { repository: "" },
      { repository: "norepo" },
      { repository: "a/b/c" },
      { repository: "a /b" },
      { repository: 7 },
      { number: undefined },
      { number: 0 },
      { number: -1 },
      { number: 1.5 },
      { number: "7" },
      { head_sha: undefined },
      { head_sha: "" },
      { head_sha: 1 },
      { thread_key: undefined },
      { thread_key: "" },
      { pr_node_id: undefined },
      { pr_node_id: "" },
      { revision: undefined },
      { revision: -1 },
      { revision: 1.5 },
      { revision: "4" },
      { managed: undefined },
      { managed: "yes" },
      { closed: undefined },
      { closed: 0 },
    ];
    for (const patch of malformed) {
      await expect(lookup(() => Response.json({ ...valid, ...patch }))).rejects.toThrow(
        "unexpected shape",
      );
    }
    await expect(lookup(() => Response.json(null))).rejects.toThrow("unexpected shape");
    await expect(lookup(() => Response.json([valid]))).rejects.toThrow("unexpected shape");
  });
});

describe("firstUsableUrl", () => {
  const usable = "https://github.com/base/repo/runs/9";

  test("keeps an HTTPS URL within the byte limit, and takes the first usable candidate", () => {
    expect(firstUsableUrl("https://ci.example.test/a?b=c#d", usable)).toBe(
      "https://ci.example.test/a?b=c#d",
    );
    expect(firstUsableUrl(null, undefined, "http://x.example", usable)).toBe(usable);
    expect(firstUsableUrl("http://x.example", "javascript:alert(1)")).toBeUndefined();
    expect(firstUsableUrl()).toBeUndefined();
  });

  test("counts the limit in UTF-8 bytes, as api-rs does", () => {
    const prefix = "https://x.example/";
    const atLimit = prefix + "a".repeat(2_048 - prefix.length);
    expect(firstUsableUrl(atLimit)).toBe(atLimit);
    expect(firstUsableUrl(atLimit + "a", usable)).toBe(usable);
    // Few characters, but over the limit in bytes.
    const wide = prefix + "é".repeat(1_100);
    expect(wide.length).toBeLessThan(2_048);
    expect(firstUsableUrl(wide, usable)).toBe(usable);
  });

  test("rejects whitespace, control characters and the characters that break a Slack link", () => {
    for (const bad of [
      "https://x.example/a b",
      "https://x.example/a\tb",
      "https://x.example/a\nb",
      "https://x.example/a\rb",
      "https://x.example/a\u0085b",
      "https://x.example/a\u00a0b",
      "https://x.example/a\u2028b",
      "https://x.example/a\u3000b",
      "https://x.example/a\u0000b",
      "https://x.example/a\u007fb",
      "https://x.example/a<b",
      "https://x.example/a>b",
      "https://x.example/a|b",
    ]) {
      expect(firstUsableUrl(bad, usable)).toBe(usable);
      expect(firstUsableUrl(bad)).toBeUndefined();
    }
  });
});
