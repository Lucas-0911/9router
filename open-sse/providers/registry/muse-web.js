export default {
  id: "muse-web",
  priority: 220,
  // Distinct alias: "muse" belongs to the Muse API provider; this cookie/web
  // variant for muse.ai Hatch is addressed by its own id.
  alias: "muse-web",
  aliases: [
    "mu",
  ],
  uiAlias: "muse-web",
  display: {
    name: "Muse Web",
    icon: "auto_awesome",
    color: "#7C3AED",
    textIcon: "MU",
    website: "https://muse.ai",
  },
  category: "webCookie",
  authType: "cookie",
  // Windows 2.0.5 exposes muse-spark as chat plus image and video.
  serviceKinds: ["llm", "image", "video"],
  videoConfig: { executorDriven: true },
  authHint: "Paste muse.ai cookies. hatch_sess is required. hatch_vml, hatch_native_auth_device and hatch_gw are optional.",
  transport: {
    baseUrl: "https://muse.ai",
    format: "openai",
    authType: "cookie",
  },
  models: [
    { id: "muse-chat", name: "Muse Chat" },
    // Same Hatch chat path as muse-chat. Bare id used by MuseRouter clients.
    { id: "muse-spark", name: "Muse Spark" },
    { id: "muse-image", name: "Muse Image", params: ["n", "size"], kind: "image" },
    { id: "muse-video", name: "Muse Video", params: ["size", "seconds", "image"], kind: "video" },
  ],
};
