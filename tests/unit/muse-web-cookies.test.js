import { describe, expect, it } from "vitest";
import { toMuseCookies } from "../../open-sse/executors/muse-web/session.js";

describe("muse.ai cookies for Playwright", () => {
  it("omits expires so Chrome accepts the batch", () => {
    const cookies = toMuseCookies("hatch_sess=abc; hatch_vml=def");
    expect(cookies.map((cookie) => cookie.name)).toEqual(["hatch_sess", "hatch_vml"]);
    for (const cookie of cookies) {
      expect(cookie).not.toHaveProperty("expires");
      expect(cookie.domain).toBe(".muse.ai");
    }
  });

  it("keeps __Host- cookies host-only", () => {
    const [cookie] = toMuseCookies("__Host-muse=1");
    expect(cookie.domain).toBe("muse.ai");
    expect(cookie).not.toHaveProperty("expires");
  });
});
