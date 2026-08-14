// Theme is FOLLOWED from the OS, not chosen by the user. A manual
// dark/light choice in Settings overrides it (fs-theme). Read before
// first paint so there is no flash of the wrong scheme.
document.documentElement.setAttribute(
  "data-theme",
  (localStorage.getItem("fs-theme") || "system") === "system"
    ? matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark"
    : localStorage.getItem("fs-theme")
);