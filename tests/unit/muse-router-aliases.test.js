import { describe, expect, it } from "vitest";

import { getModelInfoCore } from "../../open-sse/services/model.js";
import { getProviderModels } from "../../open-sse/config/providerModels.js";

describe("MuseRouter bare model ids", () => {
  it("routes muse-spark to the Hatch chat model on muse-web", async () => {
    await expect(getModelInfoCore("muse-spark", {})).resolves.toEqual({
      provider: "muse-web",
      model: "muse-spark",
    });
  });

  it("routes bare image and video ids to muse-web", async () => {
    await expect(getModelInfoCore("muse-image", {})).resolves.toEqual({
      provider: "muse-web",
      model: "muse-image",
    });
    await expect(getModelInfoCore("muse-video", {})).resolves.toEqual({
      provider: "muse-web",
      model: "muse-video",
    });
  });

  it("registers muse-spark next to the existing Hatch models", () => {
    const ids = getProviderModels("muse-web").map((model) => model.id);
    expect(ids).toEqual(
      expect.arrayContaining(["muse-chat", "muse-spark", "muse-image", "muse-video"]),
    );
  });
});
