export const LOCAL_CONTROL_SELECTORS = [
  "a",
  "button",
  "input",
  "label",
  "select",
  "textarea",
  "summary",
  "[contenteditable='true']",
  "[data-local-control]",
  "[role='button']",
  "[role='checkbox']",
  "[role='combobox']",
  "[role='link']",
  "[role='menuitem']",
  "[role='option']",
  "[role='radio']",
  "[role='slider']",
  "[role='switch']",
  "[role='tab']",
  "[role='textbox']",
] as const;

const LOCAL_CONTROL_SELECTOR = LOCAL_CONTROL_SELECTORS.join(",");

export function isLocalControlTarget(target: EventTarget | null): boolean {
  if (!target || typeof (target as Element).closest !== "function") return false;
  return Boolean((target as Element).closest(LOCAL_CONTROL_SELECTOR));
}
