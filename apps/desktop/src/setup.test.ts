import { describe, expect, it } from "vitest";
import {
  canCompleteSetup,
  mergeRepositoryScopes,
  type DesktopSetupInput,
  type DesktopSetupStatus,
} from "./setup";

const status: DesktopSetupStatus = {
  completed: false,
  github: {
    id: "github",
    label: "GitHub",
    available: true,
    detail: "Authenticated",
  },
  agents: [
    {
      id: "codex",
      label: "Codex",
      available: true,
      detail: "codex 1.0",
    },
  ],
  selected_repositories: [],
  default_agent: null,
  analytics_enabled: false,
  crash_reports_enabled: false,
};

const input: DesktopSetupInput = {
  repositories: ["github:owner/repo"],
  default_agent: "codex",
  analytics_enabled: false,
  crash_reports_enabled: false,
};

describe("desktop setup", () => {
  it("requires authenticated GitHub, an installed default agent, and a repo", () => {
    expect(canCompleteSetup(status, input)).toBe(true);
    expect(
      canCompleteSetup(
        { ...status, github: { ...status.github, available: false } },
        input,
      ),
    ).toBe(false);
    expect(canCompleteSetup(status, { ...input, repositories: [] })).toBe(false);
    expect(
      canCompleteSetup(status, { ...input, default_agent: "claude" }),
    ).toBe(false);
  });

  it("merges repositories from multiple organizations without duplicates", () => {
    expect(
      mergeRepositoryScopes(
        [{ id: "github:zeta/two", label: "zeta/two", parent: "github:zeta" }],
        [
          {
            id: "github:acme/one",
            label: "acme/one",
            parent: "github:acme",
          },
          {
            id: "github:zeta/two",
            label: "zeta/two",
            parent: "github:zeta",
          },
        ],
      ).map((scope) => scope.id),
    ).toEqual(["github:acme/one", "github:zeta/two"]);
  });
});
