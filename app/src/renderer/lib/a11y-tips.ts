// Tooltips are CSS-only (`[data-tip]::after`), which screen readers don't announce.
// Mirror each data-tip into the accessibility tree:
//   - an element with no name of its own (icon button, bare span) gets it as aria-label;
//   - an element that already has a name (visible text or aria-label) keeps that name and
//     gets the tip as aria-description, so the visible label is not replaced.
// Attributes we set are marked so a later tip change updates them instead of stacking.

const MARK = "data-tip-a11y";

function apply(el: Element) {
  const tip = el.getAttribute("data-tip");
  const mine = el.getAttribute(MARK);
  if (!tip) {
    if (mine) el.removeAttribute(mine);
    el.removeAttribute(MARK);
    return;
  }
  if (mine) {
    if (el.getAttribute(mine) !== tip) el.setAttribute(mine, tip);
    return;
  }
  const hasLabel = el.hasAttribute("aria-label") || el.hasAttribute("aria-labelledby");
  const hasText = (el.textContent ?? "").trim().length > 0;
  const attr = hasLabel || hasText ? "aria-description" : "aria-label";
  if (attr === "aria-description" && el.getAttribute("aria-label") === tip) return;
  el.setAttribute(attr, tip);
  el.setAttribute(MARK, attr);
}

export function installTipA11y(root: Element = document.body) {
  root.querySelectorAll("[data-tip]").forEach(apply);
  const mo = new MutationObserver((records) => {
    for (const r of records) {
      if (r.type === "attributes") apply(r.target as Element);
      else
        r.addedNodes.forEach((n) => {
          if (n.nodeType !== 1) return;
          const e = n as Element;
          if (e.hasAttribute("data-tip")) apply(e);
          e.querySelectorAll("[data-tip]").forEach(apply);
        });
    }
  });
  mo.observe(root, { subtree: true, childList: true, attributes: true, attributeFilter: ["data-tip"] });
  return () => mo.disconnect();
}
