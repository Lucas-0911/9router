try {
  const state = JSON.parse(localStorage.getItem("theme") || "null");
  const theme = state?.state?.theme || "system";
  if (theme === "dark" || (theme === "system" && matchMedia("(prefers-color-scheme: dark)").matches)) document.documentElement.classList.add("dark");
} catch {}
const fontsLoaded = () => document.documentElement.classList.add("fonts-loaded");
if (document.fonts?.load) document.fonts.load('24px "Material Symbols Outlined"').then(fontsLoaded).catch(fontsLoaded);
else fontsLoaded();
setTimeout(fontsLoaded, 3000);
