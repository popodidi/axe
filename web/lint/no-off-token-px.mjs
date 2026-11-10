// Rejects px lengths other than 0 and the 1px hairline in declaration values
// and at-rule params (@media, @container, @value, ...). Values are tokenized
// with CSS number grammar (sign, decimals, exponents) and the unit is
// unescaped and case-folded, so 13PX, -13px, 1.3e1px and 13\70 x all count.
import stylelint from "stylelint";
import valueParser from "postcss-value-parser";
const ruleName = "axe/no-off-token-px";
const messages = stylelint.utils.ruleMessages(ruleName, {
  rejected: (value) => `Use a design token instead of "${value}"`,
});
function unescape(value) {
  return value
    .replace(/\\([0-9a-fA-F]{1,6})[ \t\n]?/g, (_, hex) =>
      String.fromCodePoint(parseInt(hex, 16)),
    )
    .replace(/\\(.)/g, "$1");
}
function isOffTokenPx(word) {
  const parsed = valueParser.unit(word);
  if (!parsed || parsed.unit.toLowerCase() !== "px") return false;
  return ![0, 1].includes(Math.abs(Number(parsed.number)));
}
// The one exception: `@value` breakpoint definitions in the files listed in
// the `breakpointFiles` option (path suffixes), because media and container
// queries cannot read custom properties.
const rule = (enabled, options) => (root, result) => {
  if (!enabled) return;
  const file = (root.source?.input.file || "").replaceAll("\\", "/");
  const breakpoints = (options?.breakpointFiles || []).some((f) =>
    file.endsWith("/" + f),
  );
  const check = (node, value) =>
    // Escapes resolve before tokenizing: "13\70 x" is the dimension 13px.
    valueParser(unescape(value)).walk((part) => {
      if (part.type === "function" && part.value.toLowerCase() === "url")
        return false;
      if (part.type === "word" && isOffTokenPx(part.value))
        stylelint.utils.report({
          ruleName,
          result,
          node,
          word: part.value,
          message: messages.rejected(part.value),
        });
    });
  root.walkDecls((decl) => check(decl, decl.value));
  root.walkAtRules((atRule) => {
    if (breakpoints && atRule.name.toLowerCase() === "value") return;
    check(atRule, atRule.params);
  });
};
rule.ruleName = ruleName;
rule.messages = messages;
export default stylelint.createPlugin(ruleName, rule);
