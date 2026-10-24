import { defineConfig } from 'vitest/config';
import { fileURLToPath } from 'node:url';
import { WASM_RUNTIME_FLAGS } from '../../src/extraction/wasm-runtime-flags';

/**
 * The engine suite with `capture-setup.ts` added, for `capture-fixtures.mjs`. Only the engine
 * tests run (no global dist-check setup: the capture never needs a rebuilt CLI).
 */
export default defineConfig({
  root: fileURLToPath(new URL('../..', import.meta.url)),
  test: {
    globals: true,
    environment: 'node',
    include: ['__tests__/**/*.test.ts'],
    exclude: ['**/node_modules/**', '**/dist/**', '__tests__/ui-package.test.ts'],
    globalSetup: ['./__tests__/global-setup-home-sandbox.ts'],
    setupFiles: ['./__tests__/setup-home-sandbox.ts', './scripts/rust-golden/capture-setup.ts'],
    env: { CODEGRAPH_ALLOW_UNSAFE_NODE: '1', CODEGRAPH_TELEMETRY: '0' },
    poolOptions: { forks: { execArgv: [...WASM_RUNTIME_FLAGS] } },
    testTimeout: 60000,
  },
});
