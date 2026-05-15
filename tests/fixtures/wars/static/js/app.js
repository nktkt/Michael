// Trivial client-side hook for the `static/` fixture.
(function () {
  "use strict";
  if (typeof document === "undefined") {
    return;
  }
  document.addEventListener("DOMContentLoaded", function () {
    var heading = document.querySelector("h1");
    if (heading) {
      heading.setAttribute("data-hydrated", "true");
    }
  });
})();
