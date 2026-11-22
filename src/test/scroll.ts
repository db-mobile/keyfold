// jsdom has no layout: an element is 0 px high, so a virtualised list
// would show no rows. A scroll box with a stated size makes the
// virtualizer hand out the rows that fit; tests of a list's model use it
// to read rows. Nothing is rendered or measured for real.

export function scrollBox<T extends HTMLElement = HTMLDivElement>(height = 600, width = 800): T {
  const el = document.createElement("div");
  Object.defineProperties(el, {
    offsetHeight: { value: height },
    offsetWidth: { value: width },
    clientHeight: { value: height },
    clientWidth: { value: width },
  });
  el.scrollTo = () => {};
  el.getBoundingClientRect = () => ({ x: 0, y: 0, top: 0, left: 0, bottom: height, right: width, width, height, toJSON: () => ({}) });
  return el as unknown as T;
}

/// Move a scroll box as the user would: the offset changes and the scroll
/// event fires. A list's model treats the scroll as settled 80 ms later.
export function scrollTo(el: HTMLElement, top: number): void {
  el.scrollTop = top;
  el.dispatchEvent(new Event("scroll"));
}
