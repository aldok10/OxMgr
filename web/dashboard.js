console.log("dashboard.js loaded");
import { sel } from "./js/core/const.js";
import { target } from "./js/core/boot-target.js";
import { App } from "./js/shell/app.js";
import { LogPage } from "./js/logpage/logpage.js";

/// Boot module: decides from the URL whether this load is a process-list
if (target) {
  new LogPage(target.name, target.stream, target.index).init();
} else {
  sel("#addr").textContent = `http://${location.host}`;
  window.app = new App();
  window.app.init();
  console.log("App initialized", window.app);
}
