// ponytail: registry fingerprints are display-only here; Rust builds upstream headers.
export const platform = () => /Mac/.test(navigator.platform) ? "darwin" : /Win/.test(navigator.platform) ? "win32" : "linux";
export const arch = () => navigator.userAgentData?.architecture || "unknown";
