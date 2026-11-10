// Design-system enforcement: colors and lengths come from tokens.
// Only app/styles/brand-tokens.css (the copy of docs/quiet-design-tokens.css) holds
// literal values; app tokens and components reference var(--...) only.
import offTokenPx from "./lint/no-off-token-px.mjs";
const config = {
  plugins: [offTokenPx],
  ignoreFiles: [
    "app/styles/brand-tokens.css",
    "app/styles/accent-tokens.css",
    "out/**",
    ".next/**",
    ".playwright/**",
    "test-results/**",
    "playwright-report/**",
  ],
  rules: {
    "color-no-hex": true,
    "color-named": "never",
    "function-disallowed-list": [
      "rgb",
      "rgba",
      "hsl",
      "hsla",
      "hwb",
      "lab",
      "lch",
      "oklab",
      "oklch",
      "color",
      "color-mix",
    ],
    "harvis/no-off-token-px": [
      true,
      { breakpointFiles: ["app/styles/breakpoints.module.css"] },
    ],
  },
};
export default config;
