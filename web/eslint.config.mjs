import { defineConfig, globalIgnores } from "eslint/config";
import next from "eslint-config-next/core-web-vitals";
import ts from "eslint-config-next/typescript";
const literal =
  "/#[0-9a-f]{3,8}\\b|\\b(rgba?|hsla?)\\(|\\d(e[+-]?\\d+)?px\\b/i";
const message =
  "Style with CSS modules and design tokens (app/styles), not literals.";
export default defineConfig([
  ...next,
  ...ts,
  {
    rules: {
      "@typescript-eslint/no-unused-vars": [
        "warn",
        { ignoreRestSiblings: true },
      ],
    },
  },
  {
    files: ["app/**/*.tsx", "components/**/*.tsx"],
    rules: {
      "no-restricted-syntax": [
        "error",
        { selector: `Literal[value=${literal}]`, message },
        { selector: `TemplateElement[value.raw=${literal}]`, message },
        { selector: `JSXText[value=${literal}]`, message },
        { selector: "JSXAttribute[name.name='style']", message },
      ],
    },
  },
  {
    files: ["**/*.test.tsx"],
    rules: { "no-restricted-syntax": "off" },
  },
  globalIgnores([
    "out/**",
    ".next/**",
    "next-env.d.ts",
    ".playwright/**",
    "test-results/**",
    "playwright-report/**",
  ]),
]);
