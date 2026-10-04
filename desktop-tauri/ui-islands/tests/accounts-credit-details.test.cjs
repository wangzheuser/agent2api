'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '../../..');
const ts = require(path.join(root, 'desktop-tauri/ui-islands/node_modules/typescript'));
const source = fs.readFileSync(path.join(root, 'desktop-tauri/ui-islands/src/islands/accounts-credit-details.ts'), 'utf8');
const compiled = ts.transpileModule(source, {compilerOptions: {module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022}}).outputText;
const context = {exports: {}};
vm.runInNewContext(compiled, context);
const {creditDetailsOf, genericBalanceDetailsOf, formatCreditAmount} = context.exports;
const now = 1791000000000;
const segment = {id: 'resource-a:cycle-a', resourceId: 'resource-a', packageCode: 'monthly', name: '月度额度',
  remaining: 0.75, total: 500, expiresAt: now + 3600000, expiresAtText: null, expiryStatus: 'known', entitlementEndsAt: now + 31536000000, state: 'active'};
const details = {version: 1, kind: 'personal', fetchedAt: now, complete: true, remaining: 1.5, unlimited: false,
  unattributedRemaining: 0, issues: [], segments: [segment, {...segment, id: 'resource-b:cycle-a', resourceId: 'resource-b'}]};
const wrap = value => ({creditDetails: value});
let count = 0;
function check(name, test) {test(); count += 1; console.log(`PASS ${name}`);}
check('legacy usage has no credit details', () => assert.equal(creditDetailsOf({totalLeft: 1}), null));
check('generic wallet keeps balance, unit and subscription expiry', () => {
  const result = genericBalanceDetailsOf({available: '12.5', unit: 'credit', wallets: [{type: 'main', balance: 12.5, total: 20}], subscription: {expireAt: '2026-10-10T00:00:00Z'}}, now)
  assert.equal(result.available, 12.5); assert.equal(result.unit, 'credit'); assert.equal(result.wallets[0].total, 20); assert.equal(result.subscription.expireAt, Date.parse('2026-10-10T00:00:00Z'))
});
check('failure does not pass as successful details', () => assert.equal(creditDetailsOf({...wrap(details), error: 'failed'}), null));
check('precise fractions and independent resources preserved', () => {const result = creditDetailsOf(wrap(details)); assert.equal(result.remaining, 1.5); assert.equal(result.segments.length, 2); assert.notEqual(result.segments[0].resourceId, result.segments[1].resourceId);});
check('valid zero is preserved', () => assert.equal(creditDetailsOf(wrap({...details, remaining: 0, segments: [{...segment, remaining: 0, state: 'exhausted'}]})).remaining, 0));
check('unknown total remains null', () => assert.equal(creditDetailsOf(wrap({...details, segments: [{...segment, total: null}]})).segments[0].total, null));
check('unknown expiry remains distinct from never', () => assert.equal(creditDetailsOf(wrap({...details, segments: [{...segment, expiresAt: null, expiryStatus: 'unknown'}]})).segments[0].expiryStatus, 'unknown'));
check('unverified timezone keeps only text', () => {const result = creditDetailsOf(wrap({...details, segments: [{...segment, expiresAt: null, expiresAtText: '2026-10-31 23:59:59', expiryStatus: 'timezone_unverified'}]})); assert.equal(result.segments[0].expiresAt, null); assert.equal(result.segments[0].expiresAtText, '2026-10-31 23:59:59');});
check('enterprise unlimited needs no numeric amount', () => assert.equal(creditDetailsOf(wrap({...details, kind: 'enterprise', unlimited: true, remaining: null, segments: [{...segment, remaining: null, total: null}]})).unlimited, true));
check('partial query keeps unknown aggregate', () => assert.equal(creditDetailsOf(wrap({...details, remaining: null, complete: false, issues: ['truncated']})).remaining, null));
check('mismatch is preserved for explicit UI handling', () => assert.equal(creditDetailsOf(wrap({...details, complete: false, issues: ['balance_mismatch']})).complete, false));
check('expired cached data remains identifiable', () => assert.equal(creditDetailsOf(wrap({...details, segments: [{...segment, expiresAt: now - 1, state: 'active'}]})).segments[0].expiresAt, now - 1));
for (const [name, change] of [['unknown version', {version: 2}], ['nonfinite amount', {remaining: Infinity}], ['negative amount', {remaining: -1}], ['missing fetched time', {fetchedAt: undefined}], ['invalid fetched time', {fetchedAt: Infinity}], ['invalid issues', {issues: ['truncated', 1]}], ['missing segments', {segments: undefined}]]) {
  check(`reject ${name}`, () => assert.equal(creditDetailsOf(wrap({...details, ...change})), null));
}
check('reject duplicate segment identities', () => assert.equal(creditDetailsOf(wrap({...details, segments: [segment, {...segment}]})), null));
check('reject known date without absolute timestamp', () => assert.equal(creditDetailsOf(wrap({...details, segments: [{...segment, expiresAt: null}]})), null));
check('reject out-of-range Date timestamp', () => assert.equal(creditDetailsOf(wrap({...details, segments: [{...segment, expiresAt: 8640000000000001}]})), null));
check('reject NaN segment total', () => assert.equal(creditDetailsOf(wrap({...details, segments: [{...segment, total: NaN}]})), null));
check('amount formatter retains fractional precision and zero', () => {assert.match(formatCreditAmount(0), /^0[.,]00$/); assert.match(formatCreditAmount(1.5), /^1[.,]50$/); assert.match(formatCreditAmount(0.12345678), /12345678$/); assert.equal(formatCreditAmount(null), '未知');});
console.log(`${count} credit detail UI checks passed`);
