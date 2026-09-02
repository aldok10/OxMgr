// Cluster demo: one HTTP server per OxMgr-managed instance.
//
// OxMgr expands `cluster_mode` + `cluster_instances` into one process per
// instance and injects NODE_APP_INSTANCE (0-based) into each. No PORT env var
// is injected, so the server derives its port from the instance index to keep
// the instances from colliding on one listener.
const http = require("http");

const idx = Number(process.env.NODE_APP_INSTANCE || 0);
const port = 5500 + idx;

http
  .createServer((req, res) => {
    res.writeHead(200, { "Content-Type": "text/plain" });
    res.end(`api-${idx} on :${port}\n`);
  })
  .listen(port, "0.0.0.0", () => {
    console.log(`api-${idx} listening on :${port}`);
  });