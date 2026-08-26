/**
 * JS fixture. Code-like text in comments: class NotReal {}
 */
import { helper } from './util.js';
import defaultThing, { one as uno } from '../shared/index.js';
const legacy = require('./legacy.cjs');

export const MAX_AGE = 30;

export class Widget extends BaseWidget {
  #secret = 'not-a-real-secret-token-abc';

  render(props) {
    return helper(props) + this.#secret.length;
  }
}

export function makeWidget(opts) {
  function choose() { return opts.id; }
  return new Widget(choose());
}

const arrowAdd = (a, b) => a + b;

async function load() {
  const mod = await import('./lazy.js');
  return mod.default;
}

module.exports = { makeWidget };
