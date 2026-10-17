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

// Pointer events use the broad LOCAL_CONTROL_SELECTORS list; keyboard events
// use this narrower list because a focused button must not swallow typing
// meant for the remote desktop.
export const LOCAL_KEYBOARD_SELECTORS = [
  "input:not([type='button']):not([type='submit']):not([type='reset']):not([type='checkbox']):not([type='radio']):not([type='range'])",
  "textarea",
  "select",
  "[contenteditable='true']",
  "[role='textbox']",
  "[role='searchbox']",
  "[role='combobox']",
  "[role='spinbutton']",
  "[data-local-keyboard]",
] as const;

const LOCAL_KEYBOARD_SELECTOR = LOCAL_KEYBOARD_SELECTORS.join(",");

export function isLocalKeyboardTarget(target: EventTarget | null): boolean {
  if (!target || typeof (target as Element).closest !== "function") return false;
  return Boolean((target as Element).closest(LOCAL_KEYBOARD_SELECTOR));
}
