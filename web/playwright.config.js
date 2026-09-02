const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: './tests',
  globalSetup: require.resolve('./tests/global-setup.js'),
  globalTeardown: require.resolve('./tests/global-teardown.js'),
  use: {
    baseURL: 'http://127.0.0.1:46002',
    httpCredentials: {
      username: 'admin',
      password: 'changeme',
    },
  },
});
