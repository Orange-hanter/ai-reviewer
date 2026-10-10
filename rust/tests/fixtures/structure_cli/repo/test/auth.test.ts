import { test } from 'node:test';
test('reject invalid token', () => {
  expect(() => validate('bad')).toThrow();
});
test('accepts a valid token', () => {
  assert.equal(validate('good'), true);
});
