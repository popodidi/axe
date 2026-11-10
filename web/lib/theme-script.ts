// Dark is the default. A stored "light" choice is applied before first paint
// by themeScript in app/layout.tsx; useTheme (lib/theme.ts) toggles it.
export const themeKey = "axe-theme";
export const themeScript = `try{if(localStorage.getItem("${themeKey}")==="light")document.documentElement.dataset.theme="light"}catch(e){}`;
