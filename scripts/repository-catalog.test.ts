import test from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { RepositoryCatalogSchema } from "@lazybox/protocol";
import type { RunnerClient } from "@lazybox/client-core";
import { repositoryCatalog } from "../packages/surfaces/src/repository-catalog.ts";
test("catalog readers share in-flight discovery and retain results after failure", async () => {
  let calls = 0,
    fail = false,
    release: () => void = () => {};
  const wait = new Promise<void>((resolve) => {
    release = resolve;
  });
  const api = {
    getRepositoryCatalog: async () => {
      calls++;
      await wait;
      if (fail) throw Error("offline");
      return create(RepositoryCatalogSchema, {
        entries: [{ identity: "github:org/repo", name: "org/repo" }],
        refreshSeconds: 0,
      });
    },
  } as unknown as RunnerClient;
  const a = repositoryCatalog(api),
    b = repositoryCatalog(api);
  assert.equal(a, b);
  const first = a.load(),
    second = b.load();
  release();
  await Promise.all([first, second]);
  assert.equal(calls, 1);
  assert.equal(a.getSnapshot().catalog?.entries[0].identity, "github:org/repo");
  fail = true;
  await a.load(true);
  assert.match(a.getSnapshot().error, /offline/);
  assert.equal(a.getSnapshot().catalog?.entries.length, 1);
  assert.equal(a.getSnapshot().refreshing, false);
});
