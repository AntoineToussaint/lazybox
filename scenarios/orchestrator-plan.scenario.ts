import { scenario, caption, say, press, click, expect, highlight, wait, conversation, button, dialog, region, role, css } from './lib/dsl.ts';

export default scenario({
  title: 'An agent plans the work and runs a team of agents',
  story: { as: 'a lead with a feature too big for one agent', want: 'an orchestrator to propose a split I approve, then run the agents', soThat: 'the work goes in parallel and nothing starts without my say' },
  tags: ['orchestration', 'missions', 'sub-agents'],
  viewport: 'desktop',
  theme: 'dark',
  build(w) {
    const cache = w.workspace('Response cache', { repos: ['acme/api', 'acme/web'], branch: 'feature/response-cache' });
    const lead = cache.conversation('Cache lead', { createdMinutesAgo: 12 });
    lead.granted({ title: 'Ship the response cache', brief: 'Cache GET responses for five minutes; measure /products.' }, { maxChildren: 3, planRequired: true });
    w.open(lead);
    const planLine = css('.lineage-line.kind-plan_proposed', 'the proposed plan');
    const sheet = dialog('Plan for Ship the response cache');
    const add = conversation('Add the cache'), bench = conversation('Benchmark /products'), docs = conversation('Document the cache settings');
    return [
      caption('A feature too big for one agent: ask for a plan first'),
      say('Ship the response cache. Split it between agents, and show me the plan before starting anyone.'),
      press('Enter'),
      lead.receives('message', 'Split it between agents'),
      lead.thinks(1.2),
      lead.reads('api/src/cache.ts', 'api/src/routes.ts', 'web/src/api-client.ts'),
      lead.writes('Three agents: one adds the cache, one benchmarks `/products` once it lands, one documents the settings. Here is the plan.'),
      lead.proposesPlan({ summary: 'the cache first, then the benchmark; docs alongside', tasks: [
        { key: 'cache', title: 'Add the cache', brief: 'Objective: expiry and a size limit in createCache\nDone when: npm test passes', role: 'worker', paths: ['api/src/cache.ts', 'api/README.md'], estimateUsd: 4 },
        { key: 'bench', title: 'Benchmark /products', role: 'researcher', permission: 'read-only', paths: ['api/bench/'], estimateUsd: 2.5, after: ['cache'] },
        { key: 'docs', title: 'Document the cache settings', role: 'writer', paths: ['api/README.md', 'docs/cache.md'], estimateUsd: 1.5 },
      ] }),
      expect.text(planLine, 'Proposed a plan: 3 agents'),
      caption('Nothing starts until you approve it'),
      click(button('Review plan…').in(planLine)),
      expect.visible(sheet),
      expect.count(css('.plan-tasks > li').in(sheet), 3),
      caption('Every task, its order, the files two tasks share, the estimate'),
      highlight(css('.plan-tasks').in(sheet), { label: 'In order', hold: 1.8 }),
      highlight(css('.plan-overlaps').in(sheet), { label: 'Shared files', hold: 1.6 }),
      caption('Approve it, with a budget cap'),
      click(role('checkbox', 'Set a budget cap').in(sheet)),
      click(role('button', /^Approve with \$\d+ cap$/).in(sheet)),
      expect.hidden(sheet),
      expect.call('UpdateMission', body => !!body.approvePlan),
      expect.text(css('.lineage-line.kind-plan_decided'), 'You approved the plan: 3 tasks'),
      caption('The orchestrator starts its agents, each in its own conversation'),
      lead.spawns({ title: 'Add the cache', brief: 'Objective: expiry and a size limit in createCache\nDone when: npm test passes', role: 'worker' }),
      lead.spawns({ title: 'Document the cache settings', brief: 'Objective: one section per setting', role: 'writer' }),
      expect.text(css('.panel-agents'), 'Add the cache'),
      expect.text(css('.panel-agents'), 'Document the cache settings'),
      add.thinks(0.8),
      add.edits('api/src/cache.ts', { from: 'const ttl = 60;', to: 'const ttl = 300;\nconst max = 1000;' }),
      add.runs('npm test -- cache', { output: 'Tests  6 passed (6)\n', seconds: 1.2 }),
      add.reports('Expiry and eviction in createCache; six tests pass.'),
      lead.spawns({ title: 'Benchmark /products', brief: 'Objective: p95 before and after\nDone when: numbers in the PR', role: 'researcher', permission: 'read-only' }),
      bench.thinks(0.6),
      docs.writes('`CACHE_TTL_SECONDS` and `CACHE_MAX_ENTRIES` documented, with their defaults.'),
      caption('One board for the mission: who does what, what is done, what needs you'),
      press('Mod+K'),
      say('missions board'),
      press('Enter'),
      expect.visible(region('Missions and tasks')),
      expect.count(css('.mission-card').in(region('Missions and tasks')), 3),
      wait(1.5),
      highlight(css('.mission-card').in(region('Missions and tasks')).first(), { label: 'Reported done: yours to accept', hold: 2 }),
      caption('A team of agents, on a plan you approved', { hold: 3 }),
    ];
  },
});
