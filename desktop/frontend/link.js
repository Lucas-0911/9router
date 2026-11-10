import { forwardRef } from "react";
import { navigate } from "./navigation";
export default forwardRef(function Link({ href, children, onClick, replace, scroll, prefetch, ...props }, ref) {
  const url = typeof href === "object" ? href.pathname + (href.query ? "?" + new URLSearchParams(href.query) : "") : href;
  return <a {...props} ref={ref} href={url} onClick={(event) => {
    onClick?.(event);
    if (event.defaultPrevented || event.button || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || props.target === "_blank" || props.download) return;
    event.preventDefault();
    navigate(url, { replace, scroll });
  }}>{children}</a>;
});
