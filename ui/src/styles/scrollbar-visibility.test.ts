import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const css = readFileSync(fileURLToPath(new URL('./index.css', import.meta.url)), 'utf8').replace(
  /\/\*[\s\S]*?\*\//g,
  ''
);

describe('cross-platform scrollbar visibility', () => {
  it('hides persistent native tracks without disabling scrolling', () => {
    expect(css).toMatch(/\*\s*\{[^}]*scrollbar-width\s*:\s*none\s*;/);
    expect(css).toMatch(/\*::\-webkit-scrollbar\s*\{[^}]*display\s*:\s*none\s*;/);

    const universal = css.match(/\*\s*\{([^}]*)\}/)?.[1] ?? '';
    expect(universal).not.toMatch(/overflow(?:-[xy])?\s*:\s*hidden/);
  });
});
