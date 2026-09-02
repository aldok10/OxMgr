(function () {
  try {
    var stored = localStorage.getItem("oxmgr-theme");
    if (stored === "light" || stored === "dark") {
      document.documentElement.dataset.theme = stored;
    }
  } catch (e) {}
})();
