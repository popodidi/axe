// Saved layout: navigation rail collapsed state and pane widths (CSS px) for
// the nav rail, the item pane (routine, service, bot, goal), the task detail
// pane and the run page's details rail. layoutScript in
// app/layout.tsx applies it before first paint, like the theme; lib/layout.ts
// reads and updates it.
export const layoutKey = "axe-layout";
export const detailSizeVar = "--detail-size";
export const paneVars = {
  detail: "--detail-size",
  item: "--item-size",
  nav: "--nav-size",
  run: "--run-details-size",
} as const;
export type Pane = keyof typeof paneVars;
export const layoutScript = `try{var l=JSON.parse(localStorage.getItem("${layoutKey}")||"{}"),d=document.documentElement,v=${JSON.stringify(paneVars)};if(l.nav==="collapsed")d.dataset.nav="collapsed";for(var k in v){var w=k==="nav"?l.navWidth:l[k];if(w>0)d.style.setProperty(v[k],w+"px")}}catch(e){}`;
