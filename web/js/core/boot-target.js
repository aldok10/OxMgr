/// Boot target resolution — extracted from dashboard.js so the boot file
/// stops being a shared library. This leaf is imported by the two modals
/// that need the resolved target, and by dashboard.js itself.
export const logPageTarget = () => {
const match = location.pathname.match(/^\/logs\/([^/]+)\/?$/);
if (!match) return null;
const params = new URLSearchParams(location.search);
return {
  name: decodeURIComponent(match[1]),
  stream: params.get("stream") ?? "stdout",
  index: parseInt(params.get("index") ?? "0", 10),
};
};

export const target = logPageTarget();