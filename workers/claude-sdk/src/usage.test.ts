import {test} from 'node:test';
import assert from 'node:assert/strict';
import type {SDKControlGetUsageResponse} from '@anthropic-ai/claude-agent-sdk';
import {quotaObservation} from './usage.js';
const report = (rate_limits: object | null, extra = {}) => ({rate_limits_available:true, subscription_type:'max',rate_limits,...extra}) as SDKControlGetUsageResponse;
test('native usage preserves percentages, buckets and reset times without session-cost inference',()=>{
 const result=quotaObservation(report({five_hour:{utilization:42,resets_at:'2026-10-29T20:00:00Z'},seven_day:{utilization:9,resets_at:null},seven_day_opus:{utilization:null,resets_at:null},model_scoped:[{display_name:'Model A',utilization:100,resets_at:null}]}));
 assert.equal(result.state,'available');assert.equal(result.plan,'max');assert.equal(result.windows[0].usedPercent,42);assert.equal(result.windows[0].resetAt,Date.parse('2026-10-29T20:00:00Z')/1000);assert.equal(result.windows.length,3);
 assert.equal(quotaObservation(report({five_hour:{utilization:100,resets_at:null}})).state,'exhausted');
 assert.equal(quotaObservation(report({five_hour:{utilization:100,resets_at:null},extra_usage:{is_enabled:true}})).state,'available');
 for(const value of [null,{}, {five_hour:{utilization:null}}, {five_hour:{utilization:-1}}]) assert.throws(()=>quotaObservation(report(value)));
 assert.throws(()=>quotaObservation(report({five_hour:{utilization:0}},{rate_limits_available:false})));
});

test('native account email is separate from subscription plan and missing identity stays absent', () => {
 const usage = report({five_hour:{utilization:42,resets_at:null}});
 assert.equal(quotaObservation(usage, {email:'claude-fixture@example.invalid'}).email, 'claude-fixture@example.invalid');
 assert.equal(quotaObservation(usage).email, '');
 assert.equal(quotaObservation(usage, {email:'  claude-fixture@example.invalid  '}).email, 'claude-fixture@example.invalid');
});
